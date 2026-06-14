use std::collections::HashMap;
use std::env;
use std::error::Error;
use std::fmt;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Mutex;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde_json::{json, Value};
use valence::prelude::{BlockPos, BlockState, Resource};

pub const MCP_ADDR_ENV: &str = "WORLD_LOOM_MCP_ADDR";
pub const DEFAULT_MCP_ADDR: &str = "127.0.0.1:8765";
pub const MCP_PROTOCOL_VERSION: &str = "2025-06-18";
pub const MCP_ENDPOINT_PATH: &str = "/mcp";
pub const MAX_SNAPSHOT_BLOCKS: i64 = 512;
pub const MAX_FILL_BLOCKS: i64 = 128;

const MAX_HTTP_BODY_BYTES: usize = 1024 * 1024;
const MCP_TOOL_TIMEOUT: Duration = Duration::from_secs(5);
const HTTP_READ_TIMEOUT: Duration = Duration::from_secs(5);

pub type McpToolResult = Result<Value, String>;

#[derive(Debug)]
pub enum McpError {
    Io(std::io::Error),
    AddrParse(std::net::AddrParseError),
}

impl fmt::Display for McpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "I/O error: {err}"),
            Self::AddrParse(err) => write!(f, "address parse error: {err}"),
        }
    }
}

impl Error for McpError {}

impl From<std::io::Error> for McpError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

impl From<std::net::AddrParseError> for McpError {
    fn from(err: std::net::AddrParseError) -> Self {
        Self::AddrParse(err)
    }
}

#[derive(Debug)]
pub struct McpToolRequest {
    pub name: String,
    pub arguments: Value,
    reply: Sender<McpToolResult>,
}

impl McpToolRequest {
    pub fn respond(self, result: McpToolResult) {
        let _ = self.reply.send(result);
    }
}

pub struct McpRuntime {
    addr: SocketAddr,
    receiver: Mutex<Receiver<McpToolRequest>>,
    shutdown: Sender<()>,
    server: Mutex<Option<JoinHandle<()>>>,
}

impl Resource for McpRuntime {}

impl fmt::Debug for McpRuntime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("McpRuntime")
            .field("addr", &self.addr)
            .finish_non_exhaustive()
    }
}

impl McpRuntime {
    pub fn start_default() -> Result<Self, McpError> {
        let addr = env::var(MCP_ADDR_ENV)
            .unwrap_or_else(|_| DEFAULT_MCP_ADDR.to_string())
            .parse::<SocketAddr>()?;
        Self::start(addr)
    }

