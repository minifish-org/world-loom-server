use std::fmt;

use valence::prelude::{BlockPos, BlockState, ItemKind};
use valence::ChunkLayer;

pub const GROUND_Y: i32 = 64;
pub const SPAWN_FEET_Y: i32 = GROUND_Y + 1;
pub const SPAWN_HEAD_Y: i32 = GROUND_Y + 2;

pub const WORLD_BOUNDS: WorldBounds = WorldBounds {
    min_x: -64,
    max_x: 63,
    min_y: GROUND_Y - 4,
    max_y: GROUND_Y + 31,
    min_z: -64,
    max_z: 63,
};

pub const HOTBAR_BLOCKS: [ItemKind; 6] = [
    ItemKind::Stone,
    ItemKind::Dirt,
    ItemKind::GrassBlock,
    ItemKind::OakPlanks,
    ItemKind::Cobblestone,
    ItemKind::Glass,
];

const ALLOWED_SET_BLOCKS: [BlockState; 6] = [
    BlockState::STONE,
    BlockState::DIRT,
    BlockState::GRASS_BLOCK,
    BlockState::OAK_PLANKS,
    BlockState::COBBLESTONE,
    BlockState::GLASS,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorldBounds {
    pub min_x: i32,
    pub max_x: i32,
    pub min_y: i32,
    pub max_y: i32,
    pub min_z: i32,
    pub max_z: i32,
}

impl WorldBounds {
    pub fn contains(self, position: BlockPos) -> bool {
        (self.min_x..=self.max_x).contains(&position.x)
            && (self.min_y..=self.max_y).contains(&position.y)
            && (self.min_z..=self.max_z).contains(&position.z)
    }
}

pub fn base_block_state_at(bounds: WorldBounds, position: BlockPos) -> Option<BlockState> {
    if !bounds.contains(position) {
        return None;
    }

    Some(match position.y {
        y if y == GROUND_Y - 4 => BlockState::BEDROCK,
        y if y == GROUND_Y - 3 => BlockState::STONE,
        y if y == GROUND_Y - 2 || y == GROUND_Y - 1 => BlockState::DIRT,
        y if y == GROUND_Y => BlockState::GRASS_BLOCK,
        _ => BlockState::AIR,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorldCommand {
    SetBlock {
        position: BlockPos,
        block: BlockState,
    },
    RemoveBlock {
        position: BlockPos,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandContext {
    pub target_block: BlockState,
    pub anchor_block: Option<BlockState>,
    pub actor_head_inside_block: bool,
}

impl CommandContext {
    pub fn for_existing_block(target_block: BlockState) -> Self {
        Self {
            target_block,
            anchor_block: None,
            actor_head_inside_block: false,
        }
    }

    pub fn for_placement(
        target_block: BlockState,
        anchor_block: BlockState,
        actor_head_inside_block: bool,
    ) -> Self {
        Self {
            target_block,
            anchor_block: Some(anchor_block),
            actor_head_inside_block,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorldCommandError {
    OutOfBounds {
        position: BlockPos,
    },
    UnsupportedBlock {
        block: BlockState,
    },
    TargetNotEmpty {
        position: BlockPos,
        current: BlockState,
    },
    MissingPlacementAnchor {
        position: BlockPos,
    },
    CannotPlaceAgainstAir {
        position: BlockPos,
    },
    ActorHeadInsideBlock,
    ProtectedSpawn {
        position: BlockPos,
    },
    CannotRemoveAir {
        position: BlockPos,
    },
    CannotRemoveFoundation {
        position: BlockPos,
    },
    MissingLoadedChunk {
        position: BlockPos,
    },
}

impl fmt::Display for WorldCommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutOfBounds { position } => write!(f, "position {position:?} is outside bounds"),
            Self::UnsupportedBlock { block } => write!(f, "block {block} is not allowed in M2"),
            Self::TargetNotEmpty { position, current } => {
                write!(f, "position {position:?} already contains {current}")
            }
            Self::MissingPlacementAnchor { position } => {
                write!(f, "position {position:?} has no placement anchor")
            }
            Self::CannotPlaceAgainstAir { position } => {
                write!(f, "position {position:?} cannot be placed against air")
            }
            Self::ActorHeadInsideBlock => write!(f, "actor head is inside a block"),
            Self::ProtectedSpawn { position } => {
                write!(f, "position {position:?} is protected spawn space")
            }
            Self::CannotRemoveAir { position } => write!(f, "position {position:?} is already air"),
            Self::CannotRemoveFoundation { position } => {
                write!(f, "position {position:?} is protected foundation")
            }
            Self::MissingLoadedChunk { position } => {
                write!(f, "position {position:?} has no loaded chunk")
            }
        }
    }
}

impl std::error::Error for WorldCommandError {}

pub fn validate_world_command(
    bounds: WorldBounds,
    command: WorldCommand,
    context: CommandContext,
) -> Result<(), WorldCommandError> {
    match command {
        WorldCommand::SetBlock { position, block } => {
            validate_position(bounds, position)?;
            validate_set_block_type(block)?;
            validate_placement_safety(position, context)
        }
        WorldCommand::RemoveBlock { position } => {
            validate_position(bounds, position)?;
            validate_remove_block(position, context.target_block)
        }
    }
}

pub fn execute_world_command(
    layer: &mut ChunkLayer,
    bounds: WorldBounds,
    command: WorldCommand,
    context: CommandContext,
) -> Result<(), WorldCommandError> {
    validate_world_command(bounds, command, context)?;

    let (position, block) = match command {
        WorldCommand::SetBlock { position, block } => (position, block),
        WorldCommand::RemoveBlock { position } => (position, BlockState::AIR),
    };

    layer
        .set_block(position, block)
        .map(|_| ())
        .ok_or(WorldCommandError::MissingLoadedChunk { position })
}

fn validate_position(bounds: WorldBounds, position: BlockPos) -> Result<(), WorldCommandError> {
    if bounds.contains(position) {
        Ok(())
    } else {
        Err(WorldCommandError::OutOfBounds { position })
    }
}

fn validate_set_block_type(block: BlockState) -> Result<(), WorldCommandError> {
    if ALLOWED_SET_BLOCKS.contains(&block) {
        Ok(())
    } else {
        Err(WorldCommandError::UnsupportedBlock { block })
    }
}

fn validate_placement_safety(
    position: BlockPos,
    context: CommandContext,
) -> Result<(), WorldCommandError> {
    if is_protected_spawn(position) {
        return Err(WorldCommandError::ProtectedSpawn { position });
    }

    if context.actor_head_inside_block {
        return Err(WorldCommandError::ActorHeadInsideBlock);
    }

    if context.target_block != BlockState::AIR {
        return Err(WorldCommandError::TargetNotEmpty {
            position,
            current: context.target_block,
        });
    }

    let Some(anchor_block) = context.anchor_block else {
        return Err(WorldCommandError::MissingPlacementAnchor { position });
    };

    if anchor_block == BlockState::AIR {
        return Err(WorldCommandError::CannotPlaceAgainstAir { position });
    }

    Ok(())
}

fn validate_remove_block(
    position: BlockPos,
    target_block: BlockState,
) -> Result<(), WorldCommandError> {
    if target_block == BlockState::AIR {
        return Err(WorldCommandError::CannotRemoveAir { position });
    }

    if target_block == BlockState::BEDROCK || position.y <= WORLD_BOUNDS.min_y {
        return Err(WorldCommandError::CannotRemoveFoundation { position });
    }

    Ok(())
}

fn is_protected_spawn(position: BlockPos) -> bool {
    position.x.abs() <= 1
        && position.z.abs() <= 1
        && (SPAWN_FEET_Y..=SPAWN_HEAD_Y).contains(&position.y)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pos(x: i32, y: i32, z: i32) -> BlockPos {
        BlockPos::new(x, y, z)
    }

    fn placement_context() -> CommandContext {
        CommandContext::for_placement(BlockState::AIR, BlockState::GRASS_BLOCK, false)
    }

    #[test]
    fn base_block_state_matches_generated_superflat_world() {
        assert_eq!(
            base_block_state_at(WORLD_BOUNDS, pos(4, GROUND_Y - 4, 4)),
            Some(BlockState::BEDROCK)
        );
        assert_eq!(
            base_block_state_at(WORLD_BOUNDS, pos(4, GROUND_Y, 4)),
            Some(BlockState::GRASS_BLOCK)
        );
        assert_eq!(
            base_block_state_at(WORLD_BOUNDS, pos(4, GROUND_Y + 1, 4)),
            Some(BlockState::AIR)
        );
        assert_eq!(
            base_block_state_at(WORLD_BOUNDS, pos(WORLD_BOUNDS.max_x + 1, GROUND_Y, 4)),
            None
        );
    }

    #[test]
    fn set_block_accepts_allowed_block_inside_bounds() {
        let command = WorldCommand::SetBlock {
            position: pos(4, GROUND_Y + 1, 4),
            block: BlockState::STONE,
        };

        assert_eq!(
            validate_world_command(WORLD_BOUNDS, command, placement_context()),
            Ok(())
        );
    }

    #[test]
    fn set_block_rejects_out_of_bounds_position() {
        let command = WorldCommand::SetBlock {
            position: pos(WORLD_BOUNDS.max_x + 1, GROUND_Y + 1, 0),
            block: BlockState::STONE,
        };

        assert_eq!(
            validate_world_command(WORLD_BOUNDS, command, placement_context()),
            Err(WorldCommandError::OutOfBounds {
                position: pos(WORLD_BOUNDS.max_x + 1, GROUND_Y + 1, 0)
            })
        );
    }

    #[test]
    fn set_block_rejects_unsupported_block_type() {
        let command = WorldCommand::SetBlock {
            position: pos(4, GROUND_Y + 1, 4),
            block: BlockState::DIAMOND_BLOCK,
        };

        assert_eq!(
            validate_world_command(WORLD_BOUNDS, command, placement_context()),
            Err(WorldCommandError::UnsupportedBlock {
                block: BlockState::DIAMOND_BLOCK
            })
        );
    }

    #[test]
    fn set_block_rejects_non_empty_target() {
        let command = WorldCommand::SetBlock {
            position: pos(4, GROUND_Y + 1, 4),
            block: BlockState::STONE,
        };
        let context =
            CommandContext::for_placement(BlockState::DIRT, BlockState::GRASS_BLOCK, false);

        assert_eq!(
            validate_world_command(WORLD_BOUNDS, command, context),
            Err(WorldCommandError::TargetNotEmpty {
                position: pos(4, GROUND_Y + 1, 4),
                current: BlockState::DIRT
            })
        );
    }

    #[test]
    fn set_block_rejects_air_anchor() {
        let command = WorldCommand::SetBlock {
            position: pos(4, GROUND_Y + 1, 4),
            block: BlockState::STONE,
        };
        let context = CommandContext::for_placement(BlockState::AIR, BlockState::AIR, false);

        assert_eq!(
            validate_world_command(WORLD_BOUNDS, command, context),
            Err(WorldCommandError::CannotPlaceAgainstAir {
                position: pos(4, GROUND_Y + 1, 4)
            })
        );
    }

    #[test]
    fn set_block_rejects_protected_spawn_space() {
        let command = WorldCommand::SetBlock {
            position: pos(0, SPAWN_FEET_Y, 0),
            block: BlockState::STONE,
        };

        assert_eq!(
            validate_world_command(WORLD_BOUNDS, command, placement_context()),
            Err(WorldCommandError::ProtectedSpawn {
                position: pos(0, SPAWN_FEET_Y, 0)
            })
        );
    }

    #[test]
    fn set_block_rejects_actor_head_inside_block() {
        let command = WorldCommand::SetBlock {
            position: pos(4, GROUND_Y + 1, 4),
            block: BlockState::STONE,
        };
        let context = CommandContext::for_placement(BlockState::AIR, BlockState::GRASS_BLOCK, true);

        assert_eq!(
            validate_world_command(WORLD_BOUNDS, command, context),
            Err(WorldCommandError::ActorHeadInsideBlock)
        );
    }

    #[test]
    fn remove_block_accepts_non_foundation_block_inside_bounds() {
        let command = WorldCommand::RemoveBlock {
            position: pos(4, GROUND_Y, 4),
        };
        let context = CommandContext::for_existing_block(BlockState::GRASS_BLOCK);

        assert_eq!(
            validate_world_command(WORLD_BOUNDS, command, context),
            Ok(())
        );
    }

    #[test]
    fn remove_block_rejects_air() {
        let command = WorldCommand::RemoveBlock {
            position: pos(4, GROUND_Y + 1, 4),
        };
        let context = CommandContext::for_existing_block(BlockState::AIR);

        assert_eq!(
            validate_world_command(WORLD_BOUNDS, command, context),
            Err(WorldCommandError::CannotRemoveAir {
                position: pos(4, GROUND_Y + 1, 4)
            })
        );
    }

    #[test]
    fn remove_block_rejects_bedrock_foundation() {
        let command = WorldCommand::RemoveBlock {
            position: pos(4, GROUND_Y - 4, 4),
        };
        let context = CommandContext::for_existing_block(BlockState::BEDROCK);

        assert_eq!(
            validate_world_command(WORLD_BOUNDS, command, context),
            Err(WorldCommandError::CannotRemoveFoundation {
                position: pos(4, GROUND_Y - 4, 4)
            })
        );
    }
}
