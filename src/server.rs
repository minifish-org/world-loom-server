use std::time::Instant;

use valence::interact_block::InteractBlockEvent;
use valence::inventory::HeldItem;
use valence::keepalive::Ping;
use valence::prelude::*;
use valence::spawn::IsFlat;

use crate::mcp::{
    block_json, block_state_name, required_block_pos, required_block_state, required_region,
    McpRuntime, McpToolRequest, MAX_FILL_BLOCKS, MAX_SNAPSHOT_BLOCKS,
};
use crate::persistence::{PersistenceRuntime, SavedBlockOverride};
use crate::world_command::{
    execute_world_command, CommandContext, WorldBounds, WorldCommand, GROUND_Y, HOTBAR_BLOCKS,
    SPAWN_FEET_Y, WORLD_BOUNDS,
};

const TELEMETRY_INTERVAL_TICKS: i64 = 20;
const MAX_MCP_REQUESTS_PER_TICK: usize = 64;

#[derive(Resource, Debug, Clone, Copy)]
struct WorldRules {
    bounds: WorldBounds,
}

#[derive(Debug, Clone, PartialEq)]
struct PlayerTelemetry {
    username: String,
    ping_ms: i32,
}

#[derive(Debug)]
struct TelemetrySample {
    tick: i64,
    instant: Instant,
}

pub fn run() {
    let persistence = PersistenceRuntime::open_default()
        .unwrap_or_else(|err| panic!("failed to initialize SQLite persistence: {err}"));
    println!(
        "[world-loom] SQLite save path={} loaded_block_overrides={}",
        persistence.db_path().display(),
        persistence.loaded_overrides().len()
    );
    let mcp = McpRuntime::start_default()
        .unwrap_or_else(|err| panic!("failed to start local MCP server: {err}"));
    println!("[world-loom] MCP endpoint=http://{}/mcp", mcp.addr());

    App::new()
        .insert_resource(NetworkSettings {
            connection_mode: ConnectionMode::Offline,
            ..Default::default()
        })
        .insert_resource(WorldRules {
            bounds: WORLD_BOUNDS,
        })
        .insert_resource(persistence)
        .insert_resource(mcp)
        .add_plugins(DefaultPlugins)
        .add_systems(Startup, setup_world)
        .add_systems(
            Update,
            (
                init_clients,
                despawn_disconnected_clients,
                handle_player_digging,
                handle_player_placement,
                handle_mcp_requests,
                update_debug_telemetry,
            ),
        )
        .run();
}

fn setup_world(
    mut commands: Commands,
    server: Res<Server>,
    dimensions: Res<DimensionTypeRegistry>,
    biomes: Res<BiomeRegistry>,
    rules: Res<WorldRules>,
    persistence: Res<PersistenceRuntime>,
) {
    let mut layer = LayerBundle::new(ident!("overworld"), &dimensions, &biomes, &server);
    let bounds = rules.bounds;

    for chunk_z in bounds.min_z.div_euclid(16)..=bounds.max_z.div_euclid(16) {
        for chunk_x in bounds.min_x.div_euclid(16)..=bounds.max_x.div_euclid(16) {
            layer
                .chunk
                .insert_chunk([chunk_x, chunk_z], UnloadedChunk::new());
        }
    }

    for z in bounds.min_z..=bounds.max_z {
        for x in bounds.min_x..=bounds.max_x {
            layer
                .chunk
                .set_block([x, GROUND_Y - 4, z], BlockState::BEDROCK);
            layer
                .chunk
                .set_block([x, GROUND_Y - 3, z], BlockState::STONE);
            layer
                .chunk
                .set_block([x, GROUND_Y - 2, z], BlockState::DIRT);
            layer
                .chunk
                .set_block([x, GROUND_Y - 1, z], BlockState::DIRT);
            layer
                .chunk
                .set_block([x, GROUND_Y, z], BlockState::GRASS_BLOCK);
        }
    }

    let restored = apply_saved_block_overrides(&mut layer.chunk, persistence.loaded_overrides());
    if restored > 0 {
        println!(
            "[world-loom] restored {restored} saved block overrides from {}",
            persistence.db_path().display()
        );
    }

    commands.spawn(layer);
}

