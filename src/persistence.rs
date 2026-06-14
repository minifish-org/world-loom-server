use std::env;
use std::error::Error;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Mutex;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use rusqlite::{params, Connection};
use valence::prelude::{BlockPos, BlockState, Resource};

use crate::world_command::{base_block_state_at, WorldCommand, WORLD_BOUNDS};

pub const DATABASE_PATH_ENV: &str = "WORLD_LOOM_DB_PATH";
pub const DEFAULT_DATABASE_PATH: &str = "data/world-loom.sqlite3";
pub const WORLD_ID: &str = "default";
pub const SCHEMA_VERSION: i64 = 1;

const WRITE_FLUSH_INTERVAL: Duration = Duration::from_millis(250);
const WRITE_BATCH_LIMIT: usize = 256;

pub type PersistenceResult<T> = Result<T, PersistenceError>;

#[derive(Debug)]
pub enum PersistenceError {
    Io(std::io::Error),
    Sqlite(rusqlite::Error),
    InvalidBlockState { position: BlockPos, raw: i64 },
    WriterPanic,
}

impl fmt::Display for PersistenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "I/O error: {err}"),
            Self::Sqlite(err) => write!(f, "SQLite error: {err}"),
            Self::InvalidBlockState { position, raw } => {
                write!(f, "invalid block state raw id {raw} at {position:?}")
            }
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

pub struct PersistenceRuntime {
    db_path: PathBuf,
    sender: Sender<PersistenceMessage>,
    loaded_overrides: Vec<SavedBlockOverride>,
    writer: Mutex<Option<JoinHandle<()>>>,
}

impl Resource for PersistenceRuntime {}

impl fmt::Debug for PersistenceRuntime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PersistenceRuntime")
            .field("db_path", &self.db_path)
            .field("loaded_overrides", &self.loaded_overrides.len())
            .finish_non_exhaustive()
    }
}

impl PersistenceRuntime {
    pub fn open_default() -> PersistenceResult<Self> {
        Self::open(default_database_path())
    }

    pub fn open(path: impl Into<PathBuf>) -> PersistenceResult<Self> {
        let db_path = path.into();
        ensure_parent_dir(&db_path)?;

        {
            let mut conn = Connection::open(&db_path)?;
            initialize_schema(&mut conn)?;
        }

        let loaded_overrides = load_block_overrides(&db_path)?;
        let (sender, receiver) = mpsc::channel();
        let writer_path = db_path.clone();
        let writer = thread::Builder::new()
            .name("world-loom-sqlite-writer".to_string())
            .spawn(move || writer_loop(writer_path, receiver))
            .map_err(PersistenceError::Io)?;

        Ok(Self {
            db_path,
            sender,
            loaded_overrides,
            writer: Mutex::new(Some(writer)),
        })
    }

    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    pub fn loaded_overrides(&self) -> &[SavedBlockOverride] {
        &self.loaded_overrides
    }

