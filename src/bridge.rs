use std::{
    collections::HashMap,
    env, fs,
    net::{SocketAddr, ToSocketAddrs},
    path::Path,
    sync::mpsc::Sender as StdSender,
    sync::{Arc, Mutex},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use crate::mcp::{self, McpToolRequest, MCP_ENDPOINT_PATH, MCP_PROTOCOL_VERSION};
use axum::{
    body::Bytes,
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Query, State,
    },
    http::{
        header::{
            ACCESS_CONTROL_ALLOW_HEADERS, ACCESS_CONTROL_ALLOW_METHODS,
            ACCESS_CONTROL_ALLOW_ORIGIN, AUTHORIZATION, CONTENT_TYPE, ORIGIN, WWW_AUTHENTICATE,
        },
        HeaderMap, HeaderValue, StatusCode,
    },
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
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
pub const BRIDGE_TCP_READ_BUFFER_BYTES_ENV: &str = "WORLD_LOOM_BRIDGE_TCP_READ_BUFFER_BYTES";
pub const BRIDGE_WS_QUEUE_CAPACITY_ENV: &str = "WORLD_LOOM_BRIDGE_WS_QUEUE_CAPACITY";
pub const BRIDGE_MAX_PENDING_CONNECTIONS_ENV: &str = "WORLD_LOOM_BRIDGE_MAX_PENDING_CONNECTIONS";
pub const MCP_API_KEY_FILE_ENV: &str = "WORLD_LOOM_MCP_API_KEY_FILE";
pub const DEFAULT_BRIDGE_ADDR: &str = "127.0.0.1:18081";

const API_ROOT: &str = "/api/vm/net";
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_LOCAL_ORIGINS: &[&str] = &["http://localhost:3000", "http://127.0.0.1:3000"];
const DEFAULT_TCP_READ_BUFFER_BYTES: usize = 16 * 1024;
const MIN_TCP_READ_BUFFER_BYTES: usize = 4 * 1024;
const MAX_TCP_READ_BUFFER_BYTES: usize = 64 * 1024;
const DEFAULT_WS_QUEUE_CAPACITY: usize = 1024;
const MIN_WS_QUEUE_CAPACITY: usize = 16;
const MAX_WS_QUEUE_CAPACITY: usize = 8192;
const DEFAULT_MAX_PENDING_CONNECTIONS: usize = 128;
const MIN_MAX_PENDING_CONNECTIONS: usize = 1;
const MAX_MAX_PENDING_CONNECTIONS: usize = 1024;

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
    config: BridgeConfig,
    shutdown_tx: Mutex<Option<oneshot::Sender<()>>>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl Resource for BridgeRuntime {}

impl BridgeRuntime {
    pub fn start_default(mcp_sender: StdSender<McpToolRequest>) -> Result<Self, BridgeError> {
        let requested_addr =
            env::var(BRIDGE_ADDR_ENV).unwrap_or_else(|_| DEFAULT_BRIDGE_ADDR.into());
        let addr = resolve_addr(&requested_addr)
            .ok_or_else(|| BridgeError::InvalidAddress(requested_addr.clone()))?;
        let allowed_origins = AllowedOrigins::from_env();
        let config = BridgeConfig::from_env();
        let mcp_auth = McpAuth::from_env().map_err(BridgeError::Startup)?;
        Self::start(addr, allowed_origins, config, mcp_auth, mcp_sender)
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn config(&self) -> BridgeConfig {
        self.config
    }

    fn start(
        addr: SocketAddr,
        allowed_origins: AllowedOrigins,
        config: BridgeConfig,
        mcp_auth: McpAuth,
        mcp_sender: StdSender<McpToolRequest>,
    ) -> Result<Self, BridgeError> {
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
                    let state = BridgeState::new(allowed_origins, config, mcp_auth, mcp_sender);
                    let app = Router::new()
                        .route(
                            &format!("{API_ROOT}/connect"),
                            get(connect_status)
                                .post(connect_tcp)
                                .options(connect_options),
                        )
                        .route(&format!("{API_ROOT}/socket"), get(socket_ws))
                        .route(&format!("{API_ROOT}/ping"), get(ping_ws))
                        .route(
                            MCP_ENDPOINT_PATH,
                            get(mcp_get).post(mcp_post).options(mcp_options),
                        )
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
                config,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BridgeConfig {
    pub tcp_read_buffer_bytes: usize,
    pub ws_queue_capacity: usize,
    pub max_pending_connections: usize,
}

impl BridgeConfig {
    fn from_env() -> Self {
        Self {
            tcp_read_buffer_bytes: bounded_env_usize(
                BRIDGE_TCP_READ_BUFFER_BYTES_ENV,
                DEFAULT_TCP_READ_BUFFER_BYTES,
                MIN_TCP_READ_BUFFER_BYTES,
                MAX_TCP_READ_BUFFER_BYTES,
            ),
            ws_queue_capacity: bounded_env_usize(
                BRIDGE_WS_QUEUE_CAPACITY_ENV,
                DEFAULT_WS_QUEUE_CAPACITY,
                MIN_WS_QUEUE_CAPACITY,
                MAX_WS_QUEUE_CAPACITY,
            ),
            max_pending_connections: bounded_env_usize(
                BRIDGE_MAX_PENDING_CONNECTIONS_ENV,
                DEFAULT_MAX_PENDING_CONNECTIONS,
                MIN_MAX_PENDING_CONNECTIONS,
                MAX_MAX_PENDING_CONNECTIONS,
            ),
        }
    }
}

#[derive(Clone)]
struct BridgeState {
    pending_connections: Arc<Mutex<HashMap<String, TcpStream>>>,
    allowed_origins: Arc<AllowedOrigins>,
    mcp_auth: McpAuth,
    mcp_sender: StdSender<McpToolRequest>,
    connect_timeout: Duration,
    config: BridgeConfig,
}

impl BridgeState {
    fn new(
        allowed_origins: AllowedOrigins,
        config: BridgeConfig,
        mcp_auth: McpAuth,
        mcp_sender: StdSender<McpToolRequest>,
    ) -> Self {
        Self {
            pending_connections: Arc::new(Mutex::new(HashMap::new())),
            allowed_origins: Arc::new(allowed_origins),
            mcp_auth,
            mcp_sender,
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            config,
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

    fn is_mcp_authorized(&self, headers: &HeaderMap) -> bool {
        self.mcp_auth.is_authorized(headers)
    }

    fn insert_pending_connection(&self, token: String, stream: TcpStream) -> Result<(), TcpStream> {
        let mut pending = self
            .pending_connections
            .lock()
            .expect("bridge pending connection mutex");
        if pending.len() >= self.config.max_pending_connections {
            return Err(stream);
        }

        pending.insert(token, stream);
        Ok(())
    }
}

#[derive(Clone)]
struct McpAuth {
    expected_digest: Option<[u8; 32]>,
}

impl McpAuth {
    fn from_env() -> Result<Self, String> {
        let Some(path) = env::var_os(MCP_API_KEY_FILE_ENV) else {
            return Ok(Self::disabled());
        };
        let token = fs::read_to_string(Path::new(&path)).map_err(|error| {
            format!(
                "failed to read {MCP_API_KEY_FILE_ENV} file {}: {error}",
                Path::new(&path).display()
            )
        })?;
        let token = token.trim();
        if token.is_empty() {
            return Err(format!("{MCP_API_KEY_FILE_ENV} file must not be empty"));
        }
        Ok(Self::required(token))
    }

    fn disabled() -> Self {
        Self {
            expected_digest: None,
        }
    }

    fn required(token: &str) -> Self {
        Self {
            expected_digest: Some(Sha256::digest(token.as_bytes()).into()),
        }
    }

    fn is_authorized(&self, headers: &HeaderMap) -> bool {
        let Some(expected_digest) = self.expected_digest else {
            return true;
        };
        let Some(value) = headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
        else {
            return false;
        };
        let Some((scheme, token)) = value.split_once(' ') else {
            return false;
        };
        if !scheme.eq_ignore_ascii_case("Bearer") || token.is_empty() {
            return false;
        }
        let actual_digest: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        constant_time_digest_eq(&expected_digest, &actual_digest)
    }
}

fn constant_time_digest_eq(left: &[u8; 32], right: &[u8; 32]) -> bool {
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
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
    if state
        .insert_pending_connection(token.clone(), stream)
        .is_err()
    {
        return with_cors(
            headers,
            &state,
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorResponse {
                    code: 503,
                    error: "too many pending bridge connections".into(),
                }),
            )
                .into_response(),
        );
    }

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

    let config = state.config;
    ws.on_upgrade(move |socket| proxy_socket(socket, stream, config))
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

async fn mcp_options(State(state): State<BridgeState>, headers: HeaderMap) -> Response {
    if !state.is_origin_allowed(&headers) {
        return StatusCode::FORBIDDEN.into_response();
    }

    mcp_http_response(headers, &state, 204, None)
}

async fn mcp_get(State(state): State<BridgeState>, headers: HeaderMap) -> Response {
    if !state.is_origin_allowed(&headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    if !state.is_mcp_authorized(&headers) {
        return mcp_unauthorized(headers, &state);
    }

    mcp_http_response(headers, &state, 405, None)
}

async fn mcp_post(State(state): State<BridgeState>, headers: HeaderMap, body: Bytes) -> Response {
    if !state.is_origin_allowed(&headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    if !state.is_mcp_authorized(&headers) {
        return mcp_unauthorized(headers, &state);
    }

    let mcp_sender = state.mcp_sender.clone();
    let response = match tokio::task::spawn_blocking(move || {
        mcp::handle_http_json_rpc_request(&body, mcp_sender)
    })
    .await
    {
        Ok(response) => response,
        Err(err) => {
            eprintln!("[world-loom] MCP HTTP task failed: {err}");
            mcp::McpHttpResponse {
                status: 500,
                body: None,
            }
        }
    };
    mcp_http_response(headers, &state, response.status, response.body)
}

fn mcp_unauthorized(headers: HeaderMap, state: &BridgeState) -> Response {
    let mut response = mcp_http_response(headers, state, 401, None);
    response.headers_mut().insert(
        WWW_AUTHENTICATE,
        HeaderValue::from_static("Bearer realm=\"world-loom-mcp\""),
    );
    response
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

fn mcp_http_response(
    headers: HeaderMap,
    state: &BridgeState,
    status: u16,
    body: Option<serde_json::Value>,
) -> Response {
    let status = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let response = match body {
        Some(body) => (status, body.to_string()).into_response(),
        None => status.into_response(),
    };
    let mut response = with_cors(headers, state, response);
    response.headers_mut().insert(
        axum::http::header::HeaderName::from_static("mcp-protocol-version"),
        HeaderValue::from_static(MCP_PROTOCOL_VERSION),
    );
    response
}

async fn proxy_socket(socket: WebSocket, stream: TcpStream, config: BridgeConfig) {
    let (mut ws_sender, mut ws_receiver) = socket.split();
    let (mut tcp_reader, mut tcp_writer) = stream.into_split();
    let (outbound_tx, mut outbound_rx) = mpsc::channel::<Message>(config.ws_queue_capacity);

    let ws_writer = tokio::spawn(async move {
        while let Some(message) = outbound_rx.recv().await {
            if ws_sender.send(message).await.is_err() {
                break;
            }
        }
    });

    let tcp_to_ws_tx = outbound_tx.clone();
    let mut tcp_reader_task = tokio::spawn(async move {
        let mut buffer = vec![0_u8; config.tcp_read_buffer_bytes];
        loop {
            match tcp_reader.read(&mut buffer).await {
                Ok(0) => {
                    let _ = tcp_to_ws_tx
                        .send(Message::Text(
                            "proxy-shutdown:Minecraft server closed the connection.".into(),
                        ))
                        .await;
                    let _ = tcp_to_ws_tx.send(Message::Close(None)).await;
                    break;
                }
                Ok(read) => {
                    if tcp_to_ws_tx
                        .send(Message::Binary(buffer[..read].to_vec()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Err(error) => {
                    let _ = tcp_to_ws_tx
                        .send(Message::Text(format!(
                            "proxy-shutdown:Minecraft server connection failed: {error}"
                        )))
                        .await;
                    let _ = tcp_to_ws_tx.send(Message::Close(None)).await;
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
                        let _ = ws_to_tcp_tx.send(Message::Text(format!("pong:{id}"))).await;
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

fn bounded_env_usize(name: &str, default: usize, min: usize, max: usize) -> usize {
    bounded_env_value(env::var(name).ok().as_deref(), default, min, max)
}

fn bounded_env_value(raw: Option<&str>, default: usize, min: usize, max: usize) -> usize {
    raw.and_then(|value| value.trim().parse::<usize>().ok())
        .map(|value| value.clamp(min, max))
        .unwrap_or(default)
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
        HeaderValue::from_static("Content-Type, Authorization, Accept, MCP-Protocol-Version"),
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
    use serde_json::json;
    use std::io::{Read, Write};

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

    #[test]
    fn bridge_config_env_values_are_bounded() {
        assert_eq!(
            bounded_env_value(Some("1"), 100, 10, 200),
            10,
            "too-small values clamp up"
        );
        assert_eq!(
            bounded_env_value(Some("500"), 100, 10, 200),
            200,
            "too-large values clamp down"
        );
        assert_eq!(
            bounded_env_value(Some("125"), 100, 10, 200),
            125,
            "in-range values pass through"
        );
        assert_eq!(
            bounded_env_value(Some("not-a-number"), 100, 10, 200),
            100,
            "invalid values fall back to default"
        );
        assert_eq!(
            bounded_env_value(None, 100, 10, 200),
            100,
            "missing values fall back to default"
        );
    }

    fn send_http_request(addr: SocketAddr, request: &str) -> String {
        let mut stream = std::net::TcpStream::connect(addr).expect("connect to bridge");
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("set read timeout");
        stream.write_all(request.as_bytes()).expect("write request");
        let mut response_bytes = Vec::new();
        let mut buffer = [0_u8; 4096];
        loop {
            match stream.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => response_bytes.extend_from_slice(&buffer[..read]),
                Err(err)
                    if matches!(
                        err.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    if response_bytes.is_empty() {
                        panic!("read response: {err}");
                    }
                    break;
                }
                Err(err) => panic!("read response: {err}"),
            }
        }
        String::from_utf8(response_bytes).expect("response should be utf8")
    }

    #[test]
    fn bridge_serves_mcp_initialize_on_same_http_listener() {
        let bridge = BridgeRuntime::start(
            "127.0.0.1:0".parse().expect("socket addr"),
            AllowedOrigins::Any,
            BridgeConfig {
                tcp_read_buffer_bytes: DEFAULT_TCP_READ_BUFFER_BYTES,
                ws_queue_capacity: DEFAULT_WS_QUEUE_CAPACITY,
                max_pending_connections: DEFAULT_MAX_PENDING_CONNECTIONS,
            },
            McpAuth::disabled(),
            std::sync::mpsc::channel().0,
        )
        .expect("bridge should start");
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize"
        })
        .to_string();
        let request = format!(
            "POST /mcp HTTP/1.1\r\n\
             Host: {}\r\n\
             Origin: http://localhost:3000\r\n\
             Content-Type: application/json\r\n\
             Content-Length: {}\r\n\
             \r\n\
             {}",
            bridge.addr(),
            body.len(),
            body
        );

        let response = send_http_request(bridge.addr(), &request);

        assert!(
            response.starts_with("HTTP/1.1 200 OK"),
            "unexpected response: {response}"
        );
        assert!(
            response.contains("\"name\":\"world-loom-server\""),
            "unexpected response: {response}"
        );
    }

    #[test]
    fn mcp_bearer_auth_rejects_missing_and_invalid_tokens() {
        let auth = McpAuth::required("correct-token");
        let headers = HeaderMap::new();
        assert!(!auth.is_authorized(&headers));

        let mut invalid_headers = HeaderMap::new();
        invalid_headers.insert(
            AUTHORIZATION,
            HeaderValue::from_static("Bearer wrong-token"),
        );
        assert!(!auth.is_authorized(&invalid_headers));

        let mut valid_headers = HeaderMap::new();
        valid_headers.insert(
            AUTHORIZATION,
            HeaderValue::from_static("Bearer correct-token"),
        );
        assert!(auth.is_authorized(&valid_headers));
    }

    #[test]
    fn bridge_requires_bearer_auth_for_mcp_requests() {
        let bridge = BridgeRuntime::start(
            "127.0.0.1:0".parse().expect("socket addr"),
            AllowedOrigins::Any,
            BridgeConfig {
                tcp_read_buffer_bytes: DEFAULT_TCP_READ_BUFFER_BYTES,
                ws_queue_capacity: DEFAULT_WS_QUEUE_CAPACITY,
                max_pending_connections: DEFAULT_MAX_PENDING_CONNECTIONS,
            },
            McpAuth::required("correct-token"),
            std::sync::mpsc::channel().0,
        )
        .expect("bridge should start");
        let request = format!(
            "GET /mcp HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
            bridge.addr()
        );
        let response = send_http_request(bridge.addr(), &request);

        assert!(
            response.starts_with("HTTP/1.1 401 Unauthorized"),
            "unexpected response: {response}"
        );
        assert!(
            response.contains("www-authenticate: Bearer realm=\"world-loom-mcp\""),
            "unexpected response: {response}"
        );
    }
}