fn apply_saved_block_overrides(layer: &mut ChunkLayer, overrides: &[SavedBlockOverride]) -> usize {
    let mut restored = 0;

    for saved in overrides {
        if layer.set_block(saved.position, saved.block).is_some() {
            restored += 1;
        } else {
            eprintln!(
                "[world-loom] skipped saved block override outside loaded chunks: {:?}",
                saved.position
            );
        }
    }

    restored
}

#[allow(clippy::type_complexity)]
fn init_clients(
    mut clients: Query<
        (
            &mut Client,
            &mut EntityLayerId,
            &mut VisibleChunkLayer,
            &mut VisibleEntityLayers,
            &mut Position,
            &mut GameMode,
            &mut Inventory,
            &mut IsFlat,
        ),
        Added<Client>,
    >,
    layers: Query<Entity, (With<ChunkLayer>, With<EntityLayer>)>,
) {
    for (
        mut client,
        mut layer_id,
        mut visible_chunk_layer,
        mut visible_entity_layers,
        mut pos,
        mut game_mode,
        mut inventory,
        mut is_flat,
    ) in &mut clients
    {
        let layer = layers.single();

        layer_id.0 = layer;
        visible_chunk_layer.0 = layer;
        visible_entity_layers.0.insert(layer);
        pos.set([0.5, SPAWN_FEET_Y as f64, 0.5]);
        *game_mode = GameMode::Creative;
        is_flat.0 = true;
        give_hotbar_blocks(&mut inventory);

        client.send_chat_message(
            "World Loom M6: shared server-backed creative world with local MCP tools and SQLite persistence. Bounds are 128x128 blocks."
                .into_text(),
        );
    }
}

fn give_hotbar_blocks(inventory: &mut Inventory) {
    for (offset, item) in HOTBAR_BLOCKS.iter().copied().enumerate() {
        inventory.set_slot(36 + offset as u16, Some(ItemStack::new(item, 64, None)));
    }
}

fn handle_player_digging(
    mut clients: Query<(&GameMode, &mut Client)>,
    mut layers: Query<&mut ChunkLayer>,
    rules: Res<WorldRules>,
    persistence: Res<PersistenceRuntime>,
    mut events: EventReader<DiggingEvent>,
) {
    let mut layer = layers.single_mut();

    for event in events.iter() {
        let Ok((game_mode, mut client)) = clients.get_mut(event.client) else {
            continue;
        };

        if *game_mode != GameMode::Creative || event.state != DiggingState::Start {
            continue;
        }

        let command = WorldCommand::RemoveBlock {
            position: event.position,
        };
        let context = CommandContext::for_existing_block(block_state_at(&layer, event.position));

        match execute_world_command(&mut layer, rules.bounds, command, context) {
            Ok(()) => persistence.queue_world_command(command, BlockState::AIR),
            Err(err) => {
                client.send_chat_message(format!("WorldCommand rejected remove_block: {err}"));
            }
        }
    }
}

fn handle_player_placement(
    mut clients: Query<(&mut Inventory, &GameMode, &HeldItem, &mut Client)>,
    mut layers: Query<&mut ChunkLayer>,
    rules: Res<WorldRules>,
    persistence: Res<PersistenceRuntime>,
    mut events: EventReader<InteractBlockEvent>,
) {
    let mut layer = layers.single_mut();

    for event in events.iter() {
        let Ok((inventory, game_mode, held, mut client)) = clients.get_mut(event.client) else {
            continue;
        };

        if event.hand != Hand::Main || *game_mode != GameMode::Creative {
            continue;
        }

        let slot_id = held.slot();
        let Some(stack) = inventory.slot(slot_id) else {
            continue;
        };

        let Some(block_kind) = BlockKind::from_item_kind(stack.item) else {
            continue;
        };

        let target = event.position.get_in_direction(event.face);
        let command = WorldCommand::SetBlock {
            position: target,
            block: block_kind.to_state(),
        };
        let context = CommandContext::for_placement(
            block_state_at(&layer, target),
            block_state_at(&layer, event.position),
            event.head_inside_block,
        );

        let final_block = block_kind.to_state();
        match execute_world_command(&mut layer, rules.bounds, command, context) {
            Ok(()) => persistence.queue_world_command(command, final_block),
            Err(err) => {
                client.send_chat_message(format!("WorldCommand rejected set_block: {err}"));
            }
        }
    }
}