    pub fn queue_world_command(&self, command: WorldCommand, final_block: BlockState) {
        let edit = BlockEdit::from_world_command(command, final_block);
        if let Err(err) = self.sender.send(PersistenceMessage::Edit(edit)) {
            eprintln!("[world-loom] failed to queue SQLite block edit: {err}");
        }
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

pub fn load_block_overrides(path: &Path) -> PersistenceResult<Vec<SavedBlockOverride>> {
    if !path.exists() {
        return Ok(Vec::new());
    }

    let mut conn = Connection::open(path)?;
    initialize_schema(&mut conn)?;
    load_block_overrides_from_connection(&conn)
}

pub fn persist_block_edits(path: &Path, edits: &[BlockEdit]) -> PersistenceResult<()> {
    ensure_parent_dir(path)?;
    let mut conn = Connection::open(path)?;
    initialize_schema(&mut conn)?;
    flush_edits(&mut conn, edits)
}

fn writer_loop(path: PathBuf, receiver: Receiver<PersistenceMessage>) {
    if let Err(err) = run_writer_loop(&path, receiver) {
        eprintln!("[world-loom] SQLite writer stopped: {err}");
    }
}

fn run_writer_loop(path: &Path, receiver: Receiver<PersistenceMessage>) -> PersistenceResult<()> {
    ensure_parent_dir(path)?;
    let mut conn = Connection::open(path)?;
    initialize_schema(&mut conn)?;

    let mut batch = Vec::new();
    loop {
        match receiver.recv_timeout(WRITE_FLUSH_INTERVAL) {
            Ok(PersistenceMessage::Edit(edit)) => {
                batch.push(edit);
                let should_shutdown = drain_pending_messages(&receiver, &mut batch);
                flush_edits(&mut conn, &batch)?;
                batch.clear();
                if should_shutdown {
                    return Ok(());
                }
            }
            Ok(PersistenceMessage::Shutdown) => {
                drain_pending_messages(&receiver, &mut batch);
                flush_edits(&mut conn, &batch)?;
                return Ok(());
            }
            Err(RecvTimeoutError::Timeout) => {
                if !batch.is_empty() {
                    flush_edits(&mut conn, &batch)?;
                    batch.clear();
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                flush_edits(&mut conn, &batch)?;
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

fn initialize_schema(conn: &mut Connection) -> PersistenceResult<()> {
    conn.execute_batch(
        "
        PRAGMA journal_mode = WAL;
        PRAGMA synchronous = NORMAL;
        PRAGMA foreign_keys = ON;

        CREATE TABLE IF NOT EXISTS world_metadata (
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
        ",
    )?;

    conn.execute(
        "
        INSERT INTO world_metadata (
            world_id, schema_version, min_x, max_x, min_y, max_y, min_z, max_z
        )
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
        ON CONFLICT(world_id) DO UPDATE SET
            schema_version = excluded.schema_version,
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
            SCHEMA_VERSION,
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

fn flush_edits(conn: &mut Connection, edits: &[BlockEdit]) -> PersistenceResult<()> {
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

fn ensure_parent_dir(path: &Path) -> PersistenceResult<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
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

    #[test]
    fn save_load_round_trip_deletes_base_equivalent_air_override() {
        let path = temp_db_path("round-trip");
        let position = pos(4, GROUND_Y + 1, 4);

        persist_block_edits(
            &path,
            &[BlockEdit::from_world_command(
                WorldCommand::SetBlock {
                    position,
                    block: BlockState::STONE,
                },
                BlockState::STONE,
            )],
        )
        .expect("persist stone override");

        assert_eq!(
            load_block_overrides(&path).expect("load stone override"),
            vec![SavedBlockOverride {
                position,
                block: BlockState::STONE,
            }]
        );

        persist_block_edits(
            &path,
            &[BlockEdit::from_world_command(
                WorldCommand::RemoveBlock { position },
                BlockState::AIR,
            )],
        )
        .expect("persist air revert");

        assert_eq!(
            load_block_overrides(&path).expect("load empty overrides"),
            Vec::new()
        );
        assert_eq!(command_log_count(&path), 2);

        let _ = fs::remove_file(path);
    }

    #[test]
    fn removed_base_world_block_persists_as_air_override() {
        let path = temp_db_path("air-override");
        let position = pos(4, GROUND_Y, 4);

        persist_block_edits(
            &path,
            &[BlockEdit::from_world_command(
                WorldCommand::RemoveBlock { position },
                BlockState::AIR,
            )],
        )
        .expect("persist removed base block");

        assert_eq!(
            load_block_overrides(&path).expect("load air override"),
            vec![SavedBlockOverride {
                position,
                block: BlockState::AIR,
            }]
        );
        assert_eq!(command_log_count(&path), 1);

        let _ = fs::remove_file(path);
    }

    #[test]
    fn runtime_flushes_queued_edits_on_drop() {
        let path = temp_db_path("runtime");
        let position = pos(5, GROUND_Y + 1, 5);

        {
            let runtime = PersistenceRuntime::open(&path).expect("open persistence runtime");
            runtime.queue_world_command(
                WorldCommand::SetBlock {
                    position,
                    block: BlockState::GLASS,
                },
                BlockState::GLASS,
            );
        }

        assert_eq!(
            load_block_overrides(&path).expect("load queued runtime edit"),
            vec![SavedBlockOverride {
                position,
                block: BlockState::GLASS,
            }]
        );

        let _ = fs::remove_file(path);
    }
}
