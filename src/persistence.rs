use std::collections::BTreeMap;
use std::env;
use std::error::Error;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::Seek;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use fastanvil::Region as AnvilRegion;
use fastnbt::LongArray;
use rusqlite::{params, Connection, DatabaseName, OptionalExtension};
use serde::{Deserialize, Serialize};
use valence::block::{PropName, PropValue};
use valence::prelude::{BlockKind, BlockPos, BlockState, Resource};

use crate::world_command::{base_block_state_at, ChunkColumn, WorldCommand, WORLD_BOUNDS};

pub const DATABASE_PATH_ENV: &str = "WORLD_LOOM_DB_PATH";
pub const REGION_DIR_ENV: &str = "WORLD_LOOM_REGION_DIR";
pub const DEFAULT_DATABASE_PATH: &str = "data/world-loom.sqlite3";
pub const DEFAULT_REGION_DIR: &str = "data/anvil/region";
pub const WORLD_ID: &str = "default";
pub const STORAGE_BACKEND: &str = "anvil_chunk_sqlite_metadata";
pub const SQLITE_DELTA_BACKEND: &str = "sqlite_delta";
pub const ANVIL_STORAGE_BACKEND: &str = "anvil_chunk";
pub const SCHEMA_VERSION: i64 = 6;
pub const SAVE_FORMAT_VERSION: i64 = 3;

const WRITE_FLUSH_INTERVAL: Duration = Duration::from_millis(250);
const WRITE_BATCH_LIMIT: usize = 256;
const REGION_SIZE_CHUNKS: i32 = 32;
const ANVIL_DATA_VERSION_1_20_1: i32 = 3465;
const ANVIL_CHUNK_STATUS: &str = "minecraft:full";
const ANVIL_DEFAULT_BIOME: &str = "minecraft:plains";

pub type PersistenceResult<T> = Result<T, PersistenceError>;

#[derive(Debug)]
pub enum PersistenceError {
    Io(std::io::Error),
    Sqlite(rusqlite::Error),
    Anvil(fastanvil::Error),
    Nbt(fastnbt::error::Error),
    InvalidAnvilChunk(String),
    InvalidBlockState { position: BlockPos, raw: i64 },
    UnsupportedSchemaVersion { found: i64, supported: i64 },
    UnsupportedSaveFormatVersion { found: i64, supported: i64 },
    WriterPanic,
}

impl fmt::Display for PersistenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "I/O error: {err}"),
            Self::Sqlite(err) => write!(f, "SQLite error: {err}"),
            Self::Anvil(err) => write!(f, "Anvil region error: {err}"),
            Self::Nbt(err) => write!(f, "Anvil NBT error: {err}"),
            Self::InvalidAnvilChunk(reason) => write!(f, "invalid Anvil chunk: {reason}"),
            Self::InvalidBlockState { position, raw } => {
                write!(f, "invalid block state raw id {raw} at {position:?}")
            }
            Self::UnsupportedSchemaVersion { found, supported } => write!(
                f,
                "SQLite schema version {found} is newer than supported version {supported}"
            ),
            Self::UnsupportedSaveFormatVersion { found, supported } => write!(
                f,
                "save format version {found} is newer than supported version {supported}"
            ),
            Self::WriterPanic => write!(f, "SQLite writer thread panicked"),
        }
    }
}

impl Error for PersistenceError {}

impl From<std::io::Error> for PersistenceError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

impl From<rusqlite::Error> for PersistenceError {
    fn from(err: rusqlite::Error) -> Self {
        Self::Sqlite(err)
    }
}

impl From<fastanvil::Error> for PersistenceError {
    fn from(err: fastanvil::Error) -> Self {
        Self::Anvil(err)
    }
}