fn block_state_at(layer: &ChunkLayer, position: BlockPos) -> BlockState {
    layer
        .block(position)
        .map(|block| block.state)
        .unwrap_or(BlockState::AIR)
}

fn handle_mcp_requests(
    mcp: Res<McpRuntime>,
    server: Res<Server>,
    mut layers: Query<&mut ChunkLayer>,
    rules: Res<WorldRules>,
    persistence: Res<PersistenceRuntime>,
    clients: Query<(&Username, &Position, Option<&Ping>)>,
) {
    for _ in 0..MAX_MCP_REQUESTS_PER_TICK {
        let Some(request) = mcp.try_recv() else {
            return;
        };

        let result = {
            let mut layer = layers.single_mut();
            handle_mcp_tool(
                &request,
                &server,
                &mut layer,
                rules.bounds,
                &persistence,
                &clients,
                mcp.addr().to_string(),
            )
        };
        request.respond(result);
    }
}

fn handle_mcp_tool(
    request: &McpToolRequest,
    server: &Server,
    layer: &mut ChunkLayer,
    bounds: WorldBounds,
    persistence: &PersistenceRuntime,
    clients: &Query<(&Username, &Position, Option<&Ping>)>,
    mcp_addr: String,
) -> Result<serde_json::Value, String> {
    match request.name.as_str() {
        "server_status" => Ok(serde_json::json!({
            "tick": server.current_tick(),
            "connected_players": clients.iter().count(),
            "world_bounds": world_bounds_json(bounds),
            "database_path": persistence.db_path().display().to_string(),
            "mcp_endpoint": format!("http://{mcp_addr}/mcp"),
        })),
        "list_players" => Ok(serde_json::json!({
            "players": player_list_json(clients),
        })),
        "get_world_bounds" => Ok(world_bounds_json(bounds)),
        "get_block" => {
            let position = required_block_pos(&request.arguments)?;
            ensure_in_bounds(bounds, position)?;
            Ok(block_json(position, block_state_at(layer, position)))
        }
        "snapshot_region" => {
            let region = required_region(&request.arguments, MAX_SNAPSHOT_BLOCKS)?;
            ensure_region_in_bounds(bounds, region.min, region.max)?;
            let mut blocks = Vec::new();
            for position in region_positions(region.min, region.max, false) {
                blocks.push(block_json(position, block_state_at(layer, position)));
            }
            Ok(serde_json::json!({
                "min": position_json(region.min),
                "max": position_json(region.max),
                "volume": region.volume(),
                "blocks": blocks,
            }))
        }
        "set_block" => {
            let position = required_block_pos(&request.arguments)?;
            ensure_in_bounds(bounds, position)?;
            let block = required_block_state(&request.arguments)?;
            if block == BlockState::AIR {
                return Err("set_block does not accept air; use remove_block".to_string());
            }
            set_block_via_world_command(layer, bounds, persistence, position, block)?;
            Ok(serde_json::json!({
                "edited": 1,
                "block": block_json(position, block_state_at(layer, position)),
            }))
        }
        "remove_block" => {
            let position = required_block_pos(&request.arguments)?;
            ensure_in_bounds(bounds, position)?;
            remove_block_via_world_command(layer, bounds, persistence, position)?;
            Ok(serde_json::json!({
                "edited": 1,
                "block": block_json(position, block_state_at(layer, position)),
            }))
        }
        "fill_region" => {
            let region = required_region(&request.arguments, MAX_FILL_BLOCKS)?;
            ensure_region_in_bounds(bounds, region.min, region.max)?;
            let block = required_block_state(&request.arguments)?;
            fill_region_via_world_command(layer, bounds, persistence, region.min, region.max, block)
        }
        _ => Err(format!("unknown MCP tool `{}`", request.name)),
    }
}

