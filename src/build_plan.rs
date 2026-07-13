use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use valence::prelude::{BlockPos, BlockState};

use crate::world_command::WorldBounds;

pub const BUILD_PLAN_SCHEMA_VERSION: u32 = 1;
pub const MAX_BUILD_OPERATIONS: usize = 128;
pub const MAX_BUILD_TARGETS: usize = 256;
pub const MAX_IDEMPOTENCY_KEY_BYTES: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildPlan {
    pub schema_version: u32,
    pub idempotency_key: String,
    pub anchor: PlanAnchor,
    pub replace_mode: String,
    pub operations: Vec<BuildOperation>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanAnchor {
    pub x: i32,
    pub y: i32,
    pub z: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum BuildOperation {
    Set {
        at: [i32; 3],
        block: String,
    },
    FillBox {
        from: [i32; 3],
        to: [i32; 3],
        block: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpandedTarget {
    pub position: BlockPos,
    pub block: BlockState,
    pub operation_index: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlanConflict {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation_index: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position: Option<PlanPosition>,
}

impl PlanConflict {
    pub fn plan(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            message: message.into(),
            operation_index: None,
            position: None,
        }
    }

    pub fn operation(code: &str, message: impl Into<String>, operation_index: usize) -> Self {
        Self {
            code: code.to_string(),
            message: message.into(),
            operation_index: Some(operation_index),
            position: None,
        }
    }

    pub fn target(
        code: &str,
        message: impl Into<String>,
        operation_index: usize,
        position: BlockPos,
    ) -> Self {
        Self {
            code: code.to_string(),
            message: message.into(),
            operation_index: Some(operation_index),
            position: Some(position.into()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct PlanPosition {
    pub x: i32,
    pub y: i32,
    pub z: i32,
}

impl From<BlockPos> for PlanPosition {
    fn from(value: BlockPos) -> Self {
        Self {
            x: value.x,
            y: value.y,
            z: value.z,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct PlanBounds {
    pub min: PlanPosition,
    pub max: PlanPosition,
}

#[derive(Debug, Clone)]
pub struct PreparedPlan {
    pub plan: BuildPlan,
    pub canonical_json: String,
    pub plan_hash: String,
    pub targets: Vec<ExpandedTarget>,
    pub bounds: Option<PlanBounds>,
    pub conflicts: Vec<PlanConflict>,
    pub warnings: Vec<String>,
}

pub fn parse_build_plan(value: &serde_json::Value) -> Result<BuildPlan, String> {
    serde_json::from_value(value.clone()).map_err(|error| format!("invalid BuildPlan v1: {error}"))
}

pub fn prepare_build_plan(plan: BuildPlan, world_bounds: WorldBounds) -> PreparedPlan {
    let canonical_json = serde_json::to_string(&plan).expect("BuildPlan serialization cannot fail");
    let plan_hash = format!("{:x}", Sha256::digest(canonical_json.as_bytes()));
    let mut conflicts = Vec::new();
    let mut targets = Vec::new();
    let mut seen = BTreeMap::<(i32, i32, i32), usize>::new();

    if plan.schema_version != BUILD_PLAN_SCHEMA_VERSION {
        conflicts.push(PlanConflict::plan(
            "unsupported_schema_version",
            format!(
                "schema_version {} is unsupported; expected {}",
                plan.schema_version, BUILD_PLAN_SCHEMA_VERSION
            ),
        ));
    }
    if plan.idempotency_key.is_empty() || plan.idempotency_key.len() > MAX_IDEMPOTENCY_KEY_BYTES {
        conflicts.push(PlanConflict::plan(
            "invalid_idempotency_key",
            format!("idempotency_key must contain 1..={MAX_IDEMPOTENCY_KEY_BYTES} UTF-8 bytes"),
        ));
    }
    if plan.replace_mode != "air_only" {
        conflicts.push(PlanConflict::plan(
            "unsupported_replace_mode",
            format!(
                "replace_mode `{}` is unsupported; expected `air_only`",
                plan.replace_mode
            ),
        ));
    }
    if plan.operations.is_empty() || plan.operations.len() > MAX_BUILD_OPERATIONS {
        conflicts.push(PlanConflict::plan(
            "operation_limit",
            format!(
                "operations must contain 1..={MAX_BUILD_OPERATIONS} entries; received {}",
                plan.operations.len()
            ),
        ));
    }

    if conflicts.is_empty() {
        'operations: for (operation_index, operation) in plan.operations.iter().enumerate() {
            let (from, to, block_name) = match operation {
                BuildOperation::Set { at, block } => (*at, *at, block.as_str()),
                BuildOperation::FillBox { from, to, block } => {
                    if (0..3).any(|axis| from[axis] > to[axis]) {
                        conflicts.push(PlanConflict::operation(
                            "invalid_box",
                            "fill_box.from must be component-wise <= fill_box.to",
                            operation_index,
                        ));
                        continue;
                    }
                    (*from, *to, block.as_str())
                }
            };

            let Some(block) = block_state_for_name(block_name) else {
                conflicts.push(PlanConflict::operation(
                    "unsupported_block",
                    format!("block `{block_name}` is not in the BuildPlan v1 palette"),
                    operation_index,
                ));
                continue;
            };

            for y in from[1]..=to[1] {
                for x in from[0]..=to[0] {
                    for z in from[2]..=to[2] {
                        if targets.len() >= MAX_BUILD_TARGETS {
                            conflicts.push(PlanConflict::operation(
                                "expanded_target_limit",
                                format!("plan expands beyond the {MAX_BUILD_TARGETS}-target limit"),
                                operation_index,
                            ));
                            break 'operations;
                        }

                        let Some(position) = absolute_position(plan.anchor, [x, y, z]) else {
                            conflicts.push(PlanConflict::operation(
                                "coordinate_overflow",
                                "anchor plus relative coordinate overflows a signed 32-bit integer",
                                operation_index,
                            ));
                            continue;
                        };

                        let key = (position.x, position.y, position.z);
                        if let Some(first_operation) = seen.get(&key) {
                            conflicts.push(PlanConflict::target(
                                "duplicate_target",
                                format!("target already appears in operation {first_operation}"),
                                operation_index,
                                position,
                            ));
                            continue;
                        }
                        seen.insert(key, operation_index);

                        if !world_bounds.contains(position) {
                            conflicts.push(PlanConflict::target(
                                "out_of_bounds",
                                "target is outside active world bounds",
                                operation_index,
                                position,
                            ));
                        }

                        targets.push(ExpandedTarget {
                            position,
                            block,
                            operation_index,
                        });
                    }
                }
            }
        }
    }

    let bounds = expanded_bounds(&targets);
    let warnings = if targets.len() > 200 {
        vec!["plan is near the synchronous 256-target limit".to_string()]
    } else {
        Vec::new()
    };

    PreparedPlan {
        plan,
        canonical_json,
        plan_hash,
        targets,
        bounds,
        conflicts,
        warnings,
    }
}

pub fn block_state_for_name(name: &str) -> Option<BlockState> {
    match name {
        "stone" => Some(BlockState::STONE),
        "dirt" => Some(BlockState::DIRT),
        "grass_block" => Some(BlockState::GRASS_BLOCK),
        "oak_planks" => Some(BlockState::OAK_PLANKS),
        "cobblestone" => Some(BlockState::COBBLESTONE),
        "glass" => Some(BlockState::GLASS),
        _ => None,
    }
}

pub fn expanded_bounds(targets: &[ExpandedTarget]) -> Option<PlanBounds> {
    let first = targets.first()?.position;
    let mut min = first;
    let mut max = first;
    for target in &targets[1..] {
        min.x = min.x.min(target.position.x);
        min.y = min.y.min(target.position.y);
        min.z = min.z.min(target.position.z);
        max.x = max.x.max(target.position.x);
        max.y = max.y.max(target.position.y);
        max.z = max.z.max(target.position.z);
    }
    Some(PlanBounds {
        min: min.into(),
        max: max.into(),
    })
}

fn absolute_position(anchor: PlanAnchor, offset: [i32; 3]) -> Option<BlockPos> {
    Some(BlockPos::new(
        anchor.x.checked_add(offset[0])?,
        anchor.y.checked_add(offset[1])?,
        anchor.z.checked_add(offset[2])?,
    ))
}

pub fn target_positions(targets: &[ExpandedTarget]) -> BTreeSet<BlockPos> {
    targets.iter().map(|target| target.position).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world_command::WORLD_BOUNDS;

    fn plan(operations: Vec<BuildOperation>) -> BuildPlan {
        BuildPlan {
            schema_version: 1,
            idempotency_key: "test-key".to_string(),
            anchor: PlanAnchor { x: 8, y: 65, z: 8 },
            replace_mode: "air_only".to_string(),
            operations,
        }
    }

    #[test]
    fn set_and_fill_expand_in_y_x_z_order() {
        let prepared = prepare_build_plan(
            plan(vec![
                BuildOperation::Set {
                    at: [0, 0, 0],
                    block: "stone".to_string(),
                },
                BuildOperation::FillBox {
                    from: [1, 0, 0],
                    to: [2, 1, 0],
                    block: "glass".to_string(),
                },
            ]),
            WORLD_BOUNDS,
        );
        assert!(prepared.conflicts.is_empty());
        assert_eq!(prepared.targets.len(), 5);
        assert_eq!(prepared.targets[0].position, BlockPos::new(8, 65, 8));
        assert_eq!(prepared.targets[1].position, BlockPos::new(9, 65, 8));
        assert_eq!(prepared.targets[2].position, BlockPos::new(10, 65, 8));
        assert_eq!(prepared.targets[3].position, BlockPos::new(9, 66, 8));
        assert_eq!(prepared.targets[4].position, BlockPos::new(10, 66, 8));
    }

    #[test]
    fn duplicate_expansion_is_rejected() {
        let prepared = prepare_build_plan(
            plan(vec![
                BuildOperation::Set {
                    at: [0, 0, 0],
                    block: "stone".to_string(),
                },
                BuildOperation::Set {
                    at: [0, 0, 0],
                    block: "glass".to_string(),
                },
            ]),
            WORLD_BOUNDS,
        );
        assert_eq!(prepared.conflicts[0].code, "duplicate_target");
    }

    #[test]
    fn target_limit_is_checked_during_expansion() {
        let prepared = prepare_build_plan(
            plan(vec![BuildOperation::FillBox {
                from: [0, 0, 0],
                to: [256, 0, 0],
                block: "stone".to_string(),
            }]),
            WORLD_BOUNDS,
        );
        assert_eq!(prepared.targets.len(), MAX_BUILD_TARGETS);
        assert!(prepared
            .conflicts
            .iter()
            .any(|conflict| conflict.code == "expanded_target_limit"));
    }

    #[test]
    fn canonical_hash_ignores_input_key_order() {
        let a = serde_json::json!({
            "schema_version": 1,
            "idempotency_key": "same",
            "anchor": {"x": 8, "y": 65, "z": 8},
            "replace_mode": "air_only",
            "operations": [{"op": "set", "at": [0, 0, 0], "block": "stone"}]
        });
        let b = serde_json::json!({
            "operations": [{"block": "stone", "at": [0, 0, 0], "op": "set"}],
            "replace_mode": "air_only",
            "anchor": {"z": 8, "y": 65, "x": 8},
            "idempotency_key": "same",
            "schema_version": 1
        });
        let a = prepare_build_plan(parse_build_plan(&a).unwrap(), WORLD_BOUNDS);
        let b = prepare_build_plan(parse_build_plan(&b).unwrap(), WORLD_BOUNDS);
        assert_eq!(a.plan_hash, b.plan_hash);
        assert_eq!(a.canonical_json, b.canonical_json);
    }
}
