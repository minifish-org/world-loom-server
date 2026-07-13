use std::fmt;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Mutex;
use std::time::Duration;

use serde_json::{json, Value};
use valence::prelude::{BlockPos, BlockState, Resource};

use crate::build_palette::{block_name_for_state, parse_build_block_name};
use crate::build_plan::{parse_build_plan, BuildPlan, MAX_BUILD_OPERATIONS, MAX_BUILD_TARGETS};

pub const MCP_PROTOCOL_VERSION: &str = "2025-06-18";
pub const MCP_ENDPOINT_PATH: &str = "/mcp";
pub const MAX_SNAPSHOT_BLOCKS: i64 = 512;
pub const MAX_FILL_BLOCKS: i64 = 128;

const MCP_TOOL_TIMEOUT: Duration = Duration::from_secs(5);

pub type McpToolResult = Result<Value, String>;

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
    sender: Sender<McpToolRequest>,
    receiver: Mutex<Receiver<McpToolRequest>>,
}

impl Resource for McpRuntime {}

impl fmt::Debug for McpRuntime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("McpRuntime").finish_non_exhaustive()
    }
}

impl McpRuntime {
    pub fn new() -> Self {
        let (tool_sender, tool_receiver) = mpsc::channel();

        Self {
            sender: tool_sender,
            receiver: Mutex::new(tool_receiver),
        }
    }

    pub fn sender(&self) -> Sender<McpToolRequest> {
        self.sender.clone()
    }

    pub fn try_recv(&self) -> Option<McpToolRequest> {
        self.receiver
            .lock()
            .ok()
            .and_then(|receiver| receiver.try_recv().ok())
    }
}

impl Default for McpRuntime {
    fn default() -> Self {
        Self::new()
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
    let trimmed = name.trim();
    let normalized = trimmed
        .strip_prefix("minecraft:")
        .unwrap_or(trimmed)
        .to_ascii_lowercase();
    (normalized == "air")
        .then_some(BlockState::AIR)
        .or_else(|| parse_build_block_name(&normalized))
}

pub fn block_state_name(block: BlockState) -> String {
    match block {
        BlockState::AIR => "air".to_string(),
        BlockState::BEDROCK => "bedrock".to_string(),
        _ => block_name_for_state(block)
            .map(str::to_string)
            .unwrap_or_else(|| block.to_string()),
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

pub fn required_build_plan(arguments: &Value) -> Result<BuildPlan, String> {
    parse_build_plan(arguments.get("plan").unwrap_or(arguments))
}

pub fn required_build_id(arguments: &Value) -> Result<&str, String> {
    arguments
        .get("build_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= 128)
        .ok_or_else(|| "build_id must be a non-empty string of at most 128 bytes".to_string())
}

pub fn required_sculpt_spec(arguments: &Value) -> Result<&Value, String> {
    arguments
        .get("spec")
        .filter(|value| value.is_object())
        .ok_or_else(|| "missing object argument `spec`".to_string())
}

pub fn required_string<'a>(
    arguments: &'a Value,
    name: &str,
    max_bytes: usize,
) -> Result<&'a str, String> {
    arguments
        .get(name)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= max_bytes)
        .ok_or_else(|| format!("`{name}` must be a non-empty string of at most {max_bytes} bytes"))
}

pub fn optional_u32(arguments: &Value, name: &str) -> Result<Option<u32>, String> {
    let Some(value) = arguments.get(name) else {
        return Ok(None);
    };
    let value = value
        .as_u64()
        .ok_or_else(|| format!("`{name}` must be an unsigned integer"))?;
    u32::try_from(value)
        .map(Some)
        .map_err(|_| format!("`{name}` is outside u32 range"))
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
            "name": "list_build_palette",
            "title": "List Build Palette",
            "description": "List the authoritative Build Palette v1 block names and visual metadata.",
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
            "description": "Set one block through WorldCommand validation. Call list_build_palette for supported names.",
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
        json!({
            "name": "validate_build_plan",
            "title": "Validate Build Plan",
            "description": format!("Validate BuildPlan v1 without mutation. Maximum {MAX_BUILD_OPERATIONS} operations and {MAX_BUILD_TARGETS} expanded targets."),
            "inputSchema": build_plan_tool_schema(),
            "annotations": { "readOnlyHint": true }
        }),
        json!({
            "name": "apply_build_plan",
            "title": "Apply Build Plan",
            "description": "Atomically validate, apply, persist, and record an undoable BuildPlan v1.",
            "inputSchema": build_plan_tool_schema(),
            "annotations": { "destructiveHint": true, "idempotentHint": true }
        }),
        json!({
            "name": "get_build",
            "title": "Get Build",
            "description": "Return the persisted summary and state for a build_id.",
            "inputSchema": build_id_schema(),
            "annotations": { "readOnlyHint": true }
        }),
        json!({
            "name": "undo_build",
            "title": "Undo Build",
            "description": "Atomically restore the exact captured state for a build. Repeated undo is safe.",
            "inputSchema": build_id_schema(),
            "annotations": { "destructiveHint": true, "idempotentHint": true }
        }),
        json!({
            "name": "validate_sculpt_spec",
            "title": "Validate Sculpt Spec",
            "description": "Validate a bounded declarative SculptSpec v1 without publishing it.",
            "inputSchema": sculpt_spec_tool_schema(),
            "annotations": { "readOnlyHint": true }
        }),
        json!({
            "name": "inspect_asset_budget",
            "title": "Inspect Asset Budget",
            "description": "Return deterministic SculptSpec validation and geometry budgets without mutation.",
            "inputSchema": sculpt_spec_tool_schema(),
            "annotations": { "readOnlyHint": true }
        }),
        json!({
            "name": "publish_asset",
            "title": "Publish Asset",
            "description": "Validate and persist one immutable SculptSpec asset version.",
            "inputSchema": sculpt_spec_tool_schema(),
            "annotations": { "destructiveHint": false, "idempotentHint": true }
        }),
        json!({
            "name": "list_assets",
            "title": "List Assets",
            "description": "List the authoritative declarative asset catalog.",
            "inputSchema": object_schema(json!({}), vec![]),
            "annotations": { "readOnlyHint": true }
        }),
        json!({
            "name": "get_asset",
            "title": "Get Asset",
            "description": "Get one asset version, or the latest version when version is omitted.",
            "inputSchema": get_asset_schema(),
            "annotations": { "readOnlyHint": true }
        }),
        json!({
            "name": "spawn_asset",
            "title": "Spawn Asset",
            "description": "Persist and synchronize one bounded asset instance.",
            "inputSchema": spawn_asset_schema(),
            "annotations": { "destructiveHint": false, "idempotentHint": true }
        }),
        json!({
            "name": "update_asset",
            "title": "Update Asset Instance",
            "description": "Persist and synchronize a complete transform/state replacement for an active instance.",
            "inputSchema": update_asset_schema(),
            "annotations": { "destructiveHint": false }
        }),
        json!({
            "name": "remove_asset",
            "title": "Remove Asset Instance",
            "description": "Persist and synchronize removal of an asset instance. Repeated removal is safe.",
            "inputSchema": instance_id_schema(),
            "annotations": { "destructiveHint": true, "idempotentHint": true }
        }),
        json!({
            "name": "list_asset_instances",
            "title": "List Asset Instances",
            "description": "List active authoritative asset instances.",
            "inputSchema": object_schema(json!({}), vec![]),
            "annotations": { "readOnlyHint": true }
        }),
    ]
}