fn set_block_via_world_command(
    layer: &mut ChunkLayer,
    bounds: WorldBounds,
    persistence: &PersistenceRuntime,
    position: BlockPos,
    block: BlockState,
) -> Result<(), String> {
    let command = WorldCommand::SetBlock { position, block };
    let context = CommandContext::for_placement(
        block_state_at(layer, position),
        placement_anchor_block(layer, position),
        false,
    );

    execute_world_command(layer, bounds, command, context).map_err(|err| err.to_string())?;
    persistence.queue_world_command(command, block);
    Ok(())
}

fn remove_block_via_world_command(
    layer: &mut ChunkLayer,
    bounds: WorldBounds,
    persistence: &PersistenceRuntime,
    position: BlockPos,
) -> Result<(), String> {
    let command = WorldCommand::RemoveBlock { position };
    let context = CommandContext::for_existing_block(block_state_at(layer, position));

    execute_world_command(layer, bounds, command, context).map_err(|err| err.to_string())?;
    persistence.queue_world_command(command, BlockState::AIR);
    Ok(())
}

fn fill_region_via_world_command(
    layer: &mut ChunkLayer,
    bounds: WorldBounds,
    persistence: &PersistenceRuntime,
    min: BlockPos,
    max: BlockPos,
    block: BlockState,
) -> Result<serde_json::Value, String> {
    let mut edited = 0;
    let mut skipped = 0;
    let remove = block == BlockState::AIR;

    for position in region_positions(min, max, remove) {
        if remove {
            if block_state_at(layer, position) == BlockState::AIR {
                skipped += 1;
                continue;
            }
            remove_block_via_world_command(layer, bounds, persistence, position)
                .map_err(|err| format!("fill_region failed at {position:?}: {err}"))?;
        } else {
            set_block_via_world_command(layer, bounds, persistence, position, block)
                .map_err(|err| format!("fill_region failed at {position:?}: {err}"))?;
        }
        edited += 1;
    }

    Ok(serde_json::json!({
        "edited": edited,
        "skipped": skipped,
        "min": position_json(min),
        "max": position_json(max),
        "block": block_state_name(block),
    }))
}

fn placement_anchor_block(layer: &ChunkLayer, position: BlockPos) -> BlockState {
    [
        BlockPos::new(position.x, position.y - 1, position.z),
        BlockPos::new(position.x, position.y + 1, position.z),
        BlockPos::new(position.x - 1, position.y, position.z),
        BlockPos::new(position.x + 1, position.y, position.z),
        BlockPos::new(position.x, position.y, position.z - 1),
        BlockPos::new(position.x, position.y, position.z + 1),
    ]
    .into_iter()
    .map(|anchor| block_state_at(layer, anchor))
    .find(|block| *block != BlockState::AIR)
    .unwrap_or(BlockState::AIR)
}

fn ensure_in_bounds(bounds: WorldBounds, position: BlockPos) -> Result<(), String> {
    if bounds.contains(position) {
        Ok(())
    } else {
        Err(format!("position {position:?} is outside world bounds"))
    }
}

fn ensure_region_in_bounds(
    bounds: WorldBounds,
    min: BlockPos,
    max: BlockPos,
) -> Result<(), String> {
    ensure_in_bounds(bounds, min)?;
    ensure_in_bounds(bounds, max)
}

fn region_positions(min: BlockPos, max: BlockPos, reverse_y: bool) -> Vec<BlockPos> {
    let y_values: Vec<i32> = if reverse_y {
        (min.y..=max.y).rev().collect()
    } else {
        (min.y..=max.y).collect()
    };

    let mut positions = Vec::new();
    for y in y_values {
        for x in min.x..=max.x {
            for z in min.z..=max.z {
                positions.push(BlockPos::new(x, y, z));
            }
        }
    }
    positions
}

fn world_bounds_json(bounds: WorldBounds) -> serde_json::Value {
    serde_json::json!({
        "min_x": bounds.min_x,
        "max_x": bounds.max_x,
        "min_y": bounds.min_y,
        "max_y": bounds.max_y,
        "min_z": bounds.min_z,
        "max_z": bounds.max_z,
    })
}