    pub fn start(addr: SocketAddr) -> Result<Self, McpError> {
        let listener = TcpListener::bind(addr)?;
        let addr = listener.local_addr()?;
        listener.set_nonblocking(true)?;

        let (tool_sender, tool_receiver) = mpsc::channel();
        let (shutdown_sender, shutdown_receiver) = mpsc::channel();
        let server = thread::Builder::new()
            .name("world-loom-mcp-http".to_string())
            .spawn(move || serve_http(listener, shutdown_receiver, tool_sender))
            .map_err(McpError::Io)?;

        Ok(Self {
            addr,
            receiver: Mutex::new(tool_receiver),
            shutdown: shutdown_sender,
            server: Mutex::new(Some(server)),
        })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn try_recv(&self) -> Option<McpToolRequest> {
        self.receiver
            .lock()
            .ok()
            .and_then(|receiver| receiver.try_recv().ok())
    }
}

impl Drop for McpRuntime {
    fn drop(&mut self) {
        let _ = self.shutdown.send(());
        if let Ok(mut server) = self.server.lock() {
            if let Some(handle) = server.take() {
                if handle.join().is_err() {
                    eprintln!("[world-loom] MCP HTTP thread panicked during shutdown");
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    pub min: BlockPos,
    pub max: BlockPos,
}

impl Region {
    pub fn volume(self) -> i64 {
        i64::from(self.max.x - self.min.x + 1)
            * i64::from(self.max.y - self.min.y + 1)
            * i64::from(self.max.z - self.min.z + 1)
    }
}

pub fn parse_block_name(name: &str) -> Option<BlockState> {
    let normalized = name
        .trim()
        .strip_prefix("minecraft:")
        .unwrap_or(name.trim())
        .to_ascii_lowercase();

    match normalized.as_str() {
        "air" => Some(BlockState::AIR),
        "stone" => Some(BlockState::STONE),
        "dirt" => Some(BlockState::DIRT),
        "grass_block" => Some(BlockState::GRASS_BLOCK),
        "oak_planks" => Some(BlockState::OAK_PLANKS),
        "cobblestone" => Some(BlockState::COBBLESTONE),
        "glass" => Some(BlockState::GLASS),
        _ => None,
    }
}

pub fn block_state_name(block: BlockState) -> String {
    match block {
        BlockState::AIR => "air".to_string(),
        BlockState::BEDROCK => "bedrock".to_string(),
        BlockState::STONE => "stone".to_string(),
        BlockState::DIRT => "dirt".to_string(),
        BlockState::GRASS_BLOCK => "grass_block".to_string(),
        BlockState::OAK_PLANKS => "oak_planks".to_string(),
        BlockState::COBBLESTONE => "cobblestone".to_string(),
        BlockState::GLASS => "glass".to_string(),
        _ => block.to_string(),
    }
}

pub fn block_json(position: BlockPos, block: BlockState) -> Value {
    json!({
        "x": position.x,
        "y": position.y,
        "z": position.z,
        "block": block_state_name(block),
        "block_state_raw": block.to_raw(),
    })
}

pub fn required_i32(arguments: &Value, name: &str) -> Result<i32, String> {
    let value = arguments
        .get(name)
        .and_then(Value::as_i64)
        .ok_or_else(|| format!("missing integer argument `{name}`"))?;

    i32::try_from(value).map_err(|_| format!("argument `{name}` is outside i32 range"))
}

pub fn required_block_pos(arguments: &Value) -> Result<BlockPos, String> {
    Ok(BlockPos::new(
        required_i32(arguments, "x")?,
        required_i32(arguments, "y")?,
        required_i32(arguments, "z")?,
    ))
}

pub fn required_block_state(arguments: &Value) -> Result<BlockState, String> {
    let name = arguments
        .get("block")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing string argument `block`".to_string())?;
    parse_block_name(name).ok_or_else(|| format!("unsupported block `{name}`"))
}

pub fn required_region(arguments: &Value, limit: i64) -> Result<Region, String> {
    let region = Region {
        min: BlockPos::new(
            required_i32(arguments, "min_x")?,
            required_i32(arguments, "min_y")?,
            required_i32(arguments, "min_z")?,
        ),
        max: BlockPos::new(
            required_i32(arguments, "max_x")?,
            required_i32(arguments, "max_y")?,
            required_i32(arguments, "max_z")?,
        ),
    };

    if region.min.x > region.max.x || region.min.y > region.max.y || region.min.z > region.max.z {
        return Err("region min coordinates must be <= max coordinates".to_string());
    }

    let volume = region.volume();
    if volume > limit {
        return Err(format!("region volume {volume} exceeds limit {limit}"));
    }

    Ok(region)
}

pub fn tool_definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "server_status",
            "title": "Server Status",
            "description": "Return local server tick, player count, bounds, and persistence/MCP paths.",
            "inputSchema": object_schema(json!({}), vec![]),
            "annotations": { "readOnlyHint": true }
        }),
        json!({
            "name": "list_players",
            "title": "List Players",
            "description": "List connected players with position and ping.",
            "inputSchema": object_schema(json!({}), vec![]),
            "annotations": { "readOnlyHint": true }
        }),
        json!({
            "name": "get_world_bounds",
            "title": "Get World Bounds",
            "description": "Return the bounded World Loom coordinate limits.",
            "inputSchema": object_schema(json!({}), vec![]),
            "annotations": { "readOnlyHint": true }
        }),
        json!({
            "name": "get_block",
            "title": "Get Block",
            "description": "Inspect one block in the live server world.",
            "inputSchema": xyz_schema(),
            "annotations": { "readOnlyHint": true }
        }),
        json!({
            "name": "snapshot_region",
            "title": "Snapshot Region",
            "description": format!("Inspect a bounded cuboid. Maximum volume: {MAX_SNAPSHOT_BLOCKS} blocks."),
            "inputSchema": region_schema(),
            "annotations": { "readOnlyHint": true }
        }),
        json!({
            "name": "set_block",
            "title": "Set Block",
            "description": "Set one block through WorldCommand validation. Supported blocks: stone, dirt, grass_block, oak_planks, cobblestone, glass.",
            "inputSchema": block_edit_schema(),
            "annotations": { "destructiveHint": false }
        }),
        json!({
            "name": "remove_block",
            "title": "Remove Block",
            "description": "Remove one block through WorldCommand validation.",
            "inputSchema": xyz_schema(),
            "annotations": { "destructiveHint": true }
        }),
        json!({
            "name": "fill_region",
            "title": "Fill Region",
            "description": format!("Fill a bounded cuboid through WorldCommand validation. Use block=air to remove non-air cells. Maximum volume: {MAX_FILL_BLOCKS} blocks."),
            "inputSchema": fill_region_schema(),
            "annotations": { "destructiveHint": true }
        }),
    ]
}

pub fn is_known_tool(name: &str) -> bool {
    matches!(
        name,
        "server_status"
            | "list_players"
            | "get_world_bounds"
            | "get_block"
            | "snapshot_region"
            | "set_block"
            | "remove_block"
            | "fill_region"
    )
}

pub fn tool_result(result: Value) -> Value {
    json!({
        "content": [
            {
                "type": "text",
                "text": serde_json::to_string_pretty(&result).unwrap_or_else(|_| result.to_string()),
            }
        ],
        "structuredContent": result,
        "isError": false,
    })
}

pub fn tool_error(message: impl Into<String>) -> Value {
    json!({
        "content": [
            {
                "type": "text",
                "text": message.into(),
            }
        ],
        "isError": true,
    })
}

fn serve_http(listener: TcpListener, shutdown: Receiver<()>, tool_sender: Sender<McpToolRequest>) {
    loop {
        if shutdown.try_recv().is_ok() {
            break;
        }

        match listener.accept() {
            Ok((stream, _)) => {
                let sender = tool_sender.clone();
                let _ = thread::Builder::new()
                    .name("world-loom-mcp-connection".to_string())
                    .spawn(move || {
                        if let Err(err) = handle_http_connection(stream, sender) {
                            eprintln!("[world-loom] MCP HTTP request failed: {err}");
                        }
                    });
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(20));
            }
            Err(err) => {
                eprintln!("[world-loom] MCP HTTP accept failed: {err}");
                thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

fn handle_http_connection(
    mut stream: TcpStream,
    tool_sender: Sender<McpToolRequest>,
) -> Result<(), String> {
    let request = read_http_request(&mut stream)?;

    if !origin_allowed(&request.headers) {
        write_response(&mut stream, 403, "Forbidden", None)?;
        return Ok(());
    }

    if request.method == "OPTIONS" {
        write_response(&mut stream, 204, "No Content", None)?;
        return Ok(());
    }

    if request.path != MCP_ENDPOINT_PATH {
        write_json_response(
            &mut stream,
            404,
            json_rpc_error(Value::Null, -32004, "MCP endpoint is /mcp"),
        )?;
        return Ok(());
    }

    if request.method == "GET" {
        write_response(&mut stream, 405, "Method Not Allowed", None)?;
        return Ok(());
    }

    if request.method != "POST" {
        write_response(&mut stream, 405, "Method Not Allowed", None)?;
        return Ok(());
    }

    let body: Value = match serde_json::from_slice(&request.body) {
        Ok(body) => body,
        Err(err) => {
            write_json_response(
                &mut stream,
                400,
                json_rpc_error(Value::Null, -32700, format!("parse error: {err}")),
            )?;
            return Ok(());
        }
    };

    if body.get("id").is_none() {
        write_response(&mut stream, 202, "Accepted", None)?;
        return Ok(());
    }

    let response = handle_json_rpc_request(body, tool_sender);
    write_json_response(&mut stream, 200, response)
}

fn handle_json_rpc_request(body: Value, tool_sender: Sender<McpToolRequest>) -> Value {
    let id = body.get("id").cloned().unwrap_or(Value::Null);
    let Some(method) = body.get("method").and_then(Value::as_str) else {
        return json_rpc_error(id, -32600, "missing method");
    };

    match method {
        "initialize" => json_rpc_success(
            id,
            json!({
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {
                    "tools": {
                        "listChanged": false
                    }
                },
                "serverInfo": {
                    "name": "world-loom-server",
                    "version": env!("CARGO_PKG_VERSION")
                },
                "instructions": "Local World Loom MCP endpoint. Edit tools are bounded and routed through WorldCommand validation."
            }),
        ),
        "ping" => json_rpc_success(id, json!({})),
        "tools/list" => json_rpc_success(id, json!({ "tools": tool_definitions() })),
        "tools/call" => handle_tools_call(id, body.get("params").cloned(), tool_sender),
        _ => json_rpc_error(id, -32601, format!("unknown method `{method}`")),
    }
}

fn handle_tools_call(
    id: Value,
    params: Option<Value>,
    tool_sender: Sender<McpToolRequest>,
) -> Value {
    let Some(params) = params else {
        return json_rpc_error(id, -32602, "missing params");
    };
    let Some(name) = params.get("name").and_then(Value::as_str) else {
        return json_rpc_error(id, -32602, "missing params.name");
    };
    if !is_known_tool(name) {
        return json_rpc_error(id, -32602, format!("unknown tool `{name}`"));
    }

    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let (reply_sender, reply_receiver) = mpsc::channel();

    if tool_sender
        .send(McpToolRequest {
            name: name.to_string(),
            arguments,
            reply: reply_sender,
        })
        .is_err()
    {
        return json_rpc_success(id, tool_error("MCP world request queue is closed"));
    }

    match reply_receiver.recv_timeout(MCP_TOOL_TIMEOUT) {
        Ok(Ok(result)) => json_rpc_success(id, tool_result(result)),
        Ok(Err(message)) => json_rpc_success(id, tool_error(message)),
        Err(RecvTimeoutError::Timeout) => {
            json_rpc_success(id, tool_error("MCP tool timed out waiting for server tick"))
        }
        Err(RecvTimeoutError::Disconnected) => {
            json_rpc_success(id, tool_error("MCP world response channel closed"))
        }
    }
}

fn json_rpc_success(id: Value, result: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    })
}

fn json_rpc_error(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message.into(),
        }
    })
}