impl From<fastnbt::error::Error> for PersistenceError {
    fn from(err: fastnbt::error::Error) -> Self {
        Self::Nbt(err)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PersistedCommandKind {
    SetBlock,
    RemoveBlock,
}

impl PersistedCommandKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::SetBlock => "set_block",
            Self::RemoveBlock => "remove_block",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockEdit {
    pub position: BlockPos,
    pub block: BlockState,
    pub base_block: BlockState,
    pub command_kind: PersistedCommandKind,
}

impl BlockEdit {
    pub fn from_world_command(command: WorldCommand, final_block: BlockState) -> Self {
        let (position, command_kind) = match command {
            WorldCommand::SetBlock { position, .. } => (position, PersistedCommandKind::SetBlock),
            WorldCommand::RemoveBlock { position } => (position, PersistedCommandKind::RemoveBlock),
        };

        Self {
            position,
            block: final_block,
            base_block: base_block_state_at(WORLD_BOUNDS, position).unwrap_or(BlockState::AIR),
            command_kind,
        }
    }

    fn needs_override(self) -> bool {
        self.block != self.base_block
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SavedBlockOverride {
    pub position: BlockPos,
    pub block: BlockState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredChunk {
    pub column: ChunkColumn,
    pub blocks: Vec<SavedBlockOverride>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildBlockRecord {
    pub position: BlockPos,
    pub original_block: BlockState,
    pub applied_block: BlockState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewBuildRecord {
    pub build_id: String,
    pub idempotency_key: String,
    pub plan_hash: String,
    pub canonical_plan_json: String,
    pub actor: String,
    pub blocks: Vec<BuildBlockRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredBuild {
    pub build_id: String,
    pub idempotency_key: String,
    pub plan_hash: String,
    pub canonical_plan_json: String,
    pub actor: String,
    pub state: String,
    pub changed: usize,
    pub skipped: usize,
    pub created_at: String,
    pub applied_at: String,
    pub undone_at: Option<String>,
    pub blocks: Vec<BuildBlockRecord>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StoredAssetDefinition {
    pub asset_id: String,
    pub version: u32,
    pub spec_hash: String,
    pub canonical_spec_json: String,
    pub budget_json: String,
    pub actor: String,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StoredAssetInstance {
    pub instance_id: String,
    pub idempotency_key: String,
    pub asset_id: String,
    pub version: u32,
    pub position: [f64; 3],
    pub rotation_degrees: [f64; 3],
    pub scale: [f64; 3],
    pub collision_json: String,
    pub interaction_state_json: String,
    pub owner: String,
    pub created_at: String,
    pub updated_at: String,
    pub removed_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupManifest {
    pub source_path: PathBuf,
    pub backup_path: PathBuf,
    pub region_source_path: PathBuf,
    pub region_backup_path: PathBuf,
    pub storage_backend: &'static str,
    pub schema_version: i64,
    pub save_format_version: i64,
}

pub trait WorldStorage {
    fn backend_name(&self) -> &'static str;
    fn db_path(&self) -> &Path;
    fn region_dir(&self) -> Option<&Path>;
    fn schema_version(&self) -> i64;
    fn save_format_version(&self) -> i64;
    fn load_chunk(&self, column: ChunkColumn) -> PersistenceResult<Option<StoredChunk>>;
    fn persist_block_edits(&self, edits: &[BlockEdit]) -> PersistenceResult<()>;
    fn create_backup(
        &self,
        backup_path: &Path,
        region_backup_path: &Path,
    ) -> PersistenceResult<BackupManifest>;
}

#[derive(Debug, Clone)]
pub struct SqliteDeltaStorage {
    db_path: PathBuf,
}

impl SqliteDeltaStorage {
    pub fn open(path: impl Into<PathBuf>) -> PersistenceResult<Self> {
        let db_path = path.into();
        ensure_parent_dir(&db_path)?;
        let mut conn = Connection::open(&db_path)?;
        initialize_schema(&mut conn)?;
        Ok(Self { db_path })
    }
}

impl WorldStorage for SqliteDeltaStorage {
    fn backend_name(&self) -> &'static str {
        SQLITE_DELTA_BACKEND
    }

    fn db_path(&self) -> &Path {
        &self.db_path
    }

    fn region_dir(&self) -> Option<&Path> {
        None
    }

    fn schema_version(&self) -> i64 {
        SCHEMA_VERSION
    }

    fn save_format_version(&self) -> i64 {
        SAVE_FORMAT_VERSION
    }

    fn load_chunk(&self, column: ChunkColumn) -> PersistenceResult<Option<StoredChunk>> {
        let mut conn = Connection::open(&self.db_path)?;
        initialize_schema(&mut conn)?;
        let blocks = load_chunk_overrides_from_connection(&conn, column)?;
        if blocks.is_empty() {
            Ok(None)
        } else {
            Ok(Some(StoredChunk { column, blocks }))
        }
    }

    fn persist_block_edits(&self, edits: &[BlockEdit]) -> PersistenceResult<()> {
        let mut conn = Connection::open(&self.db_path)?;
        initialize_schema(&mut conn)?;
        flush_sqlite_delta_edits(&mut conn, edits)
    }

    fn create_backup(
        &self,
        backup_path: &Path,
        region_backup_path: &Path,
    ) -> PersistenceResult<BackupManifest> {
        ensure_parent_dir(backup_path)?;
        if backup_path.exists() {
            fs::remove_file(backup_path)?;
        }

        let mut conn = Connection::open(&self.db_path)?;
        initialize_schema(&mut conn)?;
        conn.backup(DatabaseName::Main, backup_path, None)?;
        fs::create_dir_all(region_backup_path)?;

        Ok(BackupManifest {
            source_path: self.db_path.clone(),
            backup_path: backup_path.to_path_buf(),
            region_source_path: PathBuf::new(),
            region_backup_path: region_backup_path.to_path_buf(),
            storage_backend: self.backend_name(),
            schema_version: self.schema_version(),
            save_format_version: self.save_format_version(),
        })
    }
}

#[derive(Debug, Clone)]
pub struct AnvilChunkStorage {
    region_dir: PathBuf,
}

impl AnvilChunkStorage {
    pub fn open(path: impl Into<PathBuf>) -> PersistenceResult<Self> {
        let region_dir = path.into();
        fs::create_dir_all(&region_dir)?;
        Ok(Self { region_dir })
    }

    pub fn region_dir(&self) -> &Path {
        &self.region_dir
    }

    pub fn load_chunk(&self, column: ChunkColumn) -> PersistenceResult<Option<StoredChunk>> {
        let (region_x, region_z) = column.region_coords(REGION_SIZE_CHUNKS);
        let path = self.region_file_path(region_x, region_z);
        if !path.exists() {
            return Ok(None);
        }

        let file = File::open(path)?;
        let mut region = AnvilRegion::from_stream(file)?;
        read_chunk_from_anvil_region(&mut region, column)
    }

    pub fn persist_block_edits(&self, edits: &[BlockEdit]) -> PersistenceResult<usize> {
        if edits.is_empty() {
            return Ok(0);
        }

        let mut by_region: BTreeMap<(i32, i32), BTreeMap<ChunkColumn, Vec<&BlockEdit>>> =
            BTreeMap::new();
        for edit in edits {
            let column = ChunkColumn::from_block_pos(edit.position);
            by_region
                .entry(column.region_coords(REGION_SIZE_CHUNKS))
                .or_default()
                .entry(column)
                .or_default()
                .push(edit);
        }

        let mut dirty_chunks = 0;
        for ((region_x, region_z), region_edits) in by_region {
            let path = self.region_file_path(region_x, region_z);
            let mut region = self.open_region_for_write(region_x, region_z)?;

            for (column, chunk_edits) in region_edits {
                let mut blocks = read_chunk_from_anvil_region(&mut region, column)?
                    .map(|chunk| chunk.blocks)
                    .unwrap_or_default();
                apply_block_edits_to_overrides(&mut blocks, &chunk_edits);

                let (local_x, local_z) = local_chunk_coords(column);
                if blocks.is_empty() {
                    region.remove_chunk(local_x, local_z)?;
                } else {
                    let bytes = encode_anvil_chunk(column, &blocks)?;
                    region.write_chunk(local_x, local_z, &bytes)?;
                }

                dirty_chunks += 1;
            }

            let mut file = region.into_inner()?;
            let len = file.stream_position()?;
            file.set_len(len)?;

            if !anvil_region_has_chunks(&path)? {
                fs::remove_file(path)?;
            }
        }

        Ok(dirty_chunks)
    }

    pub fn backup_to(&self, target_dir: &Path) -> PersistenceResult<()> {
        if target_dir.exists() {
            fs::remove_dir_all(target_dir)?;
        }
        copy_dir_recursive(&self.region_dir, target_dir)
    }

    fn open_region_for_write(
        &self,
        region_x: i32,
        region_z: i32,
    ) -> PersistenceResult<AnvilRegion<File>> {
        fs::create_dir_all(&self.region_dir)?;
        let path = self.region_file_path(region_x, region_z);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;

        if file.metadata()?.len() == 0 {
            Ok(AnvilRegion::create(file)?)
        } else {
            Ok(AnvilRegion::from_stream(file)?)
        }
    }

    fn region_file_path(&self, region_x: i32, region_z: i32) -> PathBuf {
        self.region_dir.join(format!("r.{region_x}.{region_z}.mca"))
    }
}

#[derive(Debug, Clone)]
pub struct HybridAnvilStorage {
    sqlite: SqliteDeltaStorage,
    anvil: AnvilChunkStorage,
}

impl HybridAnvilStorage {
    pub fn open(
        db_path: impl Into<PathBuf>,
        region_dir: impl Into<PathBuf>,
    ) -> PersistenceResult<Self> {
        Ok(Self {
            sqlite: SqliteDeltaStorage::open(db_path)?,
            anvil: AnvilChunkStorage::open(region_dir)?,
        })
    }

    pub fn anvil_storage(&self) -> &AnvilChunkStorage {
        &self.anvil
    }
}

impl WorldStorage for HybridAnvilStorage {
    fn backend_name(&self) -> &'static str {
        STORAGE_BACKEND
    }

    fn db_path(&self) -> &Path {
        self.sqlite.db_path()
    }

    fn region_dir(&self) -> Option<&Path> {
        Some(self.anvil.region_dir())
    }

    fn schema_version(&self) -> i64 {
        SCHEMA_VERSION
    }

    fn save_format_version(&self) -> i64 {
        SAVE_FORMAT_VERSION
    }

    fn load_chunk(&self, column: ChunkColumn) -> PersistenceResult<Option<StoredChunk>> {
        let anvil = self.anvil.load_chunk(column)?;
        let sqlite = self.sqlite.load_chunk(column)?;
        if anvil.is_none() && sqlite.is_none() {
            return Ok(None);
        }

        let mut merged = BTreeMap::new();
        for saved in anvil
            .into_iter()
            .flat_map(|chunk| chunk.blocks)
            .chain(sqlite.into_iter().flat_map(|chunk| chunk.blocks))
        {
            merged.insert(
                (saved.position.x, saved.position.y, saved.position.z),
                saved,
            );
        }
        Ok(Some(StoredChunk {
            column,
            blocks: merged.into_values().collect(),
        }))
    }

    fn persist_block_edits(&self, edits: &[BlockEdit]) -> PersistenceResult<()> {
        self.anvil.persist_block_edits(edits)?;
        let mut conn = Connection::open(self.sqlite.db_path())?;
        initialize_schema(&mut conn)?;
        flush_command_log_and_dirty_index(&mut conn, edits)?;
        Ok(())
    }

    fn create_backup(
        &self,
        backup_path: &Path,
        region_backup_path: &Path,
    ) -> PersistenceResult<BackupManifest> {
        ensure_parent_dir(backup_path)?;
        if backup_path.exists() {
            fs::remove_file(backup_path)?;
        }

        let mut conn = Connection::open(self.sqlite.db_path())?;
        initialize_schema(&mut conn)?;
        conn.backup(DatabaseName::Main, backup_path, None)?;
        self.anvil.backup_to(region_backup_path)?;

        let manifest = BackupManifest {
            source_path: self.sqlite.db_path().to_path_buf(),
            backup_path: backup_path.to_path_buf(),
            region_source_path: self.anvil.region_dir().to_path_buf(),
            region_backup_path: region_backup_path.to_path_buf(),
            storage_backend: self.backend_name(),
            schema_version: self.schema_version(),
            save_format_version: self.save_format_version(),
        };
        record_backup_manifest(&mut conn, &manifest)?;
        Ok(manifest)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AnvilChunkNbt {
    #[serde(rename = "DataVersion")]
    data_version: i32,
    #[serde(rename = "xPos")]
    x_pos: i32,
    #[serde(rename = "yPos")]
    y_pos: i32,
    #[serde(rename = "zPos")]
    z_pos: i32,
    #[serde(rename = "Status")]
    status: String,
    #[serde(rename = "LastUpdate")]
    last_update: i64,
    #[serde(rename = "InhabitedTime")]
    inhabited_time: i64,
    #[serde(default)]
    sections: Vec<AnvilSection>,
    #[serde(default)]
    block_entities: Vec<BTreeMap<String, fastnbt::Value>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AnvilSection {
    #[serde(rename = "Y")]
    y: i8,
    block_states: AnvilBlockStates,
    biomes: AnvilBiomes,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AnvilBlockStates {
    palette: Vec<AnvilPaletteEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<LongArray>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AnvilBiomes {
    palette: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<LongArray>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AnvilPaletteEntry {
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "Properties", skip_serializing_if = "Option::is_none")]
    properties: Option<BTreeMap<String, String>>,
}

fn read_chunk_from_anvil_region(
    region: &mut AnvilRegion<File>,
    column: ChunkColumn,
) -> PersistenceResult<Option<StoredChunk>> {
    let (local_x, local_z) = local_chunk_coords(column);
    let Some(bytes) = region.read_chunk(local_x, local_z)? else {
        return Ok(None);
    };

    let chunk: AnvilChunkNbt = fastnbt::from_bytes(&bytes)?;
    let blocks = decode_anvil_chunk_overrides(column, &chunk)?;
    if blocks.is_empty() {
        Ok(None)
    } else {
        Ok(Some(StoredChunk { column, blocks }))
    }
}

fn encode_anvil_chunk(
    column: ChunkColumn,
    blocks: &[SavedBlockOverride],
) -> PersistenceResult<Vec<u8>> {
    let min_section = WORLD_BOUNDS.min_y.div_euclid(16);
    let max_section = WORLD_BOUNDS.max_y.div_euclid(16);
    let override_map = blocks
        .iter()
        .map(|saved| {
            (
                (saved.position.x, saved.position.y, saved.position.z),
                saved.block,
            )
        })
        .collect::<BTreeMap<_, _>>();

    let sections = (min_section..=max_section)
        .map(|section_y| encode_anvil_section(column, section_y, &override_map))
        .collect::<PersistenceResult<Vec<_>>>()?;

    let chunk = AnvilChunkNbt {
        data_version: ANVIL_DATA_VERSION_1_20_1,
        x_pos: column.x,
        y_pos: min_section,
        z_pos: column.z,
        status: ANVIL_CHUNK_STATUS.to_string(),
        last_update: 0,
        inhabited_time: 0,
        sections,
        block_entities: Vec::new(),
    };

    Ok(fastnbt::to_bytes(&chunk)?)
}

fn encode_anvil_section(
    column: ChunkColumn,
    section_y: i32,
    override_map: &BTreeMap<(i32, i32, i32), BlockState>,
) -> PersistenceResult<AnvilSection> {
    let mut palette = Vec::<BlockState>::new();
    let mut indices = Vec::<usize>::with_capacity(4096);

    for local_y in 0..16 {
        let y = section_y * 16 + local_y;
        for z in 0..16 {
            let world_z = column.z * 16 + z;
            for x in 0..16 {
                let world_x = column.x * 16 + x;
                let position = BlockPos::new(world_x, y, world_z);
                let state = override_map
                    .get(&(world_x, y, world_z))
                    .copied()
                    .unwrap_or_else(|| {
                        base_block_state_at(WORLD_BOUNDS, position).unwrap_or(BlockState::AIR)
                    });

                let index = palette
                    .iter()
                    .position(|existing| *existing == state)
                    .unwrap_or_else(|| {
                        palette.push(state);
                        palette.len() - 1
                    });
                indices.push(index);
            }
        }
    }

    let data = (palette.len() > 1).then(|| pack_palette_indices(&indices, palette.len()));
    let palette = palette
        .into_iter()
        .map(anvil_palette_entry_for_block)
        .collect();

    Ok(AnvilSection {
        y: i8::try_from(section_y).map_err(|_| {
            PersistenceError::InvalidAnvilChunk(format!(
                "section y {section_y} cannot fit in Anvil byte"
            ))
        })?,
        block_states: AnvilBlockStates { palette, data },
        biomes: AnvilBiomes {
            palette: vec![ANVIL_DEFAULT_BIOME.to_string()],
            data: None,
        },
    })
}

fn decode_anvil_chunk_overrides(
    column: ChunkColumn,
    chunk: &AnvilChunkNbt,
) -> PersistenceResult<Vec<SavedBlockOverride>> {
    let mut blocks = Vec::new();

    for section in &chunk.sections {
        let palette = section
            .block_states
            .palette
            .iter()
            .map(block_state_from_anvil_palette_entry)
            .collect::<PersistenceResult<Vec<_>>>()?;
        if palette.is_empty() {
            return Err(PersistenceError::InvalidAnvilChunk(format!(
                "chunk {},{} has a section with empty block palette",
                column.x, column.z
            )));
        }

        for index in 0..4096 {
            let palette_index =
                unpack_palette_index(section.block_states.data.as_ref(), palette.len(), index)?;
            let Some(block) = palette.get(palette_index).copied() else {
                return Err(PersistenceError::InvalidAnvilChunk(format!(
                    "chunk {},{} palette index {palette_index} is out of bounds",
                    column.x, column.z
                )));
            };

            let local_x = (index % 16) as i32;
            let local_z = ((index / 16) % 16) as i32;
            let local_y = (index / (16 * 16)) as i32;
            let position = BlockPos::new(
                column.x * 16 + local_x,
                i32::from(section.y) * 16 + local_y,
                column.z * 16 + local_z,
            );
            if !WORLD_BOUNDS.contains(position) {
                continue;
            }

            let base = base_block_state_at(WORLD_BOUNDS, position).unwrap_or(BlockState::AIR);
            if block != base {
                blocks.push(SavedBlockOverride { position, block });
            }
        }
    }

    blocks.sort_by_key(|block| (block.position.x, block.position.y, block.position.z));
    Ok(blocks)
}

fn anvil_palette_entry_for_block(block: BlockState) -> AnvilPaletteEntry {
    let kind = block.to_kind();
    let properties = kind
        .props()
        .iter()
        .map(|prop| {
            (
                prop.to_str().to_string(),
                block
                    .get(*prop)
                    .expect("block property should exist")
                    .to_str()
                    .to_string(),
            )
        })
        .collect::<BTreeMap<_, _>>();

    AnvilPaletteEntry {
        name: format!("minecraft:{}", kind.to_str()),
        properties: (!properties.is_empty()).then_some(properties),
    }
}

fn block_state_from_anvil_palette_entry(
    entry: &AnvilPaletteEntry,
) -> PersistenceResult<BlockState> {
    let name = entry
        .name
        .strip_prefix("minecraft:")
        .unwrap_or(entry.name.as_str());
    let Some(kind) = BlockKind::from_str(name) else {
        return Err(PersistenceError::InvalidAnvilChunk(format!(
            "unknown block kind {}",
            entry.name
        )));
    };

    let mut state = kind.to_state();
    if let Some(properties) = &entry.properties {
        for (name, value) in properties {
            let Some(prop_name) = PropName::from_str(name) else {
                return Err(PersistenceError::InvalidAnvilChunk(format!(
                    "unknown block property {name} on {}",
                    entry.name
                )));
            };
            let Some(prop_value) = PropValue::from_str(value) else {
                return Err(PersistenceError::InvalidAnvilChunk(format!(
                    "unknown block property value {value} on {}",
                    entry.name
                )));
            };
            state = state.set(prop_name, prop_value);
        }
    }

    Ok(state)
}

fn apply_block_edits_to_overrides(blocks: &mut Vec<SavedBlockOverride>, edits: &[&BlockEdit]) {
    let mut by_position = blocks
        .iter()
        .map(|block| {
            (
                (block.position.x, block.position.y, block.position.z),
                block.block,
            )
        })
        .collect::<BTreeMap<_, _>>();

    for edit in edits {
        let key = (edit.position.x, edit.position.y, edit.position.z);
        if edit.needs_override() {
            by_position.insert(key, edit.block);
        } else {
            by_position.remove(&key);
        }
    }

    *blocks = by_position
        .into_iter()
        .map(|((x, y, z), block)| SavedBlockOverride {
            position: BlockPos::new(x, y, z),
            block,
        })
        .collect();
}

fn pack_palette_indices(indices: &[usize], palette_len: usize) -> LongArray {
    let bits = bits_per_palette_index(palette_len).max(4);
    let values_per_long = 64 / bits;
    let long_count = indices.len().div_ceil(values_per_long);
    let mut longs = vec![0_i64; long_count];

    for (index, palette_index) in indices.iter().copied().enumerate() {
        let long_index = index / values_per_long;
        let shift = (index % values_per_long) * bits;
        longs[long_index] |= ((palette_index as u64) << shift) as i64;
    }

    LongArray::new(longs)
}

fn unpack_palette_index(
    data: Option<&LongArray>,
    palette_len: usize,
    index: usize,
) -> PersistenceResult<usize> {
    if data.is_none() && palette_len == 1 {
        return Ok(0);
    }

    let Some(data) = data else {
        return Err(PersistenceError::InvalidAnvilChunk(
            "multi-entry block palette is missing packed data".to_string(),
        ));
    };

    let bits = bits_per_palette_index(palette_len).max(4);
    let values_per_long = 64 / bits;
    let long_index = index / values_per_long;
    let Some(long) = data.get(long_index) else {
        return Err(PersistenceError::InvalidAnvilChunk(format!(
            "packed block data is too short for index {index}"
        )));
    };

    let shift = (index % values_per_long) * bits;
    let mask = (1_u64 << bits) - 1;
    Ok(((*long as u64) >> shift & mask) as usize)
}

fn bits_per_palette_index(palette_len: usize) -> usize {
    usize::BITS as usize - (palette_len.saturating_sub(1)).leading_zeros() as usize
}

fn local_chunk_coords(column: ChunkColumn) -> (usize, usize) {
    (
        column.x.rem_euclid(REGION_SIZE_CHUNKS) as usize,
        column.z.rem_euclid(REGION_SIZE_CHUNKS) as usize,
    )
}

fn anvil_region_has_chunks(path: &Path) -> PersistenceResult<bool> {
    if !path.exists() {
        return Ok(false);
    }

    let file = File::open(path)?;
    let mut region = AnvilRegion::from_stream(file)?;
    Ok(region.iter().next().transpose()?.is_some())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistenceStatsSnapshot {
    pub storage_backend: &'static str,
    pub schema_version: i64,
    pub save_format_version: i64,
    pub loaded_legacy_overrides: usize,
    pub pending_edits: usize,
    pub flushed_edits: u64,
    pub flush_batches: u64,
    pub failed_flushes: u64,
    pub dirty_chunks: usize,
}

#[derive(Debug, Clone, Default)]
struct PersistenceStats {
    pending_edits: Arc<AtomicUsize>,
    flushed_edits: Arc<AtomicU64>,
    flush_batches: Arc<AtomicU64>,
    failed_flushes: Arc<AtomicU64>,
}

impl PersistenceStats {
    fn record_queued(&self) {
        self.pending_edits.fetch_add(1, Ordering::Relaxed);
    }

    fn record_queue_failed(&self) {
        self.pending_edits.fetch_sub(1, Ordering::Relaxed);
    }

    fn record_flushed(&self, count: usize) {
        if count == 0 {
            return;
        }
        self.pending_edits.fetch_sub(count, Ordering::Relaxed);
        self.flushed_edits
            .fetch_add(count as u64, Ordering::Relaxed);
        self.flush_batches.fetch_add(1, Ordering::Relaxed);
    }

    fn record_failed_flush(&self) {
        self.failed_flushes.fetch_add(1, Ordering::Relaxed);
    }

    fn snapshot(&self, storage: &PersistenceRuntime) -> PersistenceStatsSnapshot {
        PersistenceStatsSnapshot {
            storage_backend: storage.storage_backend,
            schema_version: storage.schema_version,
            save_format_version: storage.save_format_version,
            loaded_legacy_overrides: storage.loaded_legacy_overrides,
            pending_edits: self.pending_edits.load(Ordering::Relaxed),
            flushed_edits: self.flushed_edits.load(Ordering::Relaxed),
            flush_batches: self.flush_batches.load(Ordering::Relaxed),
            failed_flushes: self.failed_flushes.load(Ordering::Relaxed),
            dirty_chunks: storage.dirty_chunk_count().unwrap_or_default(),
        }
    }
}

pub struct PersistenceRuntime {
    storage: HybridAnvilStorage,
    db_path: PathBuf,
    region_dir: PathBuf,
    storage_backend: &'static str,
    schema_version: i64,
    save_format_version: i64,
    sender: Sender<PersistenceMessage>,
    loaded_legacy_overrides: usize,
    stats: PersistenceStats,
    writer: Mutex<Option<JoinHandle<()>>>,
}

impl Resource for PersistenceRuntime {}

impl fmt::Debug for PersistenceRuntime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PersistenceRuntime")
            .field("db_path", &self.db_path)
            .field("region_dir", &self.region_dir)
            .field("storage_backend", &self.storage_backend)
            .field("schema_version", &self.schema_version)
            .field("save_format_version", &self.save_format_version)
            .field("loaded_legacy_overrides", &self.loaded_legacy_overrides)
            .finish_non_exhaustive()
    }
}

impl PersistenceRuntime {
    pub fn open_default() -> PersistenceResult<Self> {
        Self::open(default_database_path())
    }

    pub fn open(path: impl Into<PathBuf>) -> PersistenceResult<Self> {
        let db_path = path.into();
        let region_dir = default_region_dir();
        Self::open_with_region_dir(db_path, region_dir)
    }

    pub fn open_with_region_dir(
        db_path: impl Into<PathBuf>,
        region_dir: impl Into<PathBuf>,
    ) -> PersistenceResult<Self> {
        let db_path = db_path.into();
        let region_dir = region_dir.into();
        let storage = HybridAnvilStorage::open(&db_path, &region_dir)?;
        let loaded_legacy_overrides = count_legacy_block_overrides(storage.db_path())?;
        let (sender, receiver) = mpsc::channel();
        let writer_storage = storage.clone();
        let stats = PersistenceStats::default();
        let writer_stats = stats.clone();
        let writer = thread::Builder::new()
            .name("world-loom-sqlite-writer".to_string())
            .spawn(move || writer_loop(writer_storage, receiver, writer_stats))
            .map_err(PersistenceError::Io)?;

        Ok(Self {
            storage,
            db_path,
            region_dir,
            storage_backend: STORAGE_BACKEND,
            schema_version: SCHEMA_VERSION,
            save_format_version: SAVE_FORMAT_VERSION,
            sender,
            loaded_legacy_overrides,
            stats,
            writer: Mutex::new(Some(writer)),
        })
    }

    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    pub fn region_dir(&self) -> &Path {
        &self.region_dir
    }

    pub fn loaded_legacy_overrides(&self) -> usize {
        self.loaded_legacy_overrides
    }

    pub fn load_chunk(&self, column: ChunkColumn) -> PersistenceResult<Option<StoredChunk>> {
        self.storage.load_chunk(column)
    }

    pub fn dirty_chunk_count(&self) -> PersistenceResult<usize> {
        dirty_chunk_count(&self.db_path)
    }

    pub fn stats_snapshot(&self) -> PersistenceStatsSnapshot {
        self.stats.snapshot(self)
    }

    pub fn queue_world_command(&self, command: WorldCommand, final_block: BlockState) {
        let edit = BlockEdit::from_world_command(command, final_block);
        self.stats.record_queued();
        if let Err(err) = self.sender.send(PersistenceMessage::Edit(edit)) {
            self.stats.record_queue_failed();
            eprintln!("[world-loom] failed to queue SQLite block edit: {err}");
        }
    }

    pub fn load_build_by_idempotency_key(
        &self,
        idempotency_key: &str,
    ) -> PersistenceResult<Option<StoredBuild>> {
        let mut conn = Connection::open(&self.db_path)?;
        initialize_schema(&mut conn)?;
        load_build_by_column(&conn, "idempotency_key", idempotency_key)
    }

    pub fn load_build(&self, build_id: &str) -> PersistenceResult<Option<StoredBuild>> {
        let mut conn = Connection::open(&self.db_path)?;
        initialize_schema(&mut conn)?;
        load_build_by_column(&conn, "build_id", build_id)
    }

    pub fn persist_build(&self, record: &NewBuildRecord) -> PersistenceResult<StoredBuild> {
        let mut conn = Connection::open(&self.db_path)?;
        initialize_schema(&mut conn)?;
        let tx = conn.transaction()?;
        tx.execute(
            "
            INSERT INTO builds (
                build_id, world_id, idempotency_key, plan_hash,
                canonical_plan_json, actor, state, changed, skipped,
                applied_at
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'applied', ?7, 0, CURRENT_TIMESTAMP)
            ",
            params![
                record.build_id,
                WORLD_ID,
                record.idempotency_key,
                record.plan_hash,
                record.canonical_plan_json,
                record.actor,
                i64::try_from(record.blocks.len()).unwrap_or(i64::MAX),
            ],
        )?;

        for (ordinal, block) in record.blocks.iter().enumerate() {
            tx.execute(
                "
                INSERT INTO build_blocks (
                    build_id, ordinal, x, y, z,
                    original_block_state_raw, applied_block_state_raw
                )
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                ",
                params![
                    record.build_id,
                    i64::try_from(ordinal).unwrap_or(i64::MAX),
                    block.position.x,
                    block.position.y,
                    block.position.z,
                    i64::from(block.original_block.to_raw()),
                    i64::from(block.applied_block.to_raw()),
                ],
            )?;
            set_transactional_override(&tx, block.position, block.applied_block)?;
            mark_transactional_chunk_dirty(&tx, block.position)?;
        }

        tx.commit()?;
        load_build_by_column(&conn, "build_id", &record.build_id)?.ok_or_else(|| {
            PersistenceError::InvalidAnvilChunk(
                "committed build could not be loaded from metadata".to_string(),
            )
        })
    }

    pub fn mark_build_undone(&self, build: &StoredBuild) -> PersistenceResult<StoredBuild> {
        let mut conn = Connection::open(&self.db_path)?;
        initialize_schema(&mut conn)?;
        let tx = conn.transaction()?;
        let updated = tx.execute(
            "
            UPDATE builds
            SET state = 'undone', undone_at = CURRENT_TIMESTAMP
            WHERE world_id = ?1 AND build_id = ?2 AND state = 'applied'
            ",
            params![WORLD_ID, build.build_id],
        )?;
        if updated == 0 {
            tx.rollback()?;
            return self.load_build(&build.build_id)?.ok_or_else(|| {
                PersistenceError::InvalidAnvilChunk(
                    "build disappeared while marking undo".to_string(),
                )
            });
        }

        for block in &build.blocks {
            set_transactional_override(&tx, block.position, block.original_block)?;
            mark_transactional_chunk_dirty(&tx, block.position)?;
        }
        tx.commit()?;
        load_build_by_column(&conn, "build_id", &build.build_id)?.ok_or_else(|| {
            PersistenceError::InvalidAnvilChunk(
                "undone build could not be loaded from metadata".to_string(),
            )
        })
    }

    pub fn list_asset_definitions(&self) -> PersistenceResult<Vec<StoredAssetDefinition>> {
        let mut conn = Connection::open(&self.db_path)?;
        initialize_schema(&mut conn)?;
        let mut statement = conn.prepare(
            "
            SELECT asset_id, version, spec_hash, canonical_spec_json,
                   budget_json, actor, created_at
            FROM asset_definitions
            WHERE world_id = ?1
            ORDER BY asset_id, version
            ",
        )?;
        let rows = statement.query_map(params![WORLD_ID], asset_definition_from_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn load_asset_definition(
        &self,
        asset_id: &str,
        version: u32,
    ) -> PersistenceResult<Option<StoredAssetDefinition>> {
        let mut conn = Connection::open(&self.db_path)?;
        initialize_schema(&mut conn)?;
        conn.query_row(
            "
            SELECT asset_id, version, spec_hash, canonical_spec_json,
                   budget_json, actor, created_at
            FROM asset_definitions
            WHERE world_id = ?1 AND asset_id = ?2 AND version = ?3
            ",
            params![WORLD_ID, asset_id, i64::from(version)],
            asset_definition_from_row,
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn persist_asset_definition(
        &self,
        asset_id: &str,
        version: u32,
        spec_hash: &str,
        canonical_spec_json: &str,
        budget_json: &str,
        actor: &str,
    ) -> PersistenceResult<StoredAssetDefinition> {
        let mut conn = Connection::open(&self.db_path)?;
        initialize_schema(&mut conn)?;
        conn.execute(
            "
            INSERT INTO asset_definitions (
                world_id, asset_id, version, spec_hash,
                canonical_spec_json, budget_json, actor
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
            ",
            params![
                WORLD_ID,
                asset_id,
                i64::from(version),
                spec_hash,
                canonical_spec_json,
                budget_json,
                actor,
            ],
        )?;
        conn.query_row(
            "
            SELECT asset_id, version, spec_hash, canonical_spec_json,
                   budget_json, actor, created_at
            FROM asset_definitions
            WHERE world_id = ?1 AND asset_id = ?2 AND version = ?3
            ",
            params![WORLD_ID, asset_id, i64::from(version)],
            asset_definition_from_row,
        )
        .map_err(Into::into)
    }

    pub fn list_asset_instances(&self) -> PersistenceResult<Vec<StoredAssetInstance>> {
        let mut conn = Connection::open(&self.db_path)?;
        initialize_schema(&mut conn)?;
        let mut statement = conn.prepare(&format!(
            "{} WHERE world_id = ?1 AND removed_at IS NULL ORDER BY created_at, instance_id",
            ASSET_INSTANCE_SELECT
        ))?;
        let rows = statement.query_map(params![WORLD_ID], asset_instance_from_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn load_asset_instance(
        &self,
        instance_id: &str,
    ) -> PersistenceResult<Option<StoredAssetInstance>> {
        self.load_asset_instance_by("instance_id", instance_id)
    }

    pub fn load_asset_instance_by_idempotency_key(
        &self,
        idempotency_key: &str,
    ) -> PersistenceResult<Option<StoredAssetInstance>> {
        self.load_asset_instance_by("idempotency_key", idempotency_key)
    }

    fn load_asset_instance_by(
        &self,
        column: &str,
        value: &str,
    ) -> PersistenceResult<Option<StoredAssetInstance>> {
        debug_assert!(matches!(column, "instance_id" | "idempotency_key"));
        let mut conn = Connection::open(&self.db_path)?;
        initialize_schema(&mut conn)?;
        conn.query_row(
            &format!(
                "{} WHERE world_id = ?1 AND {column} = ?2",
                ASSET_INSTANCE_SELECT
            ),
            params![WORLD_ID, value],
            asset_instance_from_row,
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn persist_asset_instance(
        &self,
        instance: &StoredAssetInstance,
    ) -> PersistenceResult<StoredAssetInstance> {
        let mut conn = Connection::open(&self.db_path)?;
        initialize_schema(&mut conn)?;
        conn.execute(
            "
            INSERT INTO asset_instances (
                instance_id, world_id, idempotency_key, asset_id, asset_version,
                position_x, position_y, position_z,
                rotation_x, rotation_y, rotation_z,
                scale_x, scale_y, scale_z,
                collision_json, interaction_state_json, owner
            )
            VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9,
                ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17
            )
            ",
            params![
                instance.instance_id,
                WORLD_ID,
                instance.idempotency_key,
                instance.asset_id,
                i64::from(instance.version),
                instance.position[0],
                instance.position[1],
                instance.position[2],
                instance.rotation_degrees[0],
                instance.rotation_degrees[1],
                instance.rotation_degrees[2],
                instance.scale[0],
                instance.scale[1],
                instance.scale[2],
                instance.collision_json,
                instance.interaction_state_json,
                instance.owner,
            ],
        )?;
        self.load_asset_instance(&instance.instance_id)?
            .ok_or_else(|| {
                PersistenceError::InvalidAnvilChunk(
                    "persisted asset instance could not be loaded".to_string(),
                )
            })
    }

    pub fn update_asset_instance(
        &self,
        instance: &StoredAssetInstance,
    ) -> PersistenceResult<StoredAssetInstance> {
        let mut conn = Connection::open(&self.db_path)?;
        initialize_schema(&mut conn)?;
        let updated = conn.execute(
            "
            UPDATE asset_instances
            SET position_x = ?3, position_y = ?4, position_z = ?5,
                rotation_x = ?6, rotation_y = ?7, rotation_z = ?8,
                scale_x = ?9, scale_y = ?10, scale_z = ?11,
                collision_json = ?12, interaction_state_json = ?13,
                updated_at = CURRENT_TIMESTAMP
            WHERE world_id = ?1 AND instance_id = ?2 AND removed_at IS NULL
            ",
            params![
                WORLD_ID,
                instance.instance_id,
                instance.position[0],
                instance.position[1],
                instance.position[2],
                instance.rotation_degrees[0],
                instance.rotation_degrees[1],
                instance.rotation_degrees[2],
                instance.scale[0],
                instance.scale[1],
                instance.scale[2],
                instance.collision_json,
                instance.interaction_state_json,
            ],
        )?;
        if updated == 0 {
            return Err(PersistenceError::InvalidAnvilChunk(
                "asset instance is missing or removed".to_string(),
            ));
        }
        self.load_asset_instance(&instance.instance_id)?
            .ok_or_else(|| {
                PersistenceError::InvalidAnvilChunk(
                    "updated asset instance could not be loaded".to_string(),
                )
            })
    }

    pub fn remove_asset_instance(
        &self,
        instance_id: &str,
    ) -> PersistenceResult<Option<StoredAssetInstance>> {
        let mut conn = Connection::open(&self.db_path)?;
        initialize_schema(&mut conn)?;
        conn.execute(
            "
            UPDATE asset_instances
            SET removed_at = COALESCE(removed_at, CURRENT_TIMESTAMP),
                updated_at = CURRENT_TIMESTAMP
            WHERE world_id = ?1 AND instance_id = ?2
            ",
            params![WORLD_ID, instance_id],
        )?;
        conn.query_row(
            &format!(
                "{} WHERE world_id = ?1 AND instance_id = ?2",
                ASSET_INSTANCE_SELECT
            ),
            params![WORLD_ID, instance_id],
            asset_instance_from_row,
        )
        .optional()
        .map_err(Into::into)
    }
}

impl Drop for PersistenceRuntime {
    fn drop(&mut self) {
        let _ = self.sender.send(PersistenceMessage::Shutdown);
        if let Ok(mut writer) = self.writer.lock() {
            if let Some(handle) = writer.take() {
                if handle.join().is_err() {
                    eprintln!("[world-loom] SQLite writer thread panicked during shutdown");
                }
            }
        }
    }
}

#[derive(Debug)]
enum PersistenceMessage {
    Edit(BlockEdit),
    Shutdown,
}

pub fn default_database_path() -> PathBuf {
    env::var_os(DATABASE_PATH_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_DATABASE_PATH))
}

pub fn default_region_dir() -> PathBuf {
    env::var_os(REGION_DIR_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_REGION_DIR))
}

pub fn load_block_overrides(path: &Path) -> PersistenceResult<Vec<SavedBlockOverride>> {
    if !path.exists() {
        return Ok(Vec::new());
    }

    load_legacy_block_overrides(path)
}

pub fn persist_block_edits(path: &Path, edits: &[BlockEdit]) -> PersistenceResult<()> {
    HybridAnvilStorage::open(path, default_region_dir())?.persist_block_edits(edits)
}

pub fn backup_database(path: &Path, backup_path: &Path) -> PersistenceResult<BackupManifest> {
    HybridAnvilStorage::open(path, default_region_dir())?
        .create_backup(backup_path, &backup_path.with_extension("regions"))
}

fn writer_loop(
    storage: HybridAnvilStorage,
    receiver: Receiver<PersistenceMessage>,
    stats: PersistenceStats,
) {
    if let Err(err) = run_writer_loop(storage, receiver, &stats) {
        stats.record_failed_flush();
        eprintln!("[world-loom] SQLite writer stopped: {err}");
    }
}

fn run_writer_loop(
    storage: HybridAnvilStorage,
    receiver: Receiver<PersistenceMessage>,
    stats: &PersistenceStats,
) -> PersistenceResult<()> {
    let mut batch = Vec::new();
    loop {
        match receiver.recv_timeout(WRITE_FLUSH_INTERVAL) {
            Ok(PersistenceMessage::Edit(edit)) => {
                batch.push(edit);
                let should_shutdown = drain_pending_messages(&receiver, &mut batch);
                flush_edits_and_record(&storage, &batch, stats)?;
                batch.clear();
                if should_shutdown {
                    return Ok(());
                }
            }
            Ok(PersistenceMessage::Shutdown) => {
                drain_pending_messages(&receiver, &mut batch);
                flush_edits_and_record(&storage, &batch, stats)?;
                return Ok(());
            }
            Err(RecvTimeoutError::Timeout) => {
                if !batch.is_empty() {
                    flush_edits_and_record(&storage, &batch, stats)?;
                    batch.clear();
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                flush_edits_and_record(&storage, &batch, stats)?;
                return Ok(());
            }
        }
    }
}

fn drain_pending_messages(
    receiver: &Receiver<PersistenceMessage>,
    batch: &mut Vec<BlockEdit>,
) -> bool {
    while batch.len() < WRITE_BATCH_LIMIT {
        match receiver.try_recv() {
            Ok(PersistenceMessage::Edit(edit)) => batch.push(edit),
            Ok(PersistenceMessage::Shutdown) => return true,
            Err(mpsc::TryRecvError::Empty) => return false,
            Err(mpsc::TryRecvError::Disconnected) => return true,
        }
    }

    false
}

fn flush_edits_and_record(
    storage: &HybridAnvilStorage,
    edits: &[BlockEdit],
    stats: &PersistenceStats,
) -> PersistenceResult<()> {
    storage.persist_block_edits(edits)?;
    stats.record_flushed(edits.len());
    Ok(())
}

fn initialize_schema(conn: &mut Connection) -> PersistenceResult<()> {
    conn.execute_batch(
        "
        PRAGMA journal_mode = WAL;
        PRAGMA synchronous = NORMAL;
        PRAGMA foreign_keys = ON;

        CREATE TABLE IF NOT EXISTS world_metadata (
            world_id TEXT PRIMARY KEY,
            storage_backend TEXT NOT NULL DEFAULT 'sqlite_delta',
            schema_version INTEGER NOT NULL,
            save_format_version INTEGER NOT NULL DEFAULT 1,
            min_x INTEGER NOT NULL,
            max_x INTEGER NOT NULL,
            min_y INTEGER NOT NULL,
            max_y INTEGER NOT NULL,
            min_z INTEGER NOT NULL,
            max_z INTEGER NOT NULL,
            created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
            updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
        );

        CREATE TABLE IF NOT EXISTS block_overrides (
            world_id TEXT NOT NULL,
            x INTEGER NOT NULL,
            y INTEGER NOT NULL,
            z INTEGER NOT NULL,
            block_state_raw INTEGER NOT NULL,
            updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
            PRIMARY KEY (world_id, x, y, z),
            FOREIGN KEY (world_id) REFERENCES world_metadata(world_id) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS command_log (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            world_id TEXT NOT NULL,
            command_kind TEXT NOT NULL CHECK (command_kind IN ('set_block', 'remove_block')),
            x INTEGER NOT NULL,
            y INTEGER NOT NULL,
            z INTEGER NOT NULL,
            block_state_raw INTEGER NOT NULL,
            recorded_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
            FOREIGN KEY (world_id) REFERENCES world_metadata(world_id) ON DELETE CASCADE
        );

        CREATE INDEX IF NOT EXISTS idx_command_log_world_id_id
            ON command_log (world_id, id);

        CREATE TABLE IF NOT EXISTS dirty_chunks (
            world_id TEXT NOT NULL,
            chunk_x INTEGER NOT NULL,
            chunk_z INTEGER NOT NULL,
            dirty_count INTEGER NOT NULL DEFAULT 0,
            last_dirty_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
            last_flushed_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
            PRIMARY KEY (world_id, chunk_x, chunk_z),
            FOREIGN KEY (world_id) REFERENCES world_metadata(world_id) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS backup_manifest (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            world_id TEXT NOT NULL,
            source_path TEXT NOT NULL,
            backup_path TEXT NOT NULL,
            region_source_path TEXT NOT NULL,
            region_backup_path TEXT NOT NULL,
            storage_backend TEXT NOT NULL,
            schema_version INTEGER NOT NULL,
            save_format_version INTEGER NOT NULL,
            created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
            FOREIGN KEY (world_id) REFERENCES world_metadata(world_id) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS builds (
            build_id TEXT PRIMARY KEY,
            world_id TEXT NOT NULL,
            idempotency_key TEXT NOT NULL,
            plan_hash TEXT NOT NULL,
            canonical_plan_json TEXT NOT NULL,
            actor TEXT NOT NULL,
            state TEXT NOT NULL CHECK (state IN ('applied', 'undone')),
            changed INTEGER NOT NULL,
            skipped INTEGER NOT NULL DEFAULT 0,
            created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
            applied_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
            undone_at TEXT,
            UNIQUE (world_id, idempotency_key),
            FOREIGN KEY (world_id) REFERENCES world_metadata(world_id) ON DELETE CASCADE
        );

        CREATE INDEX IF NOT EXISTS idx_builds_world_state
            ON builds (world_id, state, created_at);

        CREATE TABLE IF NOT EXISTS build_blocks (
            build_id TEXT NOT NULL,
            ordinal INTEGER NOT NULL,
            x INTEGER NOT NULL,
            y INTEGER NOT NULL,
            z INTEGER NOT NULL,
            original_block_state_raw INTEGER NOT NULL,
            applied_block_state_raw INTEGER NOT NULL,
            PRIMARY KEY (build_id, ordinal),
            UNIQUE (build_id, x, y, z),
            FOREIGN KEY (build_id) REFERENCES builds(build_id) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS asset_definitions (
            world_id TEXT NOT NULL,
            asset_id TEXT NOT NULL,
            version INTEGER NOT NULL,
            spec_hash TEXT NOT NULL,
            canonical_spec_json TEXT NOT NULL,
            budget_json TEXT NOT NULL,
            actor TEXT NOT NULL,
            created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
            PRIMARY KEY (world_id, asset_id, version),
            FOREIGN KEY (world_id) REFERENCES world_metadata(world_id) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS asset_instances (
            instance_id TEXT PRIMARY KEY,
            world_id TEXT NOT NULL,
            idempotency_key TEXT NOT NULL,
            asset_id TEXT NOT NULL,
            asset_version INTEGER NOT NULL,
            position_x REAL NOT NULL,
            position_y REAL NOT NULL,
            position_z REAL NOT NULL,
            rotation_x REAL NOT NULL,
            rotation_y REAL NOT NULL,
            rotation_z REAL NOT NULL,
            scale_x REAL NOT NULL,
            scale_y REAL NOT NULL,
            scale_z REAL NOT NULL,
            collision_json TEXT NOT NULL,
            interaction_state_json TEXT NOT NULL,
            owner TEXT NOT NULL,
            created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
            updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
            removed_at TEXT,
            UNIQUE (world_id, idempotency_key),
            FOREIGN KEY (world_id, asset_id, asset_version)
                REFERENCES asset_definitions(world_id, asset_id, version)
        );

        CREATE INDEX IF NOT EXISTS idx_asset_instances_world_active
            ON asset_instances (world_id, removed_at, created_at);

        CREATE TABLE IF NOT EXISTS schema_migrations (
            version INTEGER PRIMARY KEY,
            description TEXT NOT NULL,
            applied_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
        );
        ",
    )?;

    ensure_column(
        conn,
        "world_metadata",
        "storage_backend",
        "TEXT NOT NULL DEFAULT 'sqlite_delta'",
    )?;
    ensure_column(
        conn,
        "world_metadata",
        "save_format_version",
        "INTEGER NOT NULL DEFAULT 1",
    )?;

    let existing_versions = conn
        .query_row(
            "
            SELECT schema_version, save_format_version
            FROM world_metadata
            WHERE world_id = ?1
            ",
            params![WORLD_ID],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()?;

    if let Some((schema_version, save_format_version)) = existing_versions {
        if schema_version > SCHEMA_VERSION {
            return Err(PersistenceError::UnsupportedSchemaVersion {
                found: schema_version,
                supported: SCHEMA_VERSION,
            });
        }
        if save_format_version > SAVE_FORMAT_VERSION {
            return Err(PersistenceError::UnsupportedSaveFormatVersion {
                found: save_format_version,
                supported: SAVE_FORMAT_VERSION,
            });
        }
        if schema_version < SCHEMA_VERSION {
            conn.execute(
                "
                INSERT OR IGNORE INTO schema_migrations (version, description)
                VALUES (?1, ?2)
                ",
                params![
                    SCHEMA_VERSION,
                    "add declarative asset catalog and persistent instances"
                ],
            )?;
        }
    } else {
        conn.execute(
            "
            INSERT OR IGNORE INTO schema_migrations (version, description)
            VALUES (?1, ?2)
            ",
            params![SCHEMA_VERSION, "initialize SQLite delta storage schema"],
        )?;
    }

    conn.execute(
        "
        INSERT INTO world_metadata (
            world_id, storage_backend, schema_version, save_format_version,
            min_x, max_x, min_y, max_y, min_z, max_z
        )
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
        ON CONFLICT(world_id) DO UPDATE SET
            storage_backend = excluded.storage_backend,
            schema_version = excluded.schema_version,
            save_format_version = excluded.save_format_version,
            min_x = excluded.min_x,
            max_x = excluded.max_x,
            min_y = excluded.min_y,
            max_y = excluded.max_y,
            min_z = excluded.min_z,
            max_z = excluded.max_z,
            updated_at = CURRENT_TIMESTAMP
        ",
        params![
            WORLD_ID,
            STORAGE_BACKEND,
            SCHEMA_VERSION,
            SAVE_FORMAT_VERSION,
            WORLD_BOUNDS.min_x,
            WORLD_BOUNDS.max_x,
            WORLD_BOUNDS.min_y,
            WORLD_BOUNDS.max_y,
            WORLD_BOUNDS.min_z,
            WORLD_BOUNDS.max_z,
        ],
    )?;

    Ok(())
}

fn ensure_column(
    conn: &Connection,
    table: &str,
    column: &str,
    definition: &str,
) -> PersistenceResult<()> {
    let mut statement = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = statement.query([])?;

    while let Some(row) = rows.next()? {
        let existing = row.get::<_, String>(1)?;
        if existing == column {
            return Ok(());
        }
    }

    conn.execute_batch(&format!(
        "ALTER TABLE {table} ADD COLUMN {column} {definition};"
    ))?;
    Ok(())
}

fn set_transactional_override(
    tx: &rusqlite::Transaction<'_>,
    position: BlockPos,
    block: BlockState,
) -> PersistenceResult<()> {
    let base = base_block_state_at(WORLD_BOUNDS, position).unwrap_or(BlockState::AIR);
    if block == base {
        tx.execute(
            "DELETE FROM block_overrides WHERE world_id = ?1 AND x = ?2 AND y = ?3 AND z = ?4",
            params![WORLD_ID, position.x, position.y, position.z],
        )?;
    } else {
        tx.execute(
            "
            INSERT INTO block_overrides (
                world_id, x, y, z, block_state_raw, updated_at
            )
            VALUES (?1, ?2, ?3, ?4, ?5, CURRENT_TIMESTAMP)
            ON CONFLICT(world_id, x, y, z) DO UPDATE SET
                block_state_raw = excluded.block_state_raw,
                updated_at = CURRENT_TIMESTAMP
            ",
            params![
                WORLD_ID,
                position.x,
                position.y,
                position.z,
                i64::from(block.to_raw()),
            ],
        )?;
    }
    Ok(())
}

fn mark_transactional_chunk_dirty(
    tx: &rusqlite::Transaction<'_>,
    position: BlockPos,
) -> PersistenceResult<()> {
    let chunk = ChunkColumn::from_block_pos(position);
    tx.execute(
        "
        INSERT INTO dirty_chunks (
            world_id, chunk_x, chunk_z, dirty_count, last_dirty_at, last_flushed_at
        )
        VALUES (?1, ?2, ?3, 1, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)
        ON CONFLICT(world_id, chunk_x, chunk_z) DO UPDATE SET
            dirty_count = dirty_count + 1,
            last_dirty_at = CURRENT_TIMESTAMP,
            last_flushed_at = CURRENT_TIMESTAMP
        ",
        params![WORLD_ID, chunk.x, chunk.z],
    )?;
    Ok(())
}

fn load_build_by_column(
    conn: &Connection,
    column: &str,
    value: &str,
) -> PersistenceResult<Option<StoredBuild>> {
    debug_assert!(matches!(column, "build_id" | "idempotency_key"));
    let sql = format!(
        "
        SELECT build_id, idempotency_key, plan_hash, canonical_plan_json,
               actor, state, changed, skipped, created_at, applied_at, undone_at
        FROM builds
        WHERE world_id = ?1 AND {column} = ?2
        "
    );
    let row = conn
        .query_row(&sql, params![WORLD_ID, value], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, i64>(7)?,
                row.get::<_, String>(8)?,
                row.get::<_, String>(9)?,
                row.get::<_, Option<String>>(10)?,
            ))
        })
        .optional()?;
    let Some((
        build_id,
        idempotency_key,
        plan_hash,
        canonical_plan_json,
        actor,
        state,
        changed,
        skipped,
        created_at,
        applied_at,
        undone_at,
    )) = row
    else {
        return Ok(None);
    };

    let mut statement = conn.prepare(
        "
        SELECT x, y, z, original_block_state_raw, applied_block_state_raw
        FROM build_blocks
        WHERE build_id = ?1
        ORDER BY ordinal
        ",
    )?;
    let mut rows = statement.query(params![build_id])?;
    let mut blocks = Vec::new();
    while let Some(row) = rows.next()? {
        let position = BlockPos::new(row.get(0)?, row.get(1)?, row.get(2)?);
        let original_raw = row.get::<_, i64>(3)?;
        let applied_raw = row.get::<_, i64>(4)?;
        let original_block = decode_block_state(position, original_raw)?;
        let applied_block = decode_block_state(position, applied_raw)?;
        blocks.push(BuildBlockRecord {
            position,
            original_block,
            applied_block,
        });
    }

    Ok(Some(StoredBuild {
        build_id,
        idempotency_key,
        plan_hash,
        canonical_plan_json,
        actor,
        state,
        changed: usize::try_from(changed).unwrap_or(usize::MAX),
        skipped: usize::try_from(skipped).unwrap_or(usize::MAX),
        created_at,
        applied_at,
        undone_at,
        blocks,
    }))
}

fn decode_block_state(position: BlockPos, raw: i64) -> PersistenceResult<BlockState> {
    u16::try_from(raw)
        .ok()
        .and_then(BlockState::from_raw)
        .ok_or(PersistenceError::InvalidBlockState { position, raw })
}

const ASSET_INSTANCE_SELECT: &str = "
    SELECT instance_id, idempotency_key, asset_id, asset_version,
           position_x, position_y, position_z,
           rotation_x, rotation_y, rotation_z,
           scale_x, scale_y, scale_z,
           collision_json, interaction_state_json, owner,
           created_at, updated_at, removed_at
    FROM asset_instances
";

fn asset_definition_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredAssetDefinition> {
    Ok(StoredAssetDefinition {
        asset_id: row.get(0)?,
        version: row.get::<_, i64>(1)?.max(0) as u32,
        spec_hash: row.get(2)?,
        canonical_spec_json: row.get(3)?,
        budget_json: row.get(4)?,
        actor: row.get(5)?,
        created_at: row.get(6)?,
    })
}

fn asset_instance_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredAssetInstance> {
    Ok(StoredAssetInstance {
        instance_id: row.get(0)?,
        idempotency_key: row.get(1)?,
        asset_id: row.get(2)?,
        version: row.get::<_, i64>(3)?.max(0) as u32,
        position: [row.get(4)?, row.get(5)?, row.get(6)?],
        rotation_degrees: [row.get(7)?, row.get(8)?, row.get(9)?],
        scale: [row.get(10)?, row.get(11)?, row.get(12)?],
        collision_json: row.get(13)?,
        interaction_state_json: row.get(14)?,
        owner: row.get(15)?,
        created_at: row.get(16)?,
        updated_at: row.get(17)?,
        removed_at: row.get(18)?,
    })
}

fn load_legacy_block_overrides(path: &Path) -> PersistenceResult<Vec<SavedBlockOverride>> {
    let mut conn = Connection::open(path)?;
    initialize_schema(&mut conn)?;
    load_block_overrides_from_connection(&conn)
}

fn load_block_overrides_from_connection(
    conn: &Connection,
) -> PersistenceResult<Vec<SavedBlockOverride>> {
    let mut statement = conn.prepare(
        "
        SELECT x, y, z, block_state_raw
        FROM block_overrides
        WHERE world_id = ?1
        ORDER BY x, y, z
        ",
    )?;

    let mut rows = statement.query(params![WORLD_ID])?;
    let mut overrides = Vec::new();

    while let Some(row) = rows.next()? {
        let x = row.get::<_, i32>(0)?;
        let y = row.get::<_, i32>(1)?;
        let z = row.get::<_, i32>(2)?;
        let raw = row.get::<_, i64>(3)?;
        let position = BlockPos::new(x, y, z);
        let Some(block) = u16::try_from(raw).ok().and_then(BlockState::from_raw) else {
            return Err(PersistenceError::InvalidBlockState { position, raw });
        };

        overrides.push(SavedBlockOverride { position, block });
    }

    Ok(overrides)
}

fn load_chunk_overrides_from_connection(
    conn: &Connection,
    column: ChunkColumn,
) -> PersistenceResult<Vec<SavedBlockOverride>> {
    let min_x = column.x * 16;
    let max_x = min_x + 15;
    let min_z = column.z * 16;
    let max_z = min_z + 15;
    let mut statement = conn.prepare(
        "
        SELECT x, y, z, block_state_raw
        FROM block_overrides
        WHERE world_id = ?1 AND x BETWEEN ?2 AND ?3 AND z BETWEEN ?4 AND ?5
        ORDER BY x, y, z
        ",
    )?;

    let mut rows = statement.query(params![WORLD_ID, min_x, max_x, min_z, max_z])?;
    let mut overrides = Vec::new();

    while let Some(row) = rows.next()? {
        let x = row.get::<_, i32>(0)?;
        let y = row.get::<_, i32>(1)?;
        let z = row.get::<_, i32>(2)?;
        let raw = row.get::<_, i64>(3)?;
        let position = BlockPos::new(x, y, z);
        let Some(block) = u16::try_from(raw).ok().and_then(BlockState::from_raw) else {
            return Err(PersistenceError::InvalidBlockState { position, raw });
        };

        overrides.push(SavedBlockOverride { position, block });
    }

    Ok(overrides)
}

fn count_legacy_block_overrides(path: &Path) -> PersistenceResult<usize> {
    if !path.exists() {
        return Ok(0);
    }

    let conn = Connection::open(path)?;
    let count = conn
        .query_row(
            "SELECT COUNT(*) FROM block_overrides WHERE world_id = ?1",
            params![WORLD_ID],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .unwrap_or(0);
    Ok(count.max(0) as usize)
}

fn dirty_chunk_count(path: &Path) -> PersistenceResult<usize> {
    if !path.exists() {
        return Ok(0);
    }

    let conn = Connection::open(path)?;
    let count = conn
        .query_row(
            "SELECT COUNT(*) FROM dirty_chunks WHERE world_id = ?1",
            params![WORLD_ID],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .unwrap_or(0);
    Ok(count.max(0) as usize)
}

fn flush_sqlite_delta_edits(conn: &mut Connection, edits: &[BlockEdit]) -> PersistenceResult<()> {
    if edits.is_empty() {
        return Ok(());
    }

    let tx = conn.transaction()?;

    for edit in edits {
        tx.execute(
            "
            INSERT INTO command_log (
                world_id, command_kind, x, y, z, block_state_raw
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            ",
            params![
                WORLD_ID,
                edit.command_kind.as_str(),
                edit.position.x,
                edit.position.y,
                edit.position.z,
                i64::from(edit.block.to_raw()),
            ],
        )?;

        if edit.needs_override() {
            tx.execute(
                "
                INSERT INTO block_overrides (
                    world_id, x, y, z, block_state_raw, updated_at
                )
                VALUES (?1, ?2, ?3, ?4, ?5, CURRENT_TIMESTAMP)
                ON CONFLICT(world_id, x, y, z) DO UPDATE SET
                    block_state_raw = excluded.block_state_raw,
                    updated_at = CURRENT_TIMESTAMP
                ",
                params![
                    WORLD_ID,
                    edit.position.x,
                    edit.position.y,
                    edit.position.z,
                    i64::from(edit.block.to_raw()),
                ],
            )?;
        } else {
            tx.execute(
                "
                DELETE FROM block_overrides
                WHERE world_id = ?1 AND x = ?2 AND y = ?3 AND z = ?4
                ",
                params![WORLD_ID, edit.position.x, edit.position.y, edit.position.z],
            )?;
        }
    }

    tx.commit()?;
    Ok(())
}

fn flush_command_log_and_dirty_index(
    conn: &mut Connection,
    edits: &[BlockEdit],
) -> PersistenceResult<()> {
    if edits.is_empty() {
        return Ok(());
    }

    let tx = conn.transaction()?;

    for edit in edits {
        // Anvil is authoritative for ordinary edits. Clear any transactional
        // SQLite overlay at the same coordinate after the Anvil write succeeds
        // so a later restart cannot resurrect a superseded build block.
        tx.execute(
            "DELETE FROM block_overrides WHERE world_id = ?1 AND x = ?2 AND y = ?3 AND z = ?4",
            params![WORLD_ID, edit.position.x, edit.position.y, edit.position.z],
        )?;
        tx.execute(
            "
            INSERT INTO command_log (
                world_id, command_kind, x, y, z, block_state_raw
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            ",
            params![
                WORLD_ID,
                edit.command_kind.as_str(),
                edit.position.x,
                edit.position.y,
                edit.position.z,
                i64::from(edit.block.to_raw()),
            ],
        )?;

        let chunk = ChunkColumn::from_block_pos(edit.position);
        tx.execute(
            "
            INSERT INTO dirty_chunks (
                world_id, chunk_x, chunk_z, dirty_count, last_dirty_at, last_flushed_at
            )
            VALUES (?1, ?2, ?3, 1, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)
            ON CONFLICT(world_id, chunk_x, chunk_z) DO UPDATE SET
                dirty_count = dirty_count + 1,
                last_dirty_at = CURRENT_TIMESTAMP,
                last_flushed_at = CURRENT_TIMESTAMP
            ",
            params![WORLD_ID, chunk.x, chunk.z],
        )?;
    }

    tx.commit()?;
    Ok(())
}

fn record_backup_manifest(
    conn: &mut Connection,
    manifest: &BackupManifest,
) -> PersistenceResult<()> {
    conn.execute(
        "
        INSERT INTO backup_manifest (
            world_id, source_path, backup_path, region_source_path, region_backup_path,
            storage_backend, schema_version, save_format_version
        )
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
        ",
        params![
            WORLD_ID,
            manifest.source_path.display().to_string(),
            manifest.backup_path.display().to_string(),
            manifest.region_source_path.display().to_string(),
            manifest.region_backup_path.display().to_string(),
            manifest.storage_backend,
            manifest.schema_version,
            manifest.save_format_version,
        ],
    )?;
    Ok(())
}

fn ensure_parent_dir(path: &Path) -> PersistenceResult<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }

    Ok(())
}

fn copy_dir_recursive(source: &Path, target: &Path) -> PersistenceResult<()> {
    fs::create_dir_all(target)?;
    if !source.exists() {
        return Ok(());
    }

    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let source_path = entry.path();
        let target_path = target.join(entry.file_name());
        if source_path.is_dir() {
            copy_dir_recursive(&source_path, &target_path)?;
        } else {
            fs::copy(&source_path, &target_path)?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world_command::GROUND_Y;

    fn temp_db_path(name: &str) -> PathBuf {
        let mut path = env::temp_dir();
        path.push(format!(
            "world-loom-{name}-{}-{}.sqlite3",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock should be after UNIX_EPOCH")
                .as_nanos()
        ));
        path
    }

    fn temp_region_dir(name: &str) -> PathBuf {
        let mut path = env::temp_dir();
        path.push(format!(
            "world-loom-{name}-{}-{}.anvil-region",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock should be after UNIX_EPOCH")
                .as_nanos()
        ));
        path
    }

    fn anvil_region_path(region_dir: &Path, column: ChunkColumn) -> PathBuf {
        let (region_x, region_z) = column.region_coords(REGION_SIZE_CHUNKS);
        region_dir.join(format!("r.{region_x}.{region_z}.mca"))
    }

    fn read_anvil_chunk_bytes(region_dir: &Path, column: ChunkColumn) -> Vec<u8> {
        let path = anvil_region_path(region_dir, column);
        let file = File::open(path).expect("open Anvil region test file");
        let mut region = AnvilRegion::from_stream(file).expect("open Anvil region stream");
        let (local_x, local_z) = local_chunk_coords(column);
        region
            .read_chunk(local_x, local_z)
            .expect("read Anvil chunk")
            .expect("Anvil chunk should exist")
    }

    fn pos(x: i32, y: i32, z: i32) -> BlockPos {
        BlockPos::new(x, y, z)
    }

    fn command_log_count(path: &Path) -> i64 {
        let conn = Connection::open(path).expect("open sqlite test database");
        conn.query_row(
            "SELECT COUNT(*) FROM command_log WHERE world_id = ?1",
            params![WORLD_ID],
            |row| row.get(0),
        )
        .expect("count command_log rows")
    }

    fn metadata_versions(path: &Path) -> (String, i64, i64) {
        let conn = Connection::open(path).expect("open sqlite test database");
        conn.query_row(
            "
            SELECT storage_backend, schema_version, save_format_version
            FROM world_metadata
            WHERE world_id = ?1
            ",
            params![WORLD_ID],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("read world_metadata versions")
    }

    fn migration_count(path: &Path, version: i64) -> i64 {
        let conn = Connection::open(path).expect("open sqlite test database");
        conn.query_row(
            "SELECT COUNT(*) FROM schema_migrations WHERE version = ?1",
            params![version],
            |row| row.get(0),
        )
        .expect("count schema_migrations rows")
    }

    #[test]
    fn save_load_round_trip_deletes_base_equivalent_air_override() {
        let path = temp_db_path("round-trip");
        let region_dir = temp_region_dir("round-trip");
        let storage =
            HybridAnvilStorage::open(&path, &region_dir).expect("open hybrid anvil storage");
        let position = pos(4, GROUND_Y + 1, 4);
        let column = ChunkColumn::from_block_pos(position);
        let region_path = anvil_region_path(&region_dir, column);

        storage
            .persist_block_edits(&[BlockEdit::from_world_command(
                WorldCommand::SetBlock {
                    position,
                    block: BlockState::STONE,
                },
                BlockState::STONE,
            )])
            .expect("persist stone override");

        assert_eq!(
            storage
                .load_chunk(column)
                .expect("load stone chunk")
                .expect("stored chunk")
                .blocks,
            vec![SavedBlockOverride {
                position,
                block: BlockState::STONE
            }]
        );
        assert!(
            region_path.exists(),
            "dirty chunk should be written to .mca"
        );
        fastanvil::JavaChunk::from_bytes(&read_anvil_chunk_bytes(&region_dir, column))
            .expect("written chunk should parse as Java Anvil chunk");

        storage
            .persist_block_edits(&[BlockEdit::from_world_command(
                WorldCommand::RemoveBlock { position },
                BlockState::AIR,
            )])
            .expect("persist air revert");

        assert_eq!(
            storage.load_chunk(column).expect("load reverted chunk"),
            None
        );
        assert!(
            !region_path.exists(),
            "fully reverted dirty chunk should remove empty .mca file"
        );
        assert_eq!(command_log_count(&path), 2);

        let _ = fs::remove_file(path);
        let _ = fs::remove_dir_all(region_dir);
    }

    #[test]
    fn removed_base_world_block_persists_as_air_override() {
        let path = temp_db_path("air-override");
        let region_dir = temp_region_dir("air-override");
        let storage =
            HybridAnvilStorage::open(&path, &region_dir).expect("open hybrid anvil storage");
        let position = pos(4, GROUND_Y, 4);
        let column = ChunkColumn::from_block_pos(position);

        storage
            .persist_block_edits(&[BlockEdit::from_world_command(
                WorldCommand::RemoveBlock { position },
                BlockState::AIR,
            )])
            .expect("persist removed base block");

        assert_eq!(
            storage
                .load_chunk(column)
                .expect("load air chunk")
                .expect("stored chunk")
                .blocks,
            vec![SavedBlockOverride {
                position,
                block: BlockState::AIR
            }]
        );
        assert!(
            anvil_region_path(&region_dir, column).exists(),
            "base block removal should persist to an Anvil .mca file"
        );
        assert_eq!(command_log_count(&path), 1);

        let _ = fs::remove_file(path);
        let _ = fs::remove_dir_all(region_dir);
    }

    #[test]
    fn runtime_flushes_queued_edits_on_drop() {
        let path = temp_db_path("runtime");
        let region_dir = temp_region_dir("runtime");
        let position = pos(5, GROUND_Y + 1, 5);

        {
            let runtime = PersistenceRuntime::open_with_region_dir(&path, &region_dir)
                .expect("open persistence runtime");
            runtime.queue_world_command(
                WorldCommand::SetBlock {
                    position,
                    block: BlockState::GLASS,
                },
                BlockState::GLASS,
            );
        }

        let storage =
            HybridAnvilStorage::open(&path, &region_dir).expect("open hybrid anvil storage");
        assert_eq!(
            storage
                .load_chunk(ChunkColumn::from_block_pos(position))
                .expect("load queued runtime chunk")
                .expect("stored chunk")
                .blocks,
            vec![SavedBlockOverride {
                position,
                block: BlockState::GLASS
            }]
        );

        let _ = fs::remove_file(path);
        let _ = fs::remove_dir_all(region_dir);
    }

    #[test]
    fn sqlite_delta_storage_implements_world_storage() {
        let path = temp_db_path("world-storage-trait");
        let storage = SqliteDeltaStorage::open(&path).expect("open sqlite delta storage");
        let storage_trait: &dyn WorldStorage = &storage;

        assert_eq!(storage_trait.backend_name(), SQLITE_DELTA_BACKEND);
        assert_eq!(storage_trait.schema_version(), SCHEMA_VERSION);
        assert_eq!(storage_trait.save_format_version(), SAVE_FORMAT_VERSION);
        assert_eq!(
            storage_trait
                .load_chunk(ChunkColumn::new(0, 0))
                .expect("load empty chunk"),
            None
        );

        let _ = fs::remove_file(path);
    }

    #[test]
    fn migrates_v1_metadata_to_save_format_version() {
        let path = temp_db_path("v1-migration");
        {
            let conn = Connection::open(&path).expect("open v1 sqlite test database");
            conn.execute_batch(
                "
                CREATE TABLE world_metadata (
                    world_id TEXT PRIMARY KEY,
                    schema_version INTEGER NOT NULL,
                    min_x INTEGER NOT NULL,
                    max_x INTEGER NOT NULL,
                    min_y INTEGER NOT NULL,
                    max_y INTEGER NOT NULL,
                    min_z INTEGER NOT NULL,
                    max_z INTEGER NOT NULL,
                    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
                );
                INSERT INTO world_metadata (
                    world_id, schema_version, min_x, max_x, min_y, max_y, min_z, max_z
                )
                VALUES ('default', 1, -64, 63, 60, 95, -64, 63);
                ",
            )
            .expect("create v1 metadata");
        }

        let region_dir = temp_region_dir("v1-migration");
        let _storage = HybridAnvilStorage::open(&path, &region_dir).expect("migrate v1 database");

        assert_eq!(
            metadata_versions(&path),
            (
                STORAGE_BACKEND.to_string(),
                SCHEMA_VERSION,
                SAVE_FORMAT_VERSION
            )
        );
        assert_eq!(migration_count(&path, SCHEMA_VERSION), 1);

        let _ = fs::remove_file(path);
        let _ = fs::remove_dir_all(region_dir);
    }

    #[test]
    fn backup_database_copies_current_save() {
        let path = temp_db_path("backup-source");
        let backup_path = temp_db_path("backup-copy");
        let region_dir = temp_region_dir("backup-source");
        let region_backup_path = temp_region_dir("backup-copy");
        let storage =
            HybridAnvilStorage::open(&path, &region_dir).expect("open hybrid anvil storage");
        let position = pos(7, GROUND_Y + 1, 7);

        storage
            .persist_block_edits(&[BlockEdit::from_world_command(
                WorldCommand::SetBlock {
                    position,
                    block: BlockState::COBBLESTONE,
                },
                BlockState::COBBLESTONE,
            )])
            .expect("persist source edit");

        let manifest = storage
            .create_backup(&backup_path, &region_backup_path)
            .expect("backup hybrid storage");

        assert_eq!(manifest.source_path, path);
        assert_eq!(manifest.backup_path, backup_path);
        assert_eq!(manifest.region_source_path, region_dir);
        assert_eq!(manifest.region_backup_path, region_backup_path);
        assert_eq!(manifest.storage_backend, STORAGE_BACKEND);
        assert!(
            anvil_region_path(
                &manifest.region_backup_path,
                ChunkColumn::from_block_pos(position)
            )
            .exists(),
            "backup should copy Anvil .mca files"
        );
        let backup_storage =
            HybridAnvilStorage::open(&backup_path, &region_backup_path).expect("open backup");
        assert_eq!(
            backup_storage
                .load_chunk(ChunkColumn::from_block_pos(position))
                .expect("load backup chunk")
                .expect("stored chunk")
                .blocks,
            vec![SavedBlockOverride {
                position,
                block: BlockState::COBBLESTONE
            }]
        );

        let _ = fs::remove_file(path);
        let _ = fs::remove_file(backup_path);
        let _ = fs::remove_dir_all(region_dir);
        let _ = fs::remove_dir_all(region_backup_path);
    }

    #[test]
    fn build_and_exact_undo_survive_runtime_restart() {
        let path = temp_db_path("build-restart");
        let region_dir = temp_region_dir("build-restart");
        let position = pos(18, GROUND_Y + 1, 18);
        let column = ChunkColumn::from_block_pos(position);
        let record = NewBuildRecord {
            build_id: "build-test-id".to_string(),
            idempotency_key: "build-test-key".to_string(),
            plan_hash: "abc123".to_string(),
            canonical_plan_json: "{\"schema_version\":1}".to_string(),
            actor: "mcp".to_string(),
            blocks: vec![BuildBlockRecord {
                position,
                original_block: BlockState::AIR,
                applied_block: BlockState::GLASS,
            }],
        };

        {
            let runtime = PersistenceRuntime::open_with_region_dir(&path, &region_dir)
                .expect("open build persistence runtime");
            let stored = runtime.persist_build(&record).expect("persist build");
            assert_eq!(stored.state, "applied");
            assert_eq!(
                runtime
                    .load_build_by_idempotency_key("build-test-key")
                    .expect("load idempotency record")
                    .expect("build should exist")
                    .build_id,
                "build-test-id"
            );
        }

        {
            let runtime = PersistenceRuntime::open_with_region_dir(&path, &region_dir)
                .expect("reopen after build");
            let chunk = runtime
                .load_chunk(column)
                .expect("load persisted build chunk")
                .expect("build overlay should create stored chunk");
            assert_eq!(
                chunk.blocks,
                vec![SavedBlockOverride {
                    position,
                    block: BlockState::GLASS,
                }]
            );
            let build = runtime
                .load_build("build-test-id")
                .expect("load build")
                .expect("build should exist");
            let undone = runtime.mark_build_undone(&build).expect("persist undo");
            assert_eq!(undone.state, "undone");
        }

        {
            let runtime = PersistenceRuntime::open_with_region_dir(&path, &region_dir)
                .expect("reopen after undo");
            assert_eq!(
                runtime.load_chunk(column).expect("load undone chunk"),
                None,
                "undo should remove an air-equivalent transactional override"
            );
            let build = runtime
                .load_build("build-test-id")
                .expect("load undone build")
                .expect("undone audit record should remain");
            assert_eq!(build.state, "undone");
            assert!(build.undone_at.is_some());
        }

        let _ = fs::remove_file(path);
        let _ = fs::remove_dir_all(region_dir);
    }
}