fn position_json(position: BlockPos) -> serde_json::Value {
    serde_json::json!({
        "x": position.x,
        "y": position.y,
        "z": position.z,
    })
}

fn player_list_json(
    clients: &Query<(&Username, &Position, Option<&Ping>)>,
) -> Vec<serde_json::Value> {
    clients
        .iter()
        .map(|(username, position, ping)| {
            let pos = position.get();
            serde_json::json!({
                "username": username.0,
                "position": {
                    "x": pos.x,
                    "y": pos.y,
                    "z": pos.z,
                },
                "ping_ms": ping.map(|ping| ping.0),
            })
        })
        .collect()
}

fn update_debug_telemetry(
    server: Res<Server>,
    mut player_list: ResMut<PlayerList>,
    mut clients: Query<(&mut Client, &Username, &Ping)>,
    mut last_report: Local<Option<TelemetrySample>>,
) {
    let tick = server.current_tick();
    if last_report
        .as_ref()
        .is_some_and(|sample| sample.tick == tick)
        || tick % TELEMETRY_INTERVAL_TICKS != 0
    {
        return;
    }

    let now = Instant::now();
    let tick_delta_ms = last_report
        .as_ref()
        .map(|sample| {
            let elapsed_ms = now.duration_since(sample.instant).as_secs_f64() * 1000.0;
            let elapsed_ticks = (tick - sample.tick).max(1) as f64;
            elapsed_ms / elapsed_ticks
        })
        .unwrap_or(0.0);
    *last_report = Some(TelemetrySample { tick, instant: now });

    let mut players = Vec::new();

    for (_, username, ping) in clients.iter_mut() {
        players.push(PlayerTelemetry {
            username: username.0.clone(),
            ping_ms: ping.0,
        });
    }

    let header = telemetry_header(tick, players.len());
    let footer = telemetry_footer(tick_delta_ms, &players);
    let action_bar = telemetry_action_bar(tick, tick_delta_ms, players.len());

    player_list.set_header(header);
    player_list.set_footer(footer);

    for (mut client, _, _) in clients.iter_mut() {
        client.send_action_bar_message(action_bar.clone());
    }

    println!(
        "[world-loom] tick={} tick_delta_ms={:.2} players={} {}",
        tick,
        tick_delta_ms,
        players.len(),
        format_player_pings(&players)
    );
}

fn telemetry_header(tick: i64, player_count: usize) -> String {
    format!("World Loom M6 | tick {tick} | players {player_count}")
}

fn telemetry_footer(tick_delta_ms: f64, players: &[PlayerTelemetry]) -> String {
    format!(
        "tick delta {:.2}ms | {}",
        tick_delta_ms,
        format_player_pings(players)
    )
}

fn telemetry_action_bar(tick: i64, tick_delta_ms: f64, player_count: usize) -> String {
    format!("M6 tick {tick} | {tick_delta_ms:.2}ms | {player_count} players")
}

fn format_player_pings(players: &[PlayerTelemetry]) -> String {
    if players.is_empty() {
        return "no connected players".to_string();
    }

    players
        .iter()
        .map(|player| {
            let ping = if player.ping_ms >= 0 {
                format!("{}ms", player.ping_ms)
            } else {
                "pending".to_string()
            };
            format!("{}={ping}", player.username)
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn telemetry_formats_player_count_and_ping() {
        let players = [
            PlayerTelemetry {
                username: "loom_a".to_string(),
                ping_ms: 42,
            },
            PlayerTelemetry {
                username: "loom_b".to_string(),
                ping_ms: -1,
            },
        ];

        assert_eq!(
            telemetry_header(80, players.len()),
            "World Loom M6 | tick 80 | players 2"
        );
        assert_eq!(
            telemetry_footer(3.25, &players),
            "tick delta 3.25ms | loom_a=42ms, loom_b=pending"
        );
        assert_eq!(
            telemetry_action_bar(80, 3.25, players.len()),
            "M6 tick 80 | 3.25ms | 2 players"
        );
    }
}
