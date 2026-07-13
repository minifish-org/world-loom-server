use serde::Serialize;
use valence::prelude::BlockState;

pub const BUILD_PALETTE_VERSION: u32 = 1;
pub const BUILD_PALETTE_BLOCK_COUNT: usize = 115;
pub const MINECRAFT_VERSION: &str = "1.20.1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct BuildPaletteEntry {
    pub name: &'static str,
    #[serde(skip)]
    pub block: BlockState,
    pub family: &'static str,
    pub representative_rgb: &'static str,
    pub opacity: &'static str,
}

macro_rules! block {
    ($name:literal, $state:ident, $family:literal, $rgb:literal, $opacity:literal) => {
        BuildPaletteEntry {
            name: $name,
            block: BlockState::$state,
            family: $family,
            representative_rgb: $rgb,
            opacity: $opacity,
        }
    };
}

pub const BUILD_PALETTE: &[BuildPaletteEntry] = &[
    // Original World Loom building blocks.
    block!("stone", STONE, "masonry", "#7D7D7D", "opaque"),
    block!("dirt", DIRT, "earth", "#866043", "opaque"),
    block!("grass_block", GRASS_BLOCK, "earth", "#6A8A3A", "opaque"),
    block!("oak_planks", OAK_PLANKS, "planks", "#A2834F", "opaque"),
    block!("cobblestone", COBBLESTONE, "masonry", "#7A7A7A", "opaque"),
    block!("glass", GLASS, "glass", "#D7ECEC", "translucent"),
    // Concrete dye family.
    block!(
        "white_concrete",
        WHITE_CONCRETE,
        "concrete",
        "#CFD5D6",
        "opaque"
    ),
    block!(
        "orange_concrete",
        ORANGE_CONCRETE,
        "concrete",
        "#E06100",
        "opaque"
    ),
    block!(
        "magenta_concrete",
        MAGENTA_CONCRETE,
        "concrete",
        "#A9309F",
        "opaque"
    ),
    block!(
        "light_blue_concrete",
        LIGHT_BLUE_CONCRETE,
        "concrete",
        "#2389C6",
        "opaque"
    ),
    block!(
        "yellow_concrete",
        YELLOW_CONCRETE,
        "concrete",
        "#F1AF15",
        "opaque"
    ),
    block!(
        "lime_concrete",
        LIME_CONCRETE,
        "concrete",
        "#5EA919",
        "opaque"
    ),
    block!(
        "pink_concrete",
        PINK_CONCRETE,
        "concrete",
        "#D5658E",
        "opaque"
    ),
    block!(
        "gray_concrete",
        GRAY_CONCRETE,
        "concrete",
        "#36393D",
        "opaque"
    ),
    block!(
        "light_gray_concrete",
        LIGHT_GRAY_CONCRETE,
        "concrete",
        "#7D7D73",
        "opaque"
    ),
    block!(
        "cyan_concrete",
        CYAN_CONCRETE,
        "concrete",
        "#157788",
        "opaque"
    ),
    block!(
        "purple_concrete",
        PURPLE_CONCRETE,
        "concrete",
        "#64209C",
        "opaque"
    ),
    block!(
        "blue_concrete",
        BLUE_CONCRETE,
        "concrete",
        "#2C2E8F",
        "opaque"
    ),
    block!(
        "brown_concrete",
        BROWN_CONCRETE,
        "concrete",
        "#603B1F",
        "opaque"
    ),
    block!(
        "green_concrete",
        GREEN_CONCRETE,
        "concrete",
        "#495B24",
        "opaque"
    ),
    block!(
        "red_concrete",
        RED_CONCRETE,
        "concrete",
        "#8E2020",
        "opaque"
    ),
    block!(
        "black_concrete",
        BLACK_CONCRETE,
        "concrete",
        "#080A0F",
        "opaque"
    ),
    // Wool dye family.
    block!("white_wool", WHITE_WOOL, "wool", "#E9ECEC", "opaque"),
    block!("orange_wool", ORANGE_WOOL, "wool", "#F07613", "opaque"),
    block!("magenta_wool", MAGENTA_WOOL, "wool", "#BD44B3", "opaque"),
    block!(
        "light_blue_wool",
        LIGHT_BLUE_WOOL,
        "wool",
        "#3AAFD9",
        "opaque"
    ),
    block!("yellow_wool", YELLOW_WOOL, "wool", "#F8C627", "opaque"),
    block!("lime_wool", LIME_WOOL, "wool", "#70B919", "opaque"),
    block!("pink_wool", PINK_WOOL, "wool", "#ED8DAC", "opaque"),
    block!("gray_wool", GRAY_WOOL, "wool", "#3E4447", "opaque"),
    block!(
        "light_gray_wool",
        LIGHT_GRAY_WOOL,
        "wool",
        "#8E8E86",
        "opaque"
    ),
    block!("cyan_wool", CYAN_WOOL, "wool", "#168C9C", "opaque"),
    block!("purple_wool", PURPLE_WOOL, "wool", "#8932B8", "opaque"),
    block!("blue_wool", BLUE_WOOL, "wool", "#35399D", "opaque"),
    block!("brown_wool", BROWN_WOOL, "wool", "#724728", "opaque"),
    block!("green_wool", GREEN_WOOL, "wool", "#546D1B", "opaque"),
    block!("red_wool", RED_WOOL, "wool", "#A12722", "opaque"),
    block!("black_wool", BLACK_WOOL, "wool", "#141519", "opaque"),
    // Terracotta dye family.
    block!(
        "white_terracotta",
        WHITE_TERRACOTTA,
        "terracotta",
        "#D1B2A1",
        "opaque"
    ),
    block!(
        "orange_terracotta",
        ORANGE_TERRACOTTA,
        "terracotta",
        "#A15325",
        "opaque"
    ),
    block!(
        "magenta_terracotta",
        MAGENTA_TERRACOTTA,
        "terracotta",
        "#95576C",
        "opaque"
    ),
    block!(
        "light_blue_terracotta",
        LIGHT_BLUE_TERRACOTTA,
        "terracotta",
        "#706C8A",
        "opaque"
    ),
    block!(
        "yellow_terracotta",
        YELLOW_TERRACOTTA,
        "terracotta",
        "#BA8524",
        "opaque"
    ),
    block!(
        "lime_terracotta",
        LIME_TERRACOTTA,
        "terracotta",
        "#677535",
        "opaque"
    ),
    block!(
        "pink_terracotta",
        PINK_TERRACOTTA,
        "terracotta",
        "#A04D4E",
        "opaque"
    ),
    block!(
        "gray_terracotta",
        GRAY_TERRACOTTA,
        "terracotta",
        "#392A24",
        "opaque"
    ),
    block!(
        "light_gray_terracotta",
        LIGHT_GRAY_TERRACOTTA,
        "terracotta",
        "#876B62",
        "opaque"
    ),
    block!(
        "cyan_terracotta",
        CYAN_TERRACOTTA,
        "terracotta",
        "#575C5C",
        "opaque"
    ),
    block!(
        "purple_terracotta",
        PURPLE_TERRACOTTA,
        "terracotta",
        "#764656",
        "opaque"
    ),
    block!(
        "blue_terracotta",
        BLUE_TERRACOTTA,
        "terracotta",
        "#4A3B5B",
        "opaque"
    ),
    block!(
        "brown_terracotta",
        BROWN_TERRACOTTA,
        "terracotta",
        "#4D3323",
        "opaque"
    ),
    block!(
        "green_terracotta",
        GREEN_TERRACOTTA,
        "terracotta",
        "#4C522A",
        "opaque"
    ),
    block!(
        "red_terracotta",
        RED_TERRACOTTA,
        "terracotta",
        "#8E3C2E",
        "opaque"
    ),
    block!(
        "black_terracotta",
        BLACK_TERRACOTTA,
        "terracotta",
        "#251610",
        "opaque"
    ),
    // Stained glass dye family.
    block!(
        "white_stained_glass",
        WHITE_STAINED_GLASS,
        "stained_glass",
        "#F0F5F5",
        "translucent"
    ),
    block!(
        "orange_stained_glass",
        ORANGE_STAINED_GLASS,
        "stained_glass",
        "#F9801D",
        "translucent"
    ),
    block!(
        "magenta_stained_glass",
        MAGENTA_STAINED_GLASS,
        "stained_glass",
        "#C74EBD",
        "translucent"
    ),
    block!(
        "light_blue_stained_glass",
        LIGHT_BLUE_STAINED_GLASS,
        "stained_glass",
        "#3AB3DA",
        "translucent"
    ),
    block!(
        "yellow_stained_glass",
        YELLOW_STAINED_GLASS,
        "stained_glass",
        "#FED83D",
        "translucent"
    ),
    block!(
        "lime_stained_glass",
        LIME_STAINED_GLASS,
        "stained_glass",
        "#80C71F",
        "translucent"
    ),
    block!(
        "pink_stained_glass",
        PINK_STAINED_GLASS,
        "stained_glass",
        "#F38BAA",
        "translucent"
    ),
    block!(
        "gray_stained_glass",
        GRAY_STAINED_GLASS,
        "stained_glass",
        "#474F52",
        "translucent"
    ),
    block!(
        "light_gray_stained_glass",
        LIGHT_GRAY_STAINED_GLASS,
        "stained_glass",
        "#9D9D97",
        "translucent"
    ),
    block!(
        "cyan_stained_glass",
        CYAN_STAINED_GLASS,
        "stained_glass",
        "#169C9C",
        "translucent"
    ),
    block!(
        "purple_stained_glass",
        PURPLE_STAINED_GLASS,
        "stained_glass",
        "#8932B8",
        "translucent"
    ),
    block!(
        "blue_stained_glass",
        BLUE_STAINED_GLASS,
        "stained_glass",
        "#3C44AA",
        "translucent"
    ),
    block!(
        "brown_stained_glass",
        BROWN_STAINED_GLASS,
        "stained_glass",
        "#835432",
        "translucent"
    ),
    block!(
        "green_stained_glass",
        GREEN_STAINED_GLASS,
        "stained_glass",
        "#5E7C16",
        "translucent"
    ),
    block!(
        "red_stained_glass",
        RED_STAINED_GLASS,
        "stained_glass",
        "#B02E26",
        "translucent"
    ),
    block!(
        "black_stained_glass",
        BLACK_STAINED_GLASS,
        "stained_glass",
        "#1D1D21",
        "translucent"
    ),
    // Remaining plank family members.
    block!(
        "spruce_planks",
        SPRUCE_PLANKS,
        "planks",
        "#73532F",
        "opaque"
    ),
    block!("birch_planks", BIRCH_PLANKS, "planks", "#C7B77A", "opaque"),
    block!(
        "jungle_planks",
        JUNGLE_PLANKS,
        "planks",
        "#A17350",
        "opaque"
    ),
    block!(
        "acacia_planks",
        ACACIA_PLANKS,
        "planks",
        "#A85A32",
        "opaque"
    ),
    block!(
        "dark_oak_planks",
        DARK_OAK_PLANKS,
        "planks",
        "#4A321C",
        "opaque"
    ),
    block!(
        "mangrove_planks",
        MANGROVE_PLANKS,
        "planks",
        "#763B35",
        "opaque"
    ),
    block!(
        "cherry_planks",
        CHERRY_PLANKS,
        "planks",
        "#D69A9A",
        "opaque"
    ),
    block!(
        "bamboo_planks",
        BAMBOO_PLANKS,
        "planks",
        "#C0A64B",
        "opaque"
    ),
    block!(
        "crimson_planks",
        CRIMSON_PLANKS,
        "planks",
        "#653147",
        "opaque"
    ),
    block!(
        "warped_planks",
        WARPED_PLANKS,
        "planks",
        "#2B716B",
        "opaque"
    ),
    // Selected stable full building blocks.
    block!("smooth_stone", SMOOTH_STONE, "masonry", "#9E9E98", "opaque"),
    block!("stone_bricks", STONE_BRICKS, "masonry", "#7B7B74", "opaque"),
    block!("bricks", BRICKS, "masonry", "#9B4A36", "opaque"),
    block!("granite", GRANITE, "masonry", "#956755", "opaque"),
    block!(
        "polished_granite",
        POLISHED_GRANITE,
        "masonry",
        "#9C6B5A",
        "opaque"
    ),
    block!("diorite", DIORITE, "masonry", "#BEBDB8", "opaque"),
    block!(
        "polished_diorite",
        POLISHED_DIORITE,
        "masonry",
        "#C7C7C4",
        "opaque"
    ),
    block!("andesite", ANDESITE, "masonry", "#888985", "opaque"),
    block!(
        "polished_andesite",
        POLISHED_ANDESITE,
        "masonry",
        "#848683",
        "opaque"
    ),
    block!("calcite", CALCITE, "mineral", "#DFE0D2", "opaque"),
    block!("quartz_block", QUARTZ_BLOCK, "mineral", "#E7E3D8", "opaque"),
    block!(
        "smooth_quartz",
        SMOOTH_QUARTZ,
        "mineral",
        "#EEEAE1",
        "opaque"
    ),
    block!("sandstone", SANDSTONE, "masonry", "#D8C487", "opaque"),
    block!(
        "smooth_sandstone",
        SMOOTH_SANDSTONE,
        "masonry",
        "#D9C98C",
        "opaque"
    ),
    block!(
        "red_sandstone",
        RED_SANDSTONE,
        "masonry",
        "#B7612B",
        "opaque"
    ),
    block!(
        "smooth_red_sandstone",
        SMOOTH_RED_SANDSTONE,
        "masonry",
        "#B96832",
        "opaque"
    ),
    block!(
        "cobbled_deepslate",
        COBBLED_DEEPSLATE,
        "masonry",
        "#515157",
        "opaque"
    ),
    block!(
        "polished_deepslate",
        POLISHED_DEEPSLATE,
        "masonry",
        "#4D4D53",
        "opaque"
    ),
    block!(
        "deepslate_bricks",
        DEEPSLATE_BRICKS,
        "masonry",
        "#47474C",
        "opaque"
    ),
    block!("blackstone", BLACKSTONE, "masonry", "#2A232A", "opaque"),
    block!(
        "polished_blackstone",
        POLISHED_BLACKSTONE,
        "masonry",
        "#353038",
        "opaque"
    ),
    block!(
        "prismarine_bricks",
        PRISMARINE_BRICKS,
        "masonry",
        "#63A79E",
        "opaque"
    ),
    block!(
        "dark_prismarine",
        DARK_PRISMARINE,
        "masonry",
        "#335B4B",
        "opaque"
    ),
    block!("mud_bricks", MUD_BRICKS, "masonry", "#89674F", "opaque"),
    block!("obsidian", OBSIDIAN, "mineral", "#15101F", "opaque"),
    block!("iron_block", IRON_BLOCK, "mineral", "#D8D8D8", "opaque"),
    block!("gold_block", GOLD_BLOCK, "mineral", "#F6CF42", "opaque"),
    block!("coal_block", COAL_BLOCK, "mineral", "#151515", "opaque"),
    block!("lapis_block", LAPIS_BLOCK, "mineral", "#1E438C", "opaque"),
    block!(
        "diamond_block",
        DIAMOND_BLOCK,
        "mineral",
        "#62D6CD",
        "opaque"
    ),
    block!(
        "emerald_block",
        EMERALD_BLOCK,
        "mineral",
        "#2BB56A",
        "opaque"
    ),
    block!("sea_lantern", SEA_LANTERN, "light", "#ACD3C0", "opaque"),
    block!("glowstone", GLOWSTONE, "light", "#AA7B3C", "opaque"),
    block!("snow_block", SNOW_BLOCK, "mineral", "#F4F8F8", "opaque"),
    block!(
        "tinted_glass",
        TINTED_GLASS,
        "glass",
        "#2B2535",
        "translucent"
    ),
];

