use std::collections::{BTreeSet, VecDeque};
use std::env;
use std::time::Instant;

use valence::interact_block::InteractBlockEvent;
use valence::inventory::HeldItem;
use valence::keepalive::Ping;
use valence::prelude::*;
use valence::spawn::IsFlat;

use crate::bridge::BridgeRuntime;
use crate::mcp::{
    block_json, block_state_name, required_block_pos, required_block_state, required_region,
    McpRuntime, McpToolRequest, MAX_FILL_BLOCKS, MAX_SNAPSHOT_BLOCKS,
};
use crate::persistence::{PersistenceRuntime, PersistenceStatsSnapshot, StoredChunk};
use crate::world_command::{
    execute_world_command, ChunkColumn, CommandContext, WorldBounds, WorldCommand, GROUND_Y,
    HOTBAR_BLOCKS, SPAWN_FEET_Y, WORLD_BOUNDS,
};

const TELEMETRY_INTERVAL_TICKS: i64 = 20;
const TELEMETRY_WINDOW_SAMPLES: usize = 30;
const MAX_MCP_REQUESTS_PER_TICK: usize = 64;
pub const VIEW_DISTANCE_ENV: &str = "WORLD_LOOM_VIEW_DISTANCE_CHUNKS";
const DEFAULT_VIEW_DISTANCE_CHUNKS: u8 = 6;
const MAX_VIEW_DISTANCE_CHUNKS: u8 = 12;
const CHUNK_LOAD_MARGIN: i32 = 1;

#[derive(Resource, Debug, Clone, Copy)]
struct WorldRules {
    bounds: WorldBounds,
}

#[derive(Resource, Debug, Default)]
struct ChunkLifecycle {
    loaded: BTreeSet<ChunkColumn>,
    generated_chunks: u64,
    storage_loaded_chunks: u64,
    unloaded_chunks: u64,
    dirty_chunks_marked: u64,
}

impl ChunkLifecycle {
    fn mark_loaded_generated(&mut self, column: ChunkColumn) {
        if self.loaded.insert(column) {
            self.generated_chunks += 1;
        }
    }

    fn mark_loaded_from_storage(&mut self, column: ChunkColumn) {
        if self.loaded.insert(column) {
            self.generated_chunks += 1;
            self.storage_loaded_chunks += 1;
        }
    }

    fn mark_unloaded(&mut self, column: ChunkColumn) {
        if self.loaded.remove(&column) {
            self.unloaded_chunks += 1;
        }
    }

    fn mark_dirty(&mut self, column: ChunkColumn) {
        self.dirty_chunks_marked += 1;
        self.loaded.insert(column);
    }
}

#[derive(Resource, Debug, Clone, Copy)]
struct InterestConfig {
    view_distance_chunks: u8,
}

impl InterestConfig {
    fn from_env() -> Self {
        Self {
            view_distance_chunks: bounded_view_distance(
                env::var(VIEW_DISTANCE_ENV).ok().as_deref(),
            ),
        }
    }
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

#[derive(Resource, Debug)]
struct ServerTelemetry {
    snapshot: TelemetrySnapshot,
    mspt_window: VecDeque<f64>,
}

impl Default for ServerTelemetry {
    fn default() -> Self {
        Self {
            snapshot: TelemetrySnapshot::default(),
            mspt_window: VecDeque::with_capacity(TELEMETRY_WINDOW_SAMPLES),
        }
    }
}

impl ServerTelemetry {
    fn record(
        &mut self,
        tick: i64,
        last_mspt: f64,
        players: Vec<PlayerTelemetry>,
        loaded_chunks: usize,
        view_distance_chunks: u8,
        world_chunk_columns: usize,
    ) {
        if self.mspt_window.len() == TELEMETRY_WINDOW_SAMPLES {
            self.mspt_window.pop_front();
        }
        self.mspt_window.push_back(last_mspt);

        let avg_mspt = average_mspt(&self.mspt_window);
        let max_mspt = self.mspt_window.iter().copied().fold(0.0_f64, f64::max);

        self.snapshot = TelemetrySnapshot {
            tick,
            last_mspt,
            avg_mspt,
            max_mspt,
            players,
            loaded_chunks,
            view_distance_chunks,
            interest_chunks_per_player: interest_chunk_capacity(view_distance_chunks),
            world_chunk_columns,
            window_samples: self.mspt_window.len(),
        };
    }