#[derive(Debug)]
struct HttpRequest {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

fn read_http_request(stream: &mut TcpStream) -> Result<HttpRequest, String> {
    stream
        .set_read_timeout(Some(HTTP_READ_TIMEOUT))
        .map_err(|err| format!("set read timeout failed: {err}"))?;

    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 4096];
    let header_end;
    let content_length;

    loop {
        let n = stream
            .read(&mut chunk)
            .map_err(|err| format!("read failed: {err}"))?;
        if n == 0 {
            return Err("connection closed before full HTTP request".to_string());
        }
        buffer.extend_from_slice(&chunk[..n]);
        if buffer.len() > MAX_HTTP_BODY_BYTES {
            return Err("HTTP request too large".to_string());
        }

        if let Some(idx) = find_header_end(&buffer) {
            header_end = idx;
            let header_text = std::str::from_utf8(&buffer[..idx])
                .map_err(|err| format!("invalid HTTP header UTF-8: {err}"))?;
            let headers = parse_headers(header_text)?;
            content_length = headers
                .headers
                .get("content-length")
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or(0);

            if content_length > MAX_HTTP_BODY_BYTES {
                return Err("HTTP body too large".to_string());
            }

            if buffer.len() >= header_end + content_length {
                return Ok(HttpRequest {
                    method: headers.method,
                    path: headers.path,
                    headers: headers.headers,
                    body: buffer[header_end..header_end + content_length].to_vec(),
                });
            }
            break;
        }
    }