pub fn is_known_tool(name: &str) -> bool {
    matches!(
        name,
        "server_status"
            | "list_players"
            | "get_world_bounds"
            | "list_build_palette"
            | "get_block"
            | "snapshot_region"
            | "set_block"
            | "remove_block"
            | "fill_region"
            | "validate_build_plan"
            | "apply_build_plan"
            | "get_build"
            | "undo_build"
            | "validate_sculpt_spec"
            | "inspect_asset_budget"
            | "publish_asset"
            | "list_assets"
            | "get_asset"
            | "spawn_asset"
            | "update_asset"
            | "remove_asset"
            | "list_asset_instances"
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpHttpResponse {
    pub status: u16,
    pub body: Option<Value>,
}

pub fn handle_http_json_rpc_request(
    body: &[u8],
    tool_sender: Sender<McpToolRequest>,
) -> McpHttpResponse {
    let body: Value = match serde_json::from_slice(body) {
        Ok(body) => body,
        Err(err) => {
            return McpHttpResponse {
                status: 400,
                body: Some(json_rpc_error(
                    Value::Null,
                    -32700,
                    format!("parse error: {err}"),
                )),
            };
        }
    };

    if body.get("id").is_none() {
        return McpHttpResponse {
            status: 202,
            body: None,
        };
    }

    McpHttpResponse {
        status: 200,
        body: Some(handle_json_rpc_request(body, tool_sender)),
    }
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
                "description": "Canonical name returned by list_build_palette; air is not accepted here",
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
                "description": "Canonical name returned by list_build_palette, or air for removal",
            },
        }),
        vec![
            "min_x", "min_y", "min_z", "max_x", "max_y", "max_z", "block",
        ],
    )
}

fn build_plan_tool_schema() -> Value {
    object_schema(
        json!({
            "plan": {
                "type": "object",
                "description": "BuildPlan v1 document",
                "properties": {
                    "schema_version": { "type": "integer" },
                    "idempotency_key": { "type": "string", "minLength": 1, "maxLength": 128 },
                    "anchor": {
                        "type": "object",
                        "properties": {
                            "x": { "type": "integer" },
                            "y": { "type": "integer" },
                            "z": { "type": "integer" }
                        },
                        "required": ["x", "y", "z"],
                        "additionalProperties": false
                    },
                    "replace_mode": { "type": "string" },
                    "operations": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": MAX_BUILD_OPERATIONS,
                        "items": {
                            "oneOf": [
                                {
                                    "type": "object",
                                    "properties": {
                                        "op": { "const": "set" },
                                        "at": coordinate_array_schema(),
                                        "block": { "type": "string" }
                                    },
                                    "required": ["op", "at", "block"],
                                    "additionalProperties": false
                                },
                                {
                                    "type": "object",
                                    "properties": {
                                        "op": { "const": "fill_box" },
                                        "from": coordinate_array_schema(),
                                        "to": coordinate_array_schema(),
                                        "block": { "type": "string" }
                                    },
                                    "required": ["op", "from", "to", "block"],
                                    "additionalProperties": false
                                }
                            ]
                        }
                    }
                },
                "required": ["schema_version", "idempotency_key", "anchor", "replace_mode", "operations"],
                "additionalProperties": false
            }
        }),
        vec!["plan"],
    )
}

