use std::{
    collections::HashMap,
    env,
    net::{SocketAddr, ToSocketAddrs},
    sync::{Arc, Mutex},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Query, State,
    },
    http::{
        header::{
            ACCESS_CONTROL_ALLOW_HEADERS, ACCESS_CONTROL_ALLOW_METHODS,
            ACCESS_CONTROL_ALLOW_ORIGIN, CONTENT_TYPE, ORIGIN,
        },
        HeaderMap, HeaderValue, StatusCode,
    },
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{mpsc, oneshot},
    time::timeout,
};
use uuid::Uuid;
use valence::prelude::Resource;

pub const BRIDGE_ADDR_ENV: &str = "WORLD_LOOM_BRIDGE_ADDR";
pub const BRIDGE_ALLOWED_ORIGINS_ENV: &str = "WORLD_LOOM_ALLOWED_ORIGINS";
pub const DEFAULT_BRIDGE_ADDR: &str = "127.0.0.1:18081";

const API_ROOT: &str = "/api/vm/net";
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_LOCAL_ORIGINS: &[&str] = &["http://localhost:3000", "http://127.0.0.1:3000"];

#[derive(Debug)]
pub enum BridgeError {
    InvalidAddress(String),
    Bind(std::io::Error),
    Runtime(std::io::Error),
    Startup(String),
}

impl std::fmt::Display for BridgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BridgeError::InvalidAddress(address) => write!(f, "invalid bridge address: {address}"),
            BridgeError::Bind(error) => write!(f, "failed to bind browser bridge: {error}"),
            BridgeError::Runtime(error) => write!(f, "failed to start bridge runtime: {error}"),
            BridgeError::Startup(message) => write!(f, "failed to start browser bridge: {message}"),
        }
    }
}

impl std::error::Error for BridgeError {}

pub struct BridgeRuntime {
    addr: SocketAddr,
    shutdown_tx: Mutex<Option<oneshot::Sender<()>>>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl Resource for BridgeRuntime {}

impl BridgeRuntime {
    pub fn start_default() -> Result<Self, BridgeError> {
        let requested_addr =
            env::var(BRIDGE_ADDR_ENV).unwrap_or_else(|_| DEFAULT_BRIDGE_ADDR.into());
        let addr = resolve_addr(&requested_addr)
            .ok_or_else(|| BridgeError::InvalidAddress(requested_addr.clone()))?;
        let allowed_origins = AllowedOrigins::from_env();
        Self::start(addr, allowed_origins)
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    fn start(addr: SocketAddr, allowed_origins: AllowedOrigins) -> Result<Self, BridgeError> {
        let listener = std::net::TcpListener::bind(addr).map_err(BridgeError::Bind)?;
        listener.set_nonblocking(true).map_err(BridgeError::Bind)?;
        let bound_addr = listener.local_addr().map_err(BridgeError::Bind)?;
        let (startup_tx, startup_rx) = std::sync::mpsc::channel();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();

        let thread = thread::Builder::new()
            .name("world-loom-browser-bridge".into())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = startup_tx.send(Err(format!("runtime creation failed: {error}")));
                        return;
                    }
                };

                runtime.block_on(async move {
                    let listener = match TcpListener::from_std(listener) {
                        Ok(listener) => listener,
                        Err(error) => {
                            let _ = startup_tx
                                .send(Err(format!("listener conversion failed: {error}")));
                            return;
                        }
                    };
                    let state = BridgeState::new(allowed_origins);
                    let app = Router::new()
                        .route(
                            &format!("{API_ROOT}/connect"),
                            get(connect_status)
                                .post(connect_tcp)
                                .options(connect_options),
                        )
                        .route(&format!("{API_ROOT}/socket"), get(socket_ws))
                        .route(&format!("{API_ROOT}/ping"), get(ping_ws))
                        .with_state(state);

                    let server = axum::serve(listener, app).with_graceful_shutdown(async move {
                        let _ = shutdown_rx.await;
                    });

                    let _ = startup_tx.send(Ok(()));
                    if let Err(error) = server.await {
                        eprintln!("World Loom browser bridge stopped with error: {error}");
                    }
                });
            })
            .map_err(BridgeError::Runtime)?;

        match startup_rx.recv_timeout(Duration::from_secs(2)) {
            Ok(Ok(())) => Ok(Self {
                addr: bound_addr,
                shutdown_tx: Mutex::new(Some(shutdown_tx)),
                thread: Mutex::new(Some(thread)),
            }),
            Ok(Err(message)) => Err(BridgeError::Startup(message)),
            Err(error) => Err(BridgeError::Startup(format!(
                "startup confirmation timed out: {error}"
            ))),
        }
    }
}