    while buffer.len() < header_end + content_length {
        let n = stream
            .read(&mut chunk)
            .map_err(|err| format!("read body failed: {err}"))?;
        if n == 0 {
            return Err("connection closed before full HTTP body".to_string());
        }
        buffer.extend_from_slice(&chunk[..n]);
        if buffer.len() > header_end + content_length {
            break;
        }
    }

    let header_text = std::str::from_utf8(&buffer[..header_end - 4])
        .map_err(|err| format!("invalid HTTP header UTF-8: {err}"))?;
    let headers = parse_headers(header_text)?;
    Ok(HttpRequest {
        method: headers.method,
        path: headers.path,
        headers: headers.headers,
        body: buffer[header_end..header_end + content_length].to_vec(),
    })
}

#[derive(Debug)]
struct ParsedHeaders {
    method: String,
    path: String,
    headers: HashMap<String, String>,
}

fn parse_headers(header_text: &str) -> Result<ParsedHeaders, String> {
    let mut lines = header_text.lines();
    let request_line = lines.next().ok_or("missing HTTP request line")?;
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts
        .next()
        .ok_or("missing HTTP method")?
        .to_string();
    let path = request_parts.next().ok_or("missing HTTP path")?.to_string();

    let mut headers = HashMap::new();
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
    }

    Ok(ParsedHeaders {
        method,
        path,
        headers,
    })
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|idx| idx + 4)
}