pub fn build_palette_entries() -> Vec<BuildPaletteEntry> {
    let mut entries = BUILD_PALETTE.to_vec();
    entries.sort_unstable_by_key(|entry| entry.name);
    entries
}

pub fn block_state_for_name(name: &str) -> Option<BlockState> {
    BUILD_PALETTE
        .iter()
        .find(|entry| entry.name == name)
        .map(|entry| entry.block)
}

pub fn parse_build_block_name(name: &str) -> Option<BlockState> {
    let trimmed = name.trim();
    let normalized = trimmed
        .strip_prefix("minecraft:")
        .unwrap_or(trimmed)
        .to_ascii_lowercase();
    block_state_for_name(&normalized)
}

pub fn block_name_for_state(block: BlockState) -> Option<&'static str> {
    BUILD_PALETTE
        .iter()
        .find(|entry| entry.block == block)
        .map(|entry| entry.name)
}

pub fn contains_block_state(block: BlockState) -> bool {
    block_name_for_state(block).is_some()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn palette_has_stable_unique_metadata_and_default_states() {
        assert_eq!(BUILD_PALETTE.len(), BUILD_PALETTE_BLOCK_COUNT);

        let mut names = BTreeSet::new();
        let mut states = BTreeSet::new();
        for entry in BUILD_PALETTE {
            assert!(names.insert(entry.name), "duplicate name {}", entry.name);
            assert!(
                states.insert(entry.block.to_raw()),
                "duplicate state {}",
                entry.name
            );
            assert_eq!(entry.block, entry.block.to_kind().to_state());
            assert!(
                entry.representative_rgb.len() == 7
                    && entry.representative_rgb.starts_with('#')
                    && entry.representative_rgb[1..]
                        .bytes()
                        .all(|byte| byte.is_ascii_hexdigit()),
                "invalid color for {}",
                entry.name
            );
            assert!(matches!(entry.opacity, "opaque" | "translucent"));
        }
    }

    #[test]
    fn public_entries_are_sorted_and_lookup_round_trips() {
        let entries = build_palette_entries();
        assert!(entries.windows(2).all(|pair| pair[0].name < pair[1].name));
        for entry in entries {
            assert_eq!(block_state_for_name(entry.name), Some(entry.block));
            assert_eq!(block_name_for_state(entry.block), Some(entry.name));
            assert_eq!(
                parse_build_block_name(&format!("minecraft:{}", entry.name)),
                Some(entry.block)
            );
        }
    }

    #[test]
    fn unsafe_or_stateful_blocks_are_not_in_the_palette() {
        for state in [
            BlockState::WATER,
            BlockState::SAND,
            BlockState::RED_CONCRETE_POWDER,
            BlockState::REDSTONE_BLOCK,
            BlockState::CHEST,
            BlockState::OAK_DOOR,
            BlockState::STONE_STAIRS,
        ] {
            assert!(!contains_block_state(state), "unexpectedly allowed {state}");
        }
    }
}