    fn snapshot(&self) -> &TelemetrySnapshot {
        &self.snapshot
    }
}

#[derive(Debug, Clone, PartialEq)]
struct TelemetrySnapshot {
    tick: i64,
    last_mspt: f64,
    avg_mspt: f64,
    max_mspt: f64,
    players: Vec<PlayerTelemetry>,
    loaded_chunks: usize,
    view_distance_chunks: u8,
    interest_chunks_per_player: usize,
    world_chunk_columns: usize,
    window_samples: usize,
}

impl Default for TelemetrySnapshot {
    fn default() -> Self {
        Self {
            tick: 0,
            last_mspt: 0.0,
            avg_mspt: 0.0,
            max_mspt: 0.0,
            players: Vec::new(),
            loaded_chunks: 0,
            view_distance_chunks: DEFAULT_VIEW_DISTANCE_CHUNKS,
            interest_chunks_per_player: interest_chunk_capacity(DEFAULT_VIEW_DISTANCE_CHUNKS),
            world_chunk_columns: WORLD_BOUNDS.loaded_chunk_columns(),
            window_samples: 0,
        }
    }
}

pub fn run() {
    let persistence = PersistenceRuntime::open_default()
        .unwrap_or_else(|err| panic!("failed to initialize SQLite persistence: {err}"));
    let persistence_stats = persistence.stats_snapshot();
    println!(
        "[world-loom] storage={} schema_version={} save_format_version={} path={} legacy_block_overrides={}",
        persistence_stats.storage_backend,
        persistence_stats.schema_version,
        persistence_stats.save_format_version,
        persistence.db_path().display(),
        persistence.loaded_legacy_overrides()
    );
    println!(
        "[world-loom] region chunk path={}",
        persistence.region_dir().display()
    );
    let mcp = McpRuntime::start_default()
        .unwrap_or_else(|err| panic!("failed to start local MCP server: {err}"));
    println!("[world-loom] MCP endpoint=http://{}/mcp", mcp.addr());
    let bridge = BridgeRuntime::start_default()
        .unwrap_or_else(|err| panic!("failed to start browser bridge: {err}"));
    println!(
        "[world-loom] browser bridge endpoint=http://{}",
        bridge.addr()
    );
    let bridge_config = bridge.config();
    println!(
        "[world-loom] bridge backpressure tcp_read_buffer={} ws_queue_capacity={} max_pending_connections={}",
        bridge_config.tcp_read_buffer_bytes,
        bridge_config.ws_queue_capacity,
        bridge_config.max_pending_connections
    );
    let interest = InterestConfig::from_env();
    println!(
        "[world-loom] chunk interest view_distance_chunks={} loaded_chunk_columns={}",
        interest.view_distance_chunks,
        WORLD_BOUNDS.loaded_chunk_columns()
    );

    App::new()
        .insert_resource(NetworkSettings {
            connection_mode: ConnectionMode::Offline,
            ..Default::default()
        })
        .insert_resource(WorldRules {
            bounds: WORLD_BOUNDS,
        })
        .insert_resource(interest)
        .insert_resource(ChunkLifecycle::default())
        .insert_resource(ServerTelemetry::default())
        .insert_resource(persistence)
        .insert_resource(mcp)
        .insert_resource(bridge)
        .add_plugins(DefaultPlugins)
        .add_systems(Startup, setup_world)
        .add_systems(
            Update,
            (
                init_clients,
                manage_world_chunks,
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
) {
    let layer = LayerBundle::new(ident!("overworld"), &dimensions, &biomes, &server);
    commands.spawn(layer);
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
            &mut ViewDistance,
        ),
        Added<Client>,
    >,
    layers: Query<Entity, (With<ChunkLayer>, With<EntityLayer>)>,
    interest: Res<InterestConfig>,
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
        mut view_distance,
    ) in &mut clients
    {
        let layer = layers.single();

        layer_id.0 = layer;
        visible_chunk_layer.0 = layer;
        visible_entity_layers.0.insert(layer);
        pos.set([0.5, SPAWN_FEET_Y as f64, 0.5]);
        *game_mode = GameMode::Creative;
        is_flat.0 = true;
        view_distance.set(interest.view_distance_chunks);
        give_hotbar_blocks(&mut inventory);

        client.send_chat_message(format!(
            "World Loom V3: chunk lifecycle and region storage enabled. Bounds are 512x512 blocks; view distance is {} chunks.",
            interest.view_distance_chunks
        ));
    }
}

fn give_hotbar_blocks(inventory: &mut Inventory) {
    for (offset, item) in HOTBAR_BLOCKS.iter().copied().enumerate() {
        inventory.set_slot(36 + offset as u16, Some(ItemStack::new(item, 64, None)));
    }
}

fn manage_world_chunks(
    mut layers: Query<&mut ChunkLayer>,
    clients: Query<&Position, With<Client>>,
    rules: Res<WorldRules>,
    interest: Res<InterestConfig>,
    persistence: Res<PersistenceRuntime>,
    mut lifecycle: ResMut<ChunkLifecycle>,
) {
    let mut layer = layers.single_mut();
    if clients.is_empty() {
        return;
    }

    let desired = desired_chunks_for_players(&clients, rules.bounds, interest.view_distance_chunks);

    for column in &desired {
        if let Err(err) = load_or_generate_chunk(
            &mut layer,
            rules.bounds,
            &persistence,
            &mut lifecycle,
            *column,
        ) {
            eprintln!("[world-loom] failed to load chunk {column:?}: {err}");
        }
    }

    let loaded_columns = layer
        .chunks()
        .map(|(position, _)| ChunkColumn::from_chunk_pos(position))
        .collect::<Vec<_>>();

    for column in loaded_columns {
        if !desired.contains(&column) {
            layer.remove_chunk(column.to_chunk_pos());
            lifecycle.mark_unloaded(column);
        }
    }
}

fn desired_chunks_for_players(
    clients: &Query<&Position, With<Client>>,
    bounds: WorldBounds,
    view_distance_chunks: u8,
) -> BTreeSet<ChunkColumn> {
    let radius = i32::from(view_distance_chunks) + CHUNK_LOAD_MARGIN;
    let mut desired = BTreeSet::new();

    for position in clients.iter() {
        let center = ChunkColumn::from_chunk_pos(position.to_chunk_pos());
        for z in center.z - radius..=center.z + radius {
            for x in center.x - radius..=center.x + radius {
                let column = ChunkColumn::new(x, z);
                if bounds.contains_chunk(column) {
                    desired.insert(column);
                }
            }
        }
    }

    desired
}

fn load_or_generate_chunk(
    layer: &mut ChunkLayer,
    bounds: WorldBounds,
    persistence: &PersistenceRuntime,
    lifecycle: &mut ChunkLifecycle,
    column: ChunkColumn,
) -> Result<(), String> {
    if !bounds.contains_chunk(column) {
        return Ok(());
    }

    if layer.chunk(column.to_chunk_pos()).is_some() {
        lifecycle.loaded.insert(column);
        return Ok(());
    }

    layer.insert_chunk(column.to_chunk_pos(), UnloadedChunk::new());
    generate_base_chunk(layer, bounds, column);

    let stored = persistence
        .load_chunk(column)
        .map_err(|err| format!("load chunk storage failed: {err}"))?;
    let restored = if let Some(stored) = stored {
        apply_stored_chunk(layer, bounds, &stored)
    } else {
        0
    };

    if restored > 0 {
        lifecycle.mark_loaded_from_storage(column);
    } else {
        lifecycle.mark_loaded_generated(column);
    }

    Ok(())
}

fn generate_base_chunk(layer: &mut ChunkLayer, bounds: WorldBounds, column: ChunkColumn) {
    let min_x = (column.x * 16).max(bounds.min_x);
    let max_x = (column.x * 16 + 15).min(bounds.max_x);
    let min_z = (column.z * 16).max(bounds.min_z);
    let max_z = (column.z * 16 + 15).min(bounds.max_z);

    for z in min_z..=max_z {
        for x in min_x..=max_x {
            layer.set_block([x, GROUND_Y - 4, z], BlockState::BEDROCK);
            layer.set_block([x, GROUND_Y - 3, z], BlockState::STONE);
            layer.set_block([x, GROUND_Y - 2, z], BlockState::DIRT);
            layer.set_block([x, GROUND_Y - 1, z], BlockState::DIRT);
            layer.set_block([x, GROUND_Y, z], BlockState::GRASS_BLOCK);
        }
    }
}

fn apply_stored_chunk(layer: &mut ChunkLayer, bounds: WorldBounds, stored: &StoredChunk) -> usize {
    let mut restored = 0;

    for saved in &stored.blocks {
        if !bounds.contains(saved.position) {
            eprintln!(
                "[world-loom] skipped saved block outside bounds: {:?}",
                saved.position
            );
            continue;
        }

        if layer.set_block(saved.position, saved.block).is_some() {
            restored += 1;
        }
    }

    restored
}

fn ensure_chunk_loaded_for_position(
    layer: &mut ChunkLayer,
    bounds: WorldBounds,
    persistence: &PersistenceRuntime,
    lifecycle: &mut ChunkLifecycle,
    position: BlockPos,
) -> Result<(), String> {
    load_or_generate_chunk(
        layer,
        bounds,
        persistence,
        lifecycle,
        ChunkColumn::from_block_pos(position),
    )
}

fn ensure_neighbor_chunks_loaded(
    layer: &mut ChunkLayer,
    bounds: WorldBounds,
    persistence: &PersistenceRuntime,
    lifecycle: &mut ChunkLifecycle,
    position: BlockPos,
) -> Result<(), String> {
    for position in [
        position,
        BlockPos::new(position.x, position.y - 1, position.z),
        BlockPos::new(position.x, position.y + 1, position.z),
        BlockPos::new(position.x - 1, position.y, position.z),
        BlockPos::new(position.x + 1, position.y, position.z),
        BlockPos::new(position.x, position.y, position.z - 1),
        BlockPos::new(position.x, position.y, position.z + 1),
    ] {
        if bounds.contains(position) {
            ensure_chunk_loaded_for_position(layer, bounds, persistence, lifecycle, position)?;
        }
    }

    Ok(())
}

fn ensure_region_chunks_loaded(
    layer: &mut ChunkLayer,
    bounds: WorldBounds,
    persistence: &PersistenceRuntime,
    lifecycle: &mut ChunkLifecycle,
    min: BlockPos,
    max: BlockPos,
) -> Result<(), String> {
    let min_column = ChunkColumn::from_block_pos(min);
    let max_column = ChunkColumn::from_block_pos(max);
    for z in min_column.z..=max_column.z {
        for x in min_column.x..=max_column.x {
            load_or_generate_chunk(
                layer,
                bounds,
                persistence,
                lifecycle,
                ChunkColumn::new(x, z),
            )?;
        }
    }

    Ok(())
}

fn handle_player_digging(
    mut clients: Query<(&GameMode, &mut Client)>,
    mut layers: Query<&mut ChunkLayer>,
    rules: Res<WorldRules>,
    persistence: Res<PersistenceRuntime>,
    mut lifecycle: ResMut<ChunkLifecycle>,
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
            Ok(()) => {
                persistence.queue_world_command(command, BlockState::AIR);
                lifecycle.mark_dirty(ChunkColumn::from_block_pos(event.position));
            }
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
    mut lifecycle: ResMut<ChunkLifecycle>,
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
            Ok(()) => {
                persistence.queue_world_command(command, final_block);
                lifecycle.mark_dirty(ChunkColumn::from_block_pos(target));
            }
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

#[allow(clippy::too_many_arguments)]
fn handle_mcp_requests(
    mcp: Res<McpRuntime>,
    server: Res<Server>,
    mut layers: Query<&mut ChunkLayer>,
    rules: Res<WorldRules>,
    persistence: Res<PersistenceRuntime>,
    telemetry: Res<ServerTelemetry>,
    interest: Res<InterestConfig>,
    bridge: Res<BridgeRuntime>,
    mut lifecycle: ResMut<ChunkLifecycle>,
    clients: Query<(&Username, &Position, Option<&Ping>)>,
) {
    for _ in 0..MAX_MCP_REQUESTS_PER_TICK {
        let Some(request) = mcp.try_recv() else {
            return;
        };

        let result = {
            let mut layer = layers.single_mut();
            let context = McpToolContext {
                server: &server,
                bounds: rules.bounds,
                persistence: &persistence,
                telemetry: telemetry.snapshot(),
                interest: &interest,
                bridge: &bridge,
                lifecycle: &mut lifecycle,
                mcp_addr: mcp.addr().to_string(),
                players: player_list_json(&clients),
            };
            handle_mcp_tool(&request, &mut layer, context)
        };
        request.respond(result);
    }
}

struct McpToolContext<'a> {
    server: &'a Server,
    bounds: WorldBounds,
    persistence: &'a PersistenceRuntime,
    telemetry: &'a TelemetrySnapshot,
    interest: &'a InterestConfig,
    bridge: &'a BridgeRuntime,
    lifecycle: &'a mut ChunkLifecycle,
    mcp_addr: String,
    players: Vec<serde_json::Value>,
}

fn handle_mcp_tool(
    request: &McpToolRequest,
    layer: &mut ChunkLayer,
    context: McpToolContext<'_>,
) -> Result<serde_json::Value, String> {
    match request.name.as_str() {
        "server_status" => {
            let storage = context.persistence.stats_snapshot();
            let loaded_chunks = layer.chunks().count();
            Ok(serde_json::json!({
                "tick": context.server.current_tick(),
                "connected_players": context.players.len(),
                "world_bounds": world_bounds_json(context.bounds),
                "database_path": context.persistence.db_path().display().to_string(),
                "mcp_endpoint": format!("http://{}/mcp", context.mcp_addr),
                "performance": performance_json(context.telemetry),
                "network": network_json(
                    context.telemetry,
                    context.interest,
                    loaded_chunks,
                    &context.players,
                ),
                "storage": storage_json(&storage, context.persistence),
                "chunk_lifecycle": chunk_lifecycle_json(context.lifecycle),
                "bridge": bridge_json(context.bridge),
            }))
        }
        "list_players" => Ok(serde_json::json!({
            "players": context.players,
        })),
        "get_world_bounds" => Ok(world_bounds_json(context.bounds)),
        "get_block" => {
            let position = required_block_pos(&request.arguments)?;
            ensure_in_bounds(context.bounds, position)?;
            ensure_chunk_loaded_for_position(
                layer,
                context.bounds,
                context.persistence,
                context.lifecycle,
                position,
            )?;
            Ok(block_json(position, block_state_at(layer, position)))
        }
        "snapshot_region" => {
            let region = required_region(&request.arguments, MAX_SNAPSHOT_BLOCKS)?;
            ensure_region_in_bounds(context.bounds, region.min, region.max)?;
            ensure_region_chunks_loaded(
                layer,
                context.bounds,
                context.persistence,
                context.lifecycle,
                region.min,
                region.max,
            )?;
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
            ensure_in_bounds(context.bounds, position)?;
            ensure_neighbor_chunks_loaded(
                layer,
                context.bounds,
                context.persistence,
                context.lifecycle,
                position,
            )?;
            let block = required_block_state(&request.arguments)?;
            if block == BlockState::AIR {
                return Err("set_block does not accept air; use remove_block".to_string());
            }
            set_block_via_world_command(
                layer,
                context.bounds,
                context.persistence,
                position,
                block,
            )?;
            context
                .lifecycle
                .mark_dirty(ChunkColumn::from_block_pos(position));
            Ok(serde_json::json!({
                "edited": 1,
                "block": block_json(position, block_state_at(layer, position)),
            }))
        }
        "remove_block" => {
            let position = required_block_pos(&request.arguments)?;
            ensure_in_bounds(context.bounds, position)?;
            ensure_chunk_loaded_for_position(
                layer,
                context.bounds,
                context.persistence,
                context.lifecycle,
                position,
            )?;
            remove_block_via_world_command(layer, context.bounds, context.persistence, position)?;
            context
                .lifecycle
                .mark_dirty(ChunkColumn::from_block_pos(position));
            Ok(serde_json::json!({
                "edited": 1,
                "block": block_json(position, block_state_at(layer, position)),
            }))
        }
        "fill_region" => {
            let region = required_region(&request.arguments, MAX_FILL_BLOCKS)?;
            ensure_region_in_bounds(context.bounds, region.min, region.max)?;
            ensure_region_chunks_loaded(
                layer,
                context.bounds,
                context.persistence,
                context.lifecycle,
                region.min,
                region.max,
            )?;
            let block = required_block_state(&request.arguments)?;
            let result = fill_region_via_world_command(
                layer,
                context.bounds,
                context.persistence,
                region.min,
                region.max,
                block,
            )?;
            for position in region_positions(region.min, region.max, block == BlockState::AIR) {
                context
                    .lifecycle
                    .mark_dirty(ChunkColumn::from_block_pos(position));
            }
            Ok(result)
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

fn performance_json(snapshot: &TelemetrySnapshot) -> serde_json::Value {
    serde_json::json!({
        "tick": snapshot.tick,
        "last_mspt": round_ms(snapshot.last_mspt),
        "avg_mspt": round_ms(snapshot.avg_mspt),
        "max_mspt": round_ms(snapshot.max_mspt),
        "window_samples": snapshot.window_samples,
        "telemetry_interval_ticks": TELEMETRY_INTERVAL_TICKS,
    })
}

fn network_json(
    snapshot: &TelemetrySnapshot,
    interest: &InterestConfig,
    loaded_chunks: usize,
    players: &[serde_json::Value],
) -> serde_json::Value {
    serde_json::json!({
        "players": players,
        "view_distance_chunks": interest.view_distance_chunks,
        "interest_chunks_per_player": interest_chunk_capacity(interest.view_distance_chunks),
        "loaded_chunks": loaded_chunks,
        "world_chunk_columns": snapshot.world_chunk_columns,
    })
}

fn storage_json(
    stats: &PersistenceStatsSnapshot,
    persistence: &PersistenceRuntime,
) -> serde_json::Value {
    serde_json::json!({
        "backend": stats.storage_backend,
        "schema_version": stats.schema_version,
        "save_format_version": stats.save_format_version,
        "database_path": persistence.db_path().display().to_string(),
        "region_dir": persistence.region_dir().display().to_string(),
        "loaded_legacy_overrides": stats.loaded_legacy_overrides,
        "pending_edits": stats.pending_edits,
        "flushed_edits": stats.flushed_edits,
        "flush_batches": stats.flush_batches,
        "failed_flushes": stats.failed_flushes,
        "dirty_chunks": stats.dirty_chunks,
    })
}

fn chunk_lifecycle_json(lifecycle: &ChunkLifecycle) -> serde_json::Value {
    serde_json::json!({
        "loaded_chunks": lifecycle.loaded.len(),
        "generated_chunks": lifecycle.generated_chunks,
        "storage_loaded_chunks": lifecycle.storage_loaded_chunks,
        "unloaded_chunks": lifecycle.unloaded_chunks,
        "dirty_chunks_marked": lifecycle.dirty_chunks_marked,
    })
}

fn bridge_json(bridge: &BridgeRuntime) -> serde_json::Value {
    let config = bridge.config();
    serde_json::json!({
        "endpoint": format!("http://{}", bridge.addr()),
        "tcp_read_buffer_bytes": config.tcp_read_buffer_bytes,
        "ws_queue_capacity": config.ws_queue_capacity,
        "max_pending_connections": config.max_pending_connections,
    })
}

#[allow(clippy::too_many_arguments)]
fn update_debug_telemetry(
    server: Res<Server>,
    mut player_list: ResMut<PlayerList>,
    mut clients: Query<(&mut Client, &Username, &Ping)>,
    layers: Query<&ChunkLayer>,
    interest: Res<InterestConfig>,
    rules: Res<WorldRules>,
    mut telemetry: ResMut<ServerTelemetry>,
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

    let loaded_chunks = layers.single().chunks().count();
    telemetry.record(
        tick,
        tick_delta_ms,
        players,
        loaded_chunks,
        interest.view_distance_chunks,
        rules.bounds.loaded_chunk_columns(),
    );
    let snapshot = telemetry.snapshot();

    let header = telemetry_header(snapshot);
    let footer = telemetry_footer(snapshot);
    let action_bar = telemetry_action_bar(snapshot);

    player_list.set_header(header);
    player_list.set_footer(footer);

    for (mut client, _, _) in clients.iter_mut() {
        client.send_action_bar_message(action_bar.clone());
    }

    println!(
        "[world-loom] tick={} tick_delta_ms={:.2} players={} {}",
        snapshot.tick,
        snapshot.last_mspt,
        snapshot.players.len(),
        format_player_pings(&snapshot.players)
    );
}

fn telemetry_header(snapshot: &TelemetrySnapshot) -> String {
    format!(
        "World Loom V3 | tick {} | players {} | view {} chunks",
        snapshot.tick,
        snapshot.players.len(),
        snapshot.view_distance_chunks
    )
}

fn telemetry_footer(snapshot: &TelemetrySnapshot) -> String {
    format!(
        "MSPT last {:.2} avg {:.2} max {:.2} | chunks {}/{} | {}",
        snapshot.last_mspt,
        snapshot.avg_mspt,
        snapshot.max_mspt,
        snapshot.loaded_chunks,
        snapshot.world_chunk_columns,
        format_player_pings(&snapshot.players)
    )
}

fn telemetry_action_bar(snapshot: &TelemetrySnapshot) -> String {
    format!(
        "V3 tick {} | MSPT {:.2}/{:.2} avg | {} players | {} chunks",
        snapshot.tick,
        snapshot.last_mspt,
        snapshot.avg_mspt,
        snapshot.players.len(),
        snapshot.loaded_chunks
    )
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

fn average_mspt(samples: &VecDeque<f64>) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }

    samples.iter().sum::<f64>() / samples.len() as f64
}

fn round_ms(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

fn interest_chunk_capacity(view_distance_chunks: u8) -> usize {
    let diameter = usize::from(view_distance_chunks) * 2 + 1;
    diameter * diameter
}

fn bounded_view_distance(raw: Option<&str>) -> u8 {
    raw.and_then(|value| value.trim().parse::<u8>().ok())
        .unwrap_or(DEFAULT_VIEW_DISTANCE_CHUNKS)
        .clamp(2, MAX_VIEW_DISTANCE_CHUNKS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn telemetry_formats_player_count_and_ping() {
        let players = vec![
            PlayerTelemetry {
                username: "loom_a".to_string(),
                ping_ms: 42,
            },
            PlayerTelemetry {
                username: "loom_b".to_string(),
                ping_ms: -1,
            },
        ];
        let snapshot = TelemetrySnapshot {
            tick: 80,
            last_mspt: 3.25,
            avg_mspt: 3.0,
            max_mspt: 4.5,
            players,
            loaded_chunks: 64,
            view_distance_chunks: 6,
            interest_chunks_per_player: interest_chunk_capacity(6),
            world_chunk_columns: 64,
            window_samples: 2,
        };

        assert_eq!(
            telemetry_header(&snapshot),
            "World Loom V3 | tick 80 | players 2 | view 6 chunks"
        );
        assert_eq!(
            telemetry_footer(&snapshot),
            "MSPT last 3.25 avg 3.00 max 4.50 | chunks 64/64 | loom_a=42ms, loom_b=pending"
        );
        assert_eq!(
            telemetry_action_bar(&snapshot),
            "V3 tick 80 | MSPT 3.25/3.00 avg | 2 players | 64 chunks"
        );
    }

    #[test]
    fn view_distance_defaults_and_clamps() {
        assert_eq!(bounded_view_distance(None), DEFAULT_VIEW_DISTANCE_CHUNKS);
        assert_eq!(bounded_view_distance(Some("1")), 2);
        assert_eq!(bounded_view_distance(Some("64")), MAX_VIEW_DISTANCE_CHUNKS);
        assert_eq!(bounded_view_distance(Some("8")), 8);
        assert_eq!(interest_chunk_capacity(2), 25);
    }
}