fn origin_allowed(headers: &HashMap<String, String>) -> bool {
    let Some(origin) = headers.get("origin") else {
        return true;
    };

    origin == "null"
        || origin.starts_with("http://127.0.0.1")
        || origin.starts_with("http://localhost")
        || origin.starts_with("http://[::1]")
}

fn write_json_response(stream: &mut TcpStream, status: u16, body: Value) -> Result<(), String> {
    write_response(stream, status, status_text(status), Some(body.to_string()))
}

fn write_response(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    body: Option<String>,
) -> Result<(), String> {
    let body = body.unwrap_or_default();
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Access-Control-Allow-Origin: http://localhost\r\n\
         Access-Control-Allow-Headers: content-type, accept, mcp-protocol-version\r\n\
         Access-Control-Allow-Methods: POST, GET, OPTIONS\r\n\
         MCP-Protocol-Version: {MCP_PROTOCOL_VERSION}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        body.len()
    );
    stream
        .write_all(response.as_bytes())
        .map_err(|err| format!("write response failed: {err}"))
}

fn status_text(status: u16) -> &'static str {
    match status {
        200 => "OK",
        202 => "Accepted",
        204 => "No Content",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "OK",
    }
}

fn object_schema(properties: Value, required: Vec<&str>) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

fn integer_property(description: &str) -> Value {
    json!({ "type": "integer", "description": description })
}

fn xyz_schema() -> Value {
    object_schema(
        json!({
            "x": integer_property("Block X coordinate"),
            "y": integer_property("Block Y coordinate"),
            "z": integer_property("Block Z coordinate"),
        }),
        vec!["x", "y", "z"],
    )
}