impl Drop for BridgeRuntime {
    fn drop(&mut self) {
        if let Some(shutdown_tx) = self
            .shutdown_tx
            .lock()
            .expect("bridge shutdown mutex")
            .take()
        {
            let _ = shutdown_tx.send(());
        }
        if let Some(thread) = self.thread.lock().expect("bridge thread mutex").take() {
            let _ = thread.join();
        }
    }
}

#[derive(Clone)]
struct BridgeState {
    pending_connections: Arc<Mutex<HashMap<String, TcpStream>>>,
    allowed_origins: Arc<AllowedOrigins>,
    connect_timeout: Duration,
}

impl BridgeState {
    fn new(allowed_origins: AllowedOrigins) -> Self {
        Self {
            pending_connections: Arc::new(Mutex::new(HashMap::new())),
            allowed_origins: Arc::new(allowed_origins),
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
        }
    }

    fn is_origin_allowed(&self, headers: &HeaderMap) -> bool {
        let Some(origin) = headers.get(ORIGIN).and_then(|value| value.to_str().ok()) else {
            return true;
        };
        self.allowed_origins.is_allowed(origin)
    }

    fn cors_origin(&self, headers: &HeaderMap) -> HeaderValue {
        if self.allowed_origins.is_any() {
            return HeaderValue::from_static("*");
        }

        headers
            .get(ORIGIN)
            .filter(|origin| {
                origin
                    .to_str()
                    .map(|origin| self.allowed_origins.is_allowed(origin))
                    .unwrap_or(false)
            })
            .cloned()
            .unwrap_or_else(|| HeaderValue::from_static("null"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AllowedOrigins {
    Any,
    List(Vec<String>),
}

impl AllowedOrigins {
    fn from_env() -> Self {
        let Ok(raw) = env::var(BRIDGE_ALLOWED_ORIGINS_ENV) else {
            return Self::List(
                DEFAULT_LOCAL_ORIGINS
                    .iter()
                    .map(|origin| origin.to_string())
                    .collect(),
            );
        };

        if raw.trim() == "*" {
            return Self::Any;
        }

        let origins = raw
            .split(',')
            .map(str::trim)
            .filter(|origin| !origin.is_empty())
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>();

        if origins.is_empty() {
            Self::List(
                DEFAULT_LOCAL_ORIGINS
                    .iter()
                    .map(|origin| origin.to_string())
                    .collect(),
            )
        } else {
            Self::List(origins)
        }
    }

    fn is_any(&self) -> bool {
        matches!(self, Self::Any)
    }

    fn is_allowed(&self, origin: &str) -> bool {
        match self {
            Self::Any => true,
            Self::List(origins) => origins.iter().any(|allowed| allowed == origin),
        }
    }
}

#[derive(Deserialize)]
struct ConnectRequest {
    host: String,
    port: u16,
}

#[derive(Serialize)]
struct ConnectStatus {
    code: u16,
    description: &'static str,
    time: u128,
    #[serde(rename = "processingTime")]
    processing_time: u128,
}

#[derive(Serialize)]
struct ConnectResponse {
    token: String,
    remote: RemoteInfo,
}

#[derive(Serialize)]
struct RemoteInfo {
    address: String,
    family: &'static str,
    port: u16,
}

#[derive(Serialize)]
struct ErrorResponse {
    code: u16,
    error: String,
}

async fn connect_status(State(state): State<BridgeState>, headers: HeaderMap) -> Response {
    if !state.is_origin_allowed(&headers) {
        return StatusCode::FORBIDDEN.into_response();
    }

    let started = Instant::now();
    with_cors(
        headers,
        &state,
        Json(ConnectStatus {
            code: 200,
            description: "A proxy server for Minecraft web clients",
            time: current_millis(),
            processing_time: started.elapsed().as_millis(),
        })
        .into_response(),
    )
}

async fn connect_options(State(state): State<BridgeState>, headers: HeaderMap) -> Response {
    if !state.is_origin_allowed(&headers) {
        return StatusCode::FORBIDDEN.into_response();
    }

    with_cors(headers, &state, StatusCode::NO_CONTENT.into_response())
}

async fn connect_tcp(
    State(state): State<BridgeState>,
    headers: HeaderMap,
    Json(request): Json<ConnectRequest>,
) -> Response {
    if !state.is_origin_allowed(&headers) {
        return StatusCode::FORBIDDEN.into_response();
    }

    if request.host.trim().is_empty() || request.port == 0 {
        return with_cors(
            headers,
            &state,
            (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    code: 400,
                    error: "host and port are required".into(),
                }),
            )
                .into_response(),
        );
    }

    let stream = match timeout(
        state.connect_timeout,
        TcpStream::connect((request.host.as_str(), request.port)),
    )
    .await
    {
        Ok(Ok(stream)) => stream,
        Ok(Err(error)) => {
            return with_cors(
                headers,
                &state,
                (
                    StatusCode::BAD_GATEWAY,
                    Json(ErrorResponse {
                        code: 502,
                        error: format!("failed to connect to Minecraft server: {error}"),
                    }),
                )
                    .into_response(),
            );
        }
        Err(_) => {
            return with_cors(
                headers,
                &state,
                (
                    StatusCode::GATEWAY_TIMEOUT,
                    Json(ErrorResponse {
                        code: 504,
                        error: "connection to Minecraft server timed out".into(),
                    }),
                )
                    .into_response(),
            );
        }
    };

    let token = Uuid::new_v4().simple().to_string();
    state
        .pending_connections
        .lock()
        .expect("bridge pending connection mutex")
        .insert(token.clone(), stream);

    with_cors(
        headers,
        &state,
        Json(ConnectResponse {
            token,
            remote: RemoteInfo {
                address: request.host,
                family: "IPv4",
                port: request.port,
            },
        })
        .into_response(),
    )
}

async fn socket_ws(
    ws: WebSocketUpgrade,
    Query(query): Query<HashMap<String, String>>,
    State(state): State<BridgeState>,
    headers: HeaderMap,
) -> Response {
    if !state.is_origin_allowed(&headers) {
        return StatusCode::FORBIDDEN.into_response();
    }

    let Some(token) = query.get("token") else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let stream = state
        .pending_connections
        .lock()
        .expect("bridge pending connection mutex")
        .remove(token);

    let Some(stream) = stream else {
        return StatusCode::NOT_FOUND.into_response();
    };

    ws.on_upgrade(move |socket| proxy_socket(socket, stream))
}

async fn ping_ws(
    ws: WebSocketUpgrade,
    State(state): State<BridgeState>,
    headers: HeaderMap,
) -> Response {
    if !state.is_origin_allowed(&headers) {
        return StatusCode::FORBIDDEN.into_response();
    }

    ws.on_upgrade(handle_ping_socket)
}

async fn handle_ping_socket(socket: WebSocket) {
    let (mut sender, mut receiver) = socket.split();
    while let Some(Ok(message)) = receiver.next().await {
        if let Message::Text(text) = message {
            if let Some(id) = text.strip_prefix("ping:") {
                let _ = sender.send(Message::Text(format!("pong:{id}:0"))).await;
            }
        }
    }
}

async fn proxy_socket(socket: WebSocket, stream: TcpStream) {
    let (mut ws_sender, mut ws_receiver) = socket.split();
    let (mut tcp_reader, mut tcp_writer) = stream.into_split();
    let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel::<Message>();

    let ws_writer = tokio::spawn(async move {
        while let Some(message) = outbound_rx.recv().await {
            if ws_sender.send(message).await.is_err() {
                break;
            }
        }
    });

    let tcp_to_ws_tx = outbound_tx.clone();
    let mut tcp_reader_task = tokio::spawn(async move {
        let mut buffer = [0_u8; 8192];
        loop {
            match tcp_reader.read(&mut buffer).await {
                Ok(0) => {
                    let _ = tcp_to_ws_tx.send(Message::Text(
                        "proxy-shutdown:Minecraft server closed the connection.".into(),
                    ));
                    let _ = tcp_to_ws_tx.send(Message::Close(None));
                    break;
                }
                Ok(read) => {
                    if tcp_to_ws_tx
                        .send(Message::Binary(buffer[..read].to_vec()))
                        .is_err()
                    {
                        break;
                    }
                }
                Err(error) => {
                    let _ = tcp_to_ws_tx.send(Message::Text(format!(
                        "proxy-shutdown:Minecraft server connection failed: {error}"
                    )));
                    let _ = tcp_to_ws_tx.send(Message::Close(None));
                    break;
                }
            }
        }
    });

    let ws_to_tcp_tx = outbound_tx.clone();
    let mut ws_reader_task = tokio::spawn(async move {
        while let Some(message) = ws_receiver.next().await {
            match message {
                Ok(Message::Binary(bytes)) => {
                    if tcp_writer.write_all(&bytes).await.is_err() {
                        break;
                    }
                }
                Ok(Message::Text(text)) => {
                    if let Some(id) = text.strip_prefix("ping:") {
                        let _ = ws_to_tcp_tx.send(Message::Text(format!("pong:{id}")));
                    } else if tcp_writer.write_all(text.as_bytes()).await.is_err() {
                        break;
                    }
                }
                Ok(Message::Close(_)) => break,
                Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => {}
                Err(_) => break,
            }
        }
        let _ = tcp_writer.shutdown().await;
    });

    tokio::select! {
        _ = &mut tcp_reader_task => {
            ws_reader_task.abort();
            let _ = ws_reader_task.await;
        }
        _ = &mut ws_reader_task => {
            tcp_reader_task.abort();
            let _ = tcp_reader_task.await;
        }
    }

    drop(outbound_tx);
    let _ = ws_writer.await;
}

fn resolve_addr(address: &str) -> Option<SocketAddr> {
    address.to_socket_addrs().ok()?.next()
}

fn current_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn with_cors(headers: HeaderMap, state: &BridgeState, mut response: Response) -> Response {
    let cors_origin = state.cors_origin(&headers);
    let response_headers = response.headers_mut();
    response_headers.insert(ACCESS_CONTROL_ALLOW_ORIGIN, cors_origin);
    response_headers.insert(
        ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("Content-Type, Authorization"),
    );
    response_headers.insert(
        ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, POST, OPTIONS"),
    );
    response_headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    response_headers.insert(
        axum::http::header::ACCESS_CONTROL_MAX_AGE,
        HeaderValue::from_static("86400"),
    );
    response_headers.insert(
        axum::http::header::ALLOW,
        HeaderValue::from_static("GET, POST, OPTIONS"),
    );
    response_headers.insert(
        axum::http::header::HeaderName::from_static("x-world-loom-bridge"),
        HeaderValue::from_static("rust"),
    );
    response_headers.insert(
        axum::http::header::HeaderName::from_static("access-control-allow-private-network"),
        HeaderValue::from_static("true"),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowed_origins_can_allow_any_origin() {
        assert!(AllowedOrigins::Any.is_allowed("https://example.test"));
    }

    #[test]
    fn allowed_origins_match_exact_origins_only() {
        let allowed = AllowedOrigins::List(vec!["http://localhost:3000".into()]);

        assert!(allowed.is_allowed("http://localhost:3000"));
        assert!(!allowed.is_allowed("http://127.0.0.1:3000"));
    }

    #[test]
    fn bridge_default_address_resolves() {
        assert_eq!(
            resolve_addr(DEFAULT_BRIDGE_ADDR).expect("default bridge address"),
            "127.0.0.1:18081".parse().expect("socket addr")
        );
    }
}
