use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use uuid::Uuid;
use valence::prelude::{BlockPos, BlockState};
use valence::ChunkLayer;

use crate::build_plan::{ExpandedTarget, PlanBounds, PlanConflict, PreparedPlan};
use crate::persistence::{BuildBlockRecord, NewBuildRecord, PersistenceRuntime, StoredBuild};
use crate::world_command::{
    execute_world_command, validate_world_command, CommandContext, WorldBounds, WorldCommand,
    WorldCommandError,
};

pub type PositionKey = (i32, i32, i32);

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BuildValidationReport {
    pub valid: bool,
    pub plan_hash: String,
    pub expanded_targets: usize,
    pub bounds: Option<PlanBounds>,
    pub conflicts: Vec<PlanConflict>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ApplyBuildResult {
    pub build_id: String,
    pub plan_hash: String,
    pub changed: usize,
    pub skipped: usize,
    pub undo_available: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UndoBuildResult {
    pub build_id: String,
    pub state: String,
    pub restored: usize,
    pub already_undone: bool,
}

pub fn validate_build_plan(
    prepared: &PreparedPlan,
    layer: &ChunkLayer,
    bounds: WorldBounds,
    player_occupied: &BTreeSet<PositionKey>,
) -> BuildValidationReport {
    validate_build_plan_with(prepared, bounds, player_occupied, |position| {
        block_state_at(layer, position)
    })
}

pub fn validate_build_plan_with(
    prepared: &PreparedPlan,
    bounds: WorldBounds,
    player_occupied: &BTreeSet<PositionKey>,
    mut world_block_at: impl FnMut(BlockPos) -> BlockState,
) -> BuildValidationReport {
    let mut conflicts = prepared.conflicts.clone();
    if conflicts.is_empty() {
        let mut shadow = BTreeMap::<PositionKey, BlockState>::new();
        for target in &prepared.targets {
            let current = state_at(&shadow, &mut world_block_at, target.position);
            if player_occupied.contains(&position_key(target.position)) {
                conflicts.push(PlanConflict::target(
                    "player_collision",
                    "target intersects a connected player's occupied space",
                    target.operation_index,
                    target.position,
                ));
                continue;
            }

            let anchor = placement_anchor(&shadow, &mut world_block_at, target.position);
            let command = WorldCommand::SetBlock {
                position: target.position,
                block: target.block,
            };
            let context = CommandContext::for_placement(current, anchor, false);
            match validate_world_command(bounds, command, context) {
                Ok(()) => {
                    shadow.insert(position_key(target.position), target.block);
                }
                Err(error) => conflicts.push(world_command_conflict(target, error)),
            }
        }
    }

    BuildValidationReport {
        valid: conflicts.is_empty(),
        plan_hash: prepared.plan_hash.clone(),
        expanded_targets: prepared.targets.len(),
        bounds: prepared.bounds,
        conflicts,
        warnings: prepared.warnings.clone(),
    }
}

pub fn apply_build_plan(
    prepared: &PreparedPlan,
    layer: &mut ChunkLayer,
    bounds: WorldBounds,
    player_occupied: &BTreeSet<PositionKey>,
    persistence: &PersistenceRuntime,
) -> Result<ApplyBuildResult, String> {
    if let Some(existing) = persistence
        .load_build_by_idempotency_key(&prepared.plan.idempotency_key)
        .map_err(|error| format!("load build idempotency record failed: {error}"))?
    {
        if existing.plan_hash != prepared.plan_hash {
            return Err(format!(
                "idempotency_key `{}` is already bound to a different plan_hash",
                prepared.plan.idempotency_key
            ));
        }
        return Ok(apply_result(&existing));
    }

    let report = validate_build_plan(prepared, layer, bounds, player_occupied);
    if !report.valid {
        return Err(format!(
            "BuildPlan validation failed: {}",
            serde_json::to_string(&report).expect("validation report serialization cannot fail")
        ));
    }

    let blocks = prepared
        .targets
        .iter()
        .map(|target| BuildBlockRecord {
            position: target.position,
            original_block: block_state_at(layer, target.position),
            applied_block: target.block,
        })
        .collect::<Vec<_>>();

    for (applied, target) in prepared.targets.iter().enumerate() {
        let context = CommandContext::for_placement(
            block_state_at(layer, target.position),
            placement_anchor_in_layer(layer, target.position),
            false,
        );
        if let Err(error) = execute_world_command(
            layer,
            bounds,
            WorldCommand::SetBlock {
                position: target.position,
                block: target.block,
            },
            context,
        ) {
            restore_prefix(layer, &blocks, applied, false);
            return Err(format!(
                "authoritative apply failed at {:?}: {error}",
                target.position
            ));
        }
    }

    let new_record = NewBuildRecord {
        build_id: Uuid::new_v4().to_string(),
        idempotency_key: prepared.plan.idempotency_key.clone(),
        plan_hash: prepared.plan_hash.clone(),
        canonical_plan_json: prepared.canonical_json.clone(),
        actor: "mcp".to_string(),
        blocks: blocks.clone(),
    };
    let stored = match persistence.persist_build(&new_record) {
        Ok(stored) => stored,
        Err(error) => {
            restore_prefix(layer, &blocks, blocks.len(), false);
            return Err(format!(
                "durable build transaction failed; live world restored: {error}"
            ));
        }
    };

    Ok(apply_result(&stored))
}

pub fn get_build(persistence: &PersistenceRuntime, build_id: &str) -> Result<StoredBuild, String> {
    persistence
        .load_build(build_id)
        .map_err(|error| format!("load build failed: {error}"))?
        .ok_or_else(|| format!("unknown build_id `{build_id}`"))
}

pub fn undo_build(
    persistence: &PersistenceRuntime,
    layer: &mut ChunkLayer,
    bounds: WorldBounds,
    build_id: &str,
) -> Result<UndoBuildResult, String> {
    let build = get_build(persistence, build_id)?;
    if build.state == "undone" {
        return Ok(UndoBuildResult {
            build_id: build.build_id,
            state: build.state,
            restored: build.changed,
            already_undone: true,
        });
    }

    for block in &build.blocks {
        let current = block_state_at(layer, block.position);
        if current != block.applied_block {
            return Err(format!(
                "cannot undo build with drift at {:?}: expected {}, found {}",
                block.position, block.applied_block, current
            ));
        }
        if !bounds.contains(block.position) {
            return Err(format!(
                "cannot undo build target outside active bounds: {:?}",
                block.position
            ));
        }
    }

    for (restored, block) in build.blocks.iter().rev().enumerate() {
        let result = if block.original_block == BlockState::AIR {
            execute_world_command(
                layer,
                bounds,
                WorldCommand::RemoveBlock {
                    position: block.position,
                },
                CommandContext::for_existing_block(block_state_at(layer, block.position)),
            )
        } else {
            // BuildPlan v1 is air_only, so this branch is defensive for stored
            // records. It is still bounded and restores a previously captured
            // server state without accepting a new unvalidated block string.
            layer
                .set_block(block.position, block.original_block)
                .map(|_| ())
                .ok_or(WorldCommandError::MissingLoadedChunk {
                    position: block.position,
                })
        };
        if let Err(error) = result {
            restore_prefix(layer, &build.blocks, restored, true);
            return Err(format!(
                "authoritative undo failed at {:?}: {error}",
                block.position
            ));
        }
    }

    let undone = match persistence.mark_build_undone(&build) {
        Ok(stored) => stored,
        Err(error) => {
            for block in &build.blocks {
                let _ = layer.set_block(block.position, block.applied_block);
            }
            return Err(format!(
                "durable undo transaction failed; live applied state restored: {error}"
            ));
        }
    };

    Ok(UndoBuildResult {
        build_id: undone.build_id,
        state: undone.state,
        restored: undone.changed,
        already_undone: false,
    })
}

pub fn stored_build_summary(build: &StoredBuild) -> serde_json::Value {
    serde_json::json!({
        "build_id": build.build_id,
        "plan_hash": build.plan_hash,
        "actor": build.actor,
        "state": build.state,
        "changed": build.changed,
        "skipped": build.skipped,
        "undo_available": build.state == "applied",
        "created_at": build.created_at,
        "applied_at": build.applied_at,
        "undone_at": build.undone_at,
    })
}

fn apply_result(build: &StoredBuild) -> ApplyBuildResult {
    ApplyBuildResult {
        build_id: build.build_id.clone(),
        plan_hash: build.plan_hash.clone(),
        changed: build.changed,
        skipped: build.skipped,
        undo_available: build.state == "applied",
    }
}

fn world_command_conflict(target: &ExpandedTarget, error: WorldCommandError) -> PlanConflict {
    let code = match error {
        WorldCommandError::OutOfBounds { .. } => "out_of_bounds",
        WorldCommandError::UnsupportedBlock { .. } => "unsupported_block",
        WorldCommandError::TargetNotEmpty { .. } => "occupied_target",
        WorldCommandError::ProtectedSpawn { .. } => "protected_region",
        WorldCommandError::ActorHeadInsideBlock => "player_collision",
        _ => "placement_rule",
    };
    PlanConflict::target(
        code,
        error.to_string(),
        target.operation_index,
        target.position,
    )
}

fn placement_anchor(
    shadow: &BTreeMap<PositionKey, BlockState>,
    world_block_at: &mut impl FnMut(BlockPos) -> BlockState,
    position: BlockPos,
) -> BlockState {
    neighbor_positions(position)
        .into_iter()
        .map(|anchor| state_at(shadow, world_block_at, anchor))
        .find(|block| *block != BlockState::AIR)
        .unwrap_or(BlockState::AIR)
}

fn placement_anchor_in_layer(layer: &ChunkLayer, position: BlockPos) -> BlockState {
    neighbor_positions(position)
        .into_iter()
        .map(|anchor| block_state_at(layer, anchor))
        .find(|block| *block != BlockState::AIR)
        .unwrap_or(BlockState::AIR)
}

fn neighbor_positions(position: BlockPos) -> [BlockPos; 6] {
    [
        BlockPos::new(position.x, position.y - 1, position.z),
        BlockPos::new(position.x, position.y + 1, position.z),
        BlockPos::new(position.x - 1, position.y, position.z),
        BlockPos::new(position.x + 1, position.y, position.z),
        BlockPos::new(position.x, position.y, position.z - 1),
        BlockPos::new(position.x, position.y, position.z + 1),
    ]
}

fn state_at(
    shadow: &BTreeMap<PositionKey, BlockState>,
    world_block_at: &mut impl FnMut(BlockPos) -> BlockState,
    position: BlockPos,
) -> BlockState {
    shadow
        .get(&position_key(position))
        .copied()
        .unwrap_or_else(|| world_block_at(position))
}

fn block_state_at(layer: &ChunkLayer, position: BlockPos) -> BlockState {
    layer
        .block(position)
        .map(|block| block.state)
        .unwrap_or(BlockState::AIR)
}

fn position_key(position: BlockPos) -> PositionKey {
    (position.x, position.y, position.z)
}

fn restore_prefix(
    layer: &mut ChunkLayer,
    blocks: &[BuildBlockRecord],
    count: usize,
    applied_state: bool,
) {
    let selected = if applied_state {
        &blocks[blocks.len().saturating_sub(count)..]
    } else {
        &blocks[..count.min(blocks.len())]
    };
    for block in selected {
        let state = if applied_state {
            block.applied_block
        } else {
            block.original_block
        };
        let _ = layer.set_block(block.position, state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build_plan::{prepare_build_plan, BuildOperation, BuildPlan, PlanAnchor};
    use crate::world_command::{GROUND_Y, WORLD_BOUNDS};

    fn plan(operations: Vec<BuildOperation>) -> PreparedPlan {
        prepare_build_plan(
            BuildPlan {
                schema_version: 1,
                idempotency_key: "service-test".to_string(),
                anchor: PlanAnchor {
                    x: 12,
                    y: GROUND_Y + 1,
                    z: 12,
                },
                replace_mode: "air_only".to_string(),
                operations,
            },
            WORLD_BOUNDS,
        )
    }

    fn flat_world(position: BlockPos) -> BlockState {
        if position.y == GROUND_Y {
            BlockState::GRASS_BLOCK
        } else {
            BlockState::AIR
        }
    }

    #[test]
    fn validation_uses_lower_blocks_created_earlier_in_the_plan() {
        let prepared = plan(vec![BuildOperation::FillBox {
            from: [0, 0, 0],
            to: [0, 2, 0],
            block: "stone".to_string(),
        }]);
        let report =
            validate_build_plan_with(&prepared, WORLD_BOUNDS, &BTreeSet::new(), flat_world);
        assert!(report.valid, "{:?}", report.conflicts);
    }

    #[test]
    fn occupied_target_and_protected_spawn_are_reported() {
        let occupied = plan(vec![BuildOperation::Set {
            at: [0, 0, 0],
            block: "stone".to_string(),
        }]);
        let occupied_report =
            validate_build_plan_with(&occupied, WORLD_BOUNDS, &BTreeSet::new(), |_| {
                BlockState::GLASS
            });
        assert_eq!(occupied_report.conflicts[0].code, "occupied_target");

        let protected = prepare_build_plan(
            BuildPlan {
                schema_version: 1,
                idempotency_key: "protected".to_string(),
                anchor: PlanAnchor { x: 0, y: 65, z: 0 },
                replace_mode: "air_only".to_string(),
                operations: vec![BuildOperation::Set {
                    at: [0, 0, 0],
                    block: "stone".to_string(),
                }],
            },
            WORLD_BOUNDS,
        );
        let protected_report =
            validate_build_plan_with(&protected, WORLD_BOUNDS, &BTreeSet::new(), flat_world);
        assert_eq!(protected_report.conflicts[0].code, "protected_region");
    }

    #[test]
    fn validation_does_not_mutate_source_state() {
        let prepared = plan(vec![BuildOperation::Set {
            at: [0, 0, 0],
            block: "stone".to_string(),
        }]);
        let reads = std::cell::Cell::new(0);
        let report =
            validate_build_plan_with(&prepared, WORLD_BOUNDS, &BTreeSet::new(), |position| {
                reads.set(reads.get() + 1);
                flat_world(position)
            });
        assert!(report.valid);
        assert!(reads.get() > 0);
    }
}