fn block_edit_schema() -> Value {
    object_schema(
        json!({
            "x": integer_property("Block X coordinate"),
            "y": integer_property("Block Y coordinate"),
            "z": integer_property("Block Z coordinate"),
            "block": {
                "type": "string",
                "enum": ["stone", "dirt", "grass_block", "oak_planks", "cobblestone", "glass"],
            },
        }),
        vec!["x", "y", "z", "block"],
    )
}

fn region_schema() -> Value {
    object_schema(
        json!({
            "min_x": integer_property("Minimum X coordinate"),
            "min_y": integer_property("Minimum Y coordinate"),
            "min_z": integer_property("Minimum Z coordinate"),
            "max_x": integer_property("Maximum X coordinate"),
            "max_y": integer_property("Maximum Y coordinate"),
            "max_z": integer_property("Maximum Z coordinate"),
        }),
        vec!["min_x", "min_y", "min_z", "max_x", "max_y", "max_z"],
    )
}

fn fill_region_schema() -> Value {
    object_schema(
        json!({
            "min_x": integer_property("Minimum X coordinate"),
            "min_y": integer_property("Minimum Y coordinate"),
            "min_z": integer_property("Minimum Z coordinate"),
            "max_x": integer_property("Maximum X coordinate"),
            "max_y": integer_property("Maximum Y coordinate"),
            "max_z": integer_property("Maximum Z coordinate"),
            "block": {
                "type": "string",
                "enum": ["air", "stone", "dirt", "grass_block", "oak_planks", "cobblestone", "glass"],
            },
        }),
        vec![
            "min_x", "min_y", "min_z", "max_x", "max_y", "max_z", "block",
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_list_contains_m6_tools() {
        let names = tool_definitions()
            .into_iter()
            .map(|tool| {
                tool.get("name")
                    .and_then(Value::as_str)
                    .expect("tool should have name")
                    .to_string()
            })
            .collect::<Vec<_>>();

        assert_eq!(
            names,
            vec![
                "server_status",
                "list_players",
                "get_world_bounds",
                "get_block",
                "snapshot_region",
                "set_block",
                "remove_block",
                "fill_region",
            ]
        );
    }

    #[test]
    fn region_volume_limit_is_strict() {
        let allowed = json!({
            "min_x": 0,
            "min_y": 0,
            "min_z": 0,
            "max_x": 7,
            "max_y": 7,
            "max_z": 7,
        });
        assert_eq!(
            required_region(&allowed, MAX_SNAPSHOT_BLOCKS)
                .expect("512-block region should be accepted")
                .volume(),
            MAX_SNAPSHOT_BLOCKS
        );

        let too_large = json!({
            "min_x": 0,
            "min_y": 0,
            "min_z": 0,
            "max_x": 8,
            "max_y": 7,
            "max_z": 7,
        });
        assert!(required_region(&too_large, MAX_SNAPSHOT_BLOCKS).is_err());
    }

    #[test]
    fn fill_region_has_smaller_limit_than_snapshot_region() {
        let too_large = json!({
            "min_x": 0,
            "min_y": 0,
            "min_z": 0,
            "max_x": 4,
            "max_y": 4,
            "max_z": 5,
        });
        assert!(required_region(&too_large, MAX_FILL_BLOCKS).is_err());
    }

    #[test]
    fn block_names_are_limited_to_world_command_blocks() {
        assert_eq!(parse_block_name("minecraft:stone"), Some(BlockState::STONE));
        assert_eq!(parse_block_name("glass"), Some(BlockState::GLASS));
        assert_eq!(parse_block_name("diamond_block"), None);
    }

    #[test]
    fn tools_list_returns_json_rpc_shape() {
        let response = handle_json_rpc_request(
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/list"
            }),
            mpsc::channel().0,
        );

        assert_eq!(response["jsonrpc"], "2.0");
        assert_eq!(response["id"], 1);
        assert_eq!(
            response["result"]["tools"]
                .as_array()
                .expect("tools should be an array")
                .len(),
            8
        );
    }
}