fn coordinate_array_schema() -> Value {
    json!({
        "type": "array",
        "items": { "type": "integer" },
        "minItems": 3,
        "maxItems": 3
    })
}

fn build_id_schema() -> Value {
    object_schema(
        json!({
            "build_id": { "type": "string", "minLength": 1, "maxLength": 128 }
        }),
        vec!["build_id"],
    )
}

fn sculpt_spec_tool_schema() -> Value {
    object_schema(
        json!({
            "spec": {
                "type": "object",
                "description": "SculptSpec v1 document from world-loom-online/specs/sculpt-spec-v1.md"
            }
        }),
        vec!["spec"],
    )
}

fn get_asset_schema() -> Value {
    object_schema(
        json!({
            "asset_id": { "type": "string", "minLength": 1, "maxLength": 64 },
            "version": { "type": "integer", "minimum": 1, "maximum": 65535 }
        }),
        vec!["asset_id"],
    )
}

fn vector3_number_schema() -> Value {
    json!({
        "type": "array",
        "items": { "type": "number" },
        "minItems": 3,
        "maxItems": 3
    })
}

fn spawn_asset_schema() -> Value {
    object_schema(
        json!({
            "asset_id": { "type": "string", "minLength": 1, "maxLength": 64 },
            "version": { "type": "integer", "minimum": 1, "maximum": 65535 },
            "idempotency_key": { "type": "string", "minLength": 1, "maxLength": 128 },
            "position": vector3_number_schema(),
            "rotation_degrees": vector3_number_schema(),
            "scale": vector3_number_schema(),
            "interaction_state": { "type": "object", "maxProperties": 32 }
        }),
        vec![
            "asset_id",
            "version",
            "idempotency_key",
            "position",
            "rotation_degrees",
            "scale",
        ],
    )
}

fn update_asset_schema() -> Value {
    object_schema(
        json!({
            "instance_id": { "type": "string", "minLength": 1, "maxLength": 128 },
            "position": vector3_number_schema(),
            "rotation_degrees": vector3_number_schema(),
            "scale": vector3_number_schema(),
            "interaction_state": { "type": "object", "maxProperties": 32 }
        }),
        vec!["instance_id", "position", "rotation_degrees", "scale"],
    )
}

fn instance_id_schema() -> Value {
    object_schema(
        json!({
            "instance_id": { "type": "string", "minLength": 1, "maxLength": 128 }
        }),
        vec!["instance_id"],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_list_contains_world_and_build_plan_tools() {
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
                "list_build_palette",
                "get_block",
                "snapshot_region",
                "set_block",
                "remove_block",
                "fill_region",
                "validate_build_plan",
                "apply_build_plan",
                "get_build",
                "undo_build",
                "validate_sculpt_spec",
                "inspect_asset_budget",
                "publish_asset",
                "list_assets",
                "get_asset",
                "spawn_asset",
                "update_asset",
                "remove_asset",
                "list_asset_instances",
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
        assert_eq!(
            parse_block_name("RED_CONCRETE"),
            Some(BlockState::RED_CONCRETE)
        );
        assert_eq!(
            parse_block_name("diamond_block"),
            Some(BlockState::DIAMOND_BLOCK)
        );
        assert_eq!(parse_block_name("sand"), None);
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
            22
        );
    }

    #[test]
    fn validation_and_lookup_tools_are_annotated_read_only() {
        for name in [
            "list_build_palette",
            "validate_build_plan",
            "get_build",
            "validate_sculpt_spec",
            "inspect_asset_budget",
            "list_assets",
            "get_asset",
            "list_asset_instances",
        ] {
            let tool = tool_definitions()
                .into_iter()
                .find(|tool| tool["name"] == name)
                .expect("tool should be listed");
            assert_eq!(tool["annotations"]["readOnlyHint"], true);
        }
    }

    #[test]
    fn direct_edit_schemas_defer_to_the_runtime_palette() {
        for name in ["set_block", "fill_region"] {
            let tool = tool_definitions()
                .into_iter()
                .find(|tool| tool["name"] == name)
                .expect("tool should be listed");
            let block = &tool["inputSchema"]["properties"]["block"];
            assert_eq!(block["type"], "string");
            assert!(block.get("enum").is_none());
            assert!(block["description"]
                .as_str()
                .expect("block description")
                .contains("list_build_palette"));
        }
    }
}
