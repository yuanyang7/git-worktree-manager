use crate::git::{GitRepository, GitWorktree};
use std::ffi::{CStr, CString};
use std::fmt;
use std::fs;
use std::io;
use std::os::raw::{c_char, c_int, c_uchar, c_void};
use std::path::{Path, PathBuf};

const SQLITE_OK: c_int = 0;
const SQLITE_ROW: c_int = 100;
const SQLITE_DONE: c_int = 101;
const SQLITE_INTEGER: c_int = 1;
const SQLITE_TEXT: c_int = 3;
const SQLITE_NULL: c_int = 5;
const SQLITE_OPEN_READWRITE: c_int = 0x0000_0002;
const SQLITE_OPEN_CREATE: c_int = 0x0000_0004;

#[repr(C)]
struct Sqlite3 {
    _private: [u8; 0],
}

#[repr(C)]
struct Sqlite3Stmt {
    _private: [u8; 0],
}

#[link(name = "sqlite3")]
unsafe extern "C" {
    fn sqlite3_open_v2(
        filename: *const c_char,
        database: *mut *mut Sqlite3,
        flags: c_int,
        vfs: *const c_char,
    ) -> c_int;
    fn sqlite3_close(database: *mut Sqlite3) -> c_int;
    fn sqlite3_errmsg(database: *mut Sqlite3) -> *const c_char;
    fn sqlite3_exec(
        database: *mut Sqlite3,
        sql: *const c_char,
        callback: Option<
            unsafe extern "C" fn(*mut c_void, c_int, *mut *mut c_char, *mut *mut c_char) -> c_int,
        >,
        callback_argument: *mut c_void,
        error_message: *mut *mut c_char,
    ) -> c_int;
    fn sqlite3_free(pointer: *mut c_void);
    fn sqlite3_prepare_v2(
        database: *mut Sqlite3,
        sql: *const c_char,
        byte_count: c_int,
        statement: *mut *mut Sqlite3Stmt,
        tail: *mut *const c_char,
    ) -> c_int;
    fn sqlite3_finalize(statement: *mut Sqlite3Stmt) -> c_int;
    fn sqlite3_bind_text(
        statement: *mut Sqlite3Stmt,
        index: c_int,
        value: *const c_char,
        byte_count: c_int,
        destructor: Option<unsafe extern "C" fn(*mut c_void)>,
    ) -> c_int;
    fn sqlite3_bind_null(statement: *mut Sqlite3Stmt, index: c_int) -> c_int;
    fn sqlite3_bind_int64(statement: *mut Sqlite3Stmt, index: c_int, value: i64) -> c_int;
    fn sqlite3_step(statement: *mut Sqlite3Stmt) -> c_int;
    fn sqlite3_column_type(statement: *mut Sqlite3Stmt, column: c_int) -> c_int;
    fn sqlite3_column_int64(statement: *mut Sqlite3Stmt, column: c_int) -> i64;
    fn sqlite3_column_text(statement: *mut Sqlite3Stmt, column: c_int) -> *const c_uchar;
}

const SCHEMA_SQL: &str = r#"
PRAGMA foreign_keys = ON;
PRAGMA busy_timeout = 5000;

CREATE TABLE IF NOT EXISTS schema_migrations (
    version INTEGER PRIMARY KEY,
    applied_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS repositories (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    common_git_dir TEXT NOT NULL UNIQUE,
    root_path TEXT NOT NULL,
    display_name TEXT NOT NULL,
    integration_branch TEXT,
    remote_integration_branch TEXT,
    added_at INTEGER NOT NULL,
    last_seen_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS worktrees (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    repository_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    path TEXT NOT NULL,
    branch TEXT,
    head TEXT,
    git_state TEXT NOT NULL,
    lifecycle_state TEXT NOT NULL,
    first_seen_at INTEGER NOT NULL,
    created_at INTEGER,
    last_seen_at INTEGER NOT NULL,
    archived_at INTEGER,
    removed_at INTEGER,
    UNIQUE(repository_id, path)
);

CREATE UNIQUE INDEX IF NOT EXISTS worktrees_active_branch
    ON worktrees(repository_id, branch)
    WHERE branch IS NOT NULL
      AND lifecycle_state NOT IN ('missing', 'removed');

CREATE TABLE IF NOT EXISTS sessions (
    id TEXT PRIMARY KEY,
    worktree_id INTEGER NOT NULL REFERENCES worktrees(id) ON DELETE CASCADE,
    provider TEXT NOT NULL,
    provider_session_id TEXT,
    pid INTEGER,
    process_started_at INTEGER,
    terminal_metadata TEXT,
    state TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    last_seen_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS leases (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    worktree_id INTEGER NOT NULL REFERENCES worktrees(id) ON DELETE CASCADE,
    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    acquired_at INTEGER NOT NULL,
    renewed_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    state TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS observations (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    worktree_id INTEGER NOT NULL REFERENCES worktrees(id) ON DELETE CASCADE,
    observed_at INTEGER NOT NULL,
    dirty_files INTEGER,
    staged_files INTEGER,
    unstaged_files INTEGER,
    untracked_files INTEGER,
    conflicted_files INTEGER,
    ahead INTEGER,
    behind INTEGER,
    merge_classification TEXT,
    worktree_bytes INTEGER,
    git_common_bytes INTEGER,
    error TEXT
);

CREATE TABLE IF NOT EXISTS events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    repository_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    worktree_id INTEGER REFERENCES worktrees(id) ON DELETE SET NULL,
    occurred_at INTEGER NOT NULL,
    actor TEXT NOT NULL,
    action TEXT NOT NULL,
    result TEXT NOT NULL,
    details_json TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS reservations (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    repository_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    path TEXT NOT NULL,
    branch TEXT NOT NULL,
    idempotency_key TEXT,
    state TEXT NOT NULL,
    worktree_id INTEGER REFERENCES worktrees(id) ON DELETE SET NULL,
    created_at INTEGER NOT NULL,
    completed_at INTEGER
);

CREATE UNIQUE INDEX IF NOT EXISTS reservations_active_path
    ON reservations(repository_id, path)
    WHERE state IN ('pending', 'succeeded');

CREATE UNIQUE INDEX IF NOT EXISTS reservations_active_branch
    ON reservations(repository_id, branch)
    WHERE state IN ('pending', 'succeeded');

CREATE UNIQUE INDEX IF NOT EXISTS reservations_idempotency_key
    ON reservations(repository_id, idempotency_key)
    WHERE idempotency_key IS NOT NULL;

INSERT OR IGNORE INTO schema_migrations(version, applied_at) VALUES (1, strftime('%s', 'now'));
"#;

#[derive(Debug)]
pub enum InventoryError {
    Io {
        operation: String,
        source: io::Error,
    },
    Sqlite {
        operation: String,
        code: i32,
        message: String,
    },
    InvalidData(String),
    Conflict(String),
}

impl fmt::Display for InventoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { operation, source } => write!(formatter, "{operation}: {source}"),
            Self::Sqlite {
                operation,
                code,
                message,
            } => write!(formatter, "{operation} (SQLite code {code}): {message}"),
            Self::InvalidData(message) => write!(formatter, "invalid inventory data: {message}"),
            Self::Conflict(message) => write!(formatter, "inventory conflict: {message}"),
        }
    }
}

impl std::error::Error for InventoryError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryRecord {
    pub id: i64,
    pub common_git_dir: String,
    pub root_path: String,
    pub display_name: String,
    pub integration_branch: Option<String>,
    pub remote_integration_branch: Option<String>,
    pub added_at: i64,
    pub last_seen_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeRecord {
    pub id: i64,
    pub repository_id: i64,
    pub path: String,
    pub branch: Option<String>,
    pub head: Option<String>,
    pub git_state: String,
    pub lifecycle_state: String,
    pub first_seen_at: i64,
    pub created_at: Option<i64>,
    pub last_seen_at: i64,
    pub archived_at: Option<i64>,
    pub removed_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventRecord {
    pub id: i64,
    pub repository_id: i64,
    pub worktree_id: Option<i64>,
    pub occurred_at: i64,
    pub actor: String,
    pub action: String,
    pub result: String,
    pub details_json: String,
}

#[derive(Debug, Clone, Copy)]
pub struct EventInput<'a> {
    pub repository_id: i64,
    pub worktree_id: Option<i64>,
    pub occurred_at: i64,
    pub actor: &'a str,
    pub action: &'a str,
    pub result: &'a str,
    pub details_json: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReservationRecord {
    pub id: i64,
    pub repository_id: i64,
    pub path: String,
    pub branch: String,
    pub idempotency_key: Option<String>,
    pub state: String,
    pub worktree_id: Option<i64>,
    pub created_at: i64,
    pub completed_at: Option<i64>,
}

pub struct Inventory {
    path: PathBuf,
    connection: Connection,
}

impl Inventory {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, InventoryError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent).map_err(|source| InventoryError::Io {
                operation: format!("create inventory directory {}", parent.display()),
                source,
            })?;
        }
        let connection = Connection::open(&path)?;
        connection.execute_batch(SCHEMA_SQL)?;
        Ok(Self { path, connection })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn register_repository(
        &self,
        repository: &GitRepository,
        now: i64,
    ) -> Result<RepositoryRecord, InventoryError> {
        let mut statement = self.connection.prepare(
            "INSERT INTO repositories( \
                common_git_dir, root_path, display_name, integration_branch, \
                remote_integration_branch, added_at, last_seen_at \
             ) VALUES (?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(common_git_dir) DO UPDATE SET \
                root_path = excluded.root_path, \
                display_name = excluded.display_name, \
                last_seen_at = excluded.last_seen_at",
        )?;
        statement.bind_text(1, &repository.common_git_dir.to_string_lossy())?;
        statement.bind_text(2, &repository.root.to_string_lossy())?;
        statement.bind_text(3, &repository.display_name)?;
        statement.bind_optional_text(4, repository.integration_branch.as_deref())?;
        statement.bind_optional_text(5, repository.remote_integration_branch.as_deref())?;
        statement.bind_i64(6, now)?;
        statement.bind_i64(7, now)?;
        statement.expect_done()?;
        drop(statement);

        self.repository_by_common_git_dir(&repository.common_git_dir)?
            .ok_or_else(|| {
                InventoryError::InvalidData(
                    "repository upsert did not return a repository row".to_owned(),
                )
            })
    }

    pub fn reconcile_repository(
        &self,
        repository: &GitRepository,
        worktrees: &[GitWorktree],
        now: i64,
    ) -> Result<Vec<WorktreeRecord>, InventoryError> {
        let repository_record = self.register_repository(repository, now)?;
        self.connection.execute_batch("BEGIN IMMEDIATE")?;
        let result = self.reconcile_worktrees(repository_record.id, worktrees, now);
        match result {
            Ok(()) => match self.connection.execute_batch("COMMIT") {
                Ok(()) => self.worktrees(repository_record.id),
                Err(error) => {
                    let _ = self.connection.execute_batch("ROLLBACK");
                    Err(error)
                }
            },
            Err(error) => {
                let _ = self.connection.execute_batch("ROLLBACK");
                Err(error)
            }
        }
    }

    fn reconcile_worktrees(
        &self,
        repository_id: i64,
        worktrees: &[GitWorktree],
        now: i64,
    ) -> Result<(), InventoryError> {
        for worktree in worktrees {
            let path = worktree.path.to_string_lossy();
            let lifecycle_state = if worktree.prunable || !worktree.path.is_dir() {
                "missing"
            } else {
                "active"
            };
            let mut statement = self.connection.prepare(
                "INSERT INTO worktrees( \
                    repository_id, path, branch, head, git_state, lifecycle_state, \
                    first_seen_at, last_seen_at \
                 ) VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
                 ON CONFLICT(repository_id, path) DO UPDATE SET \
                    branch = excluded.branch, \
                    head = excluded.head, \
                    git_state = excluded.git_state, \
                    lifecycle_state = CASE \
                        WHEN worktrees.lifecycle_state = 'archived' THEN 'archived' \
                        ELSE excluded.lifecycle_state \
                    END, \
                    last_seen_at = excluded.last_seen_at, \
                    removed_at = CASE \
                        WHEN excluded.lifecycle_state = 'active' THEN NULL \
                        ELSE worktrees.removed_at \
                    END",
            )?;
            statement.bind_i64(1, repository_id)?;
            statement.bind_text(2, &path)?;
            statement.bind_optional_text(3, worktree.branch.as_deref())?;
            statement.bind_optional_text(4, worktree.head.as_deref())?;
            statement.bind_text(5, worktree.state())?;
            statement.bind_text(6, lifecycle_state)?;
            statement.bind_i64(7, now)?;
            statement.bind_i64(8, now)?;
            statement.expect_done()?;
        }

        let existing = self.worktree_identity_rows(repository_id)?;
        for (id, path, lifecycle_state) in existing {
            let listed_worktree = worktrees
                .iter()
                .find(|worktree| worktree.path.to_string_lossy() == path);
            if listed_worktree.is_some_and(|worktree| worktree.prunable || !worktree.path.is_dir())
            {
                self.release_reservations_for_worktree(id, now)?;
            } else if listed_worktree.is_none()
                && lifecycle_state != "removed"
                && lifecycle_state != "archived"
            {
                let mut statement = self.connection.prepare(
                    "UPDATE worktrees SET lifecycle_state = CASE \
                        WHEN EXISTS ( \
                            SELECT 1 FROM events AS started \
                            WHERE started.worktree_id = worktrees.id \
                              AND started.action = 'remove_worktree' \
                              AND started.result IN ('started', 'succeeded') \
                              AND NOT EXISTS ( \
                                  SELECT 1 FROM events AS failed \
                                  WHERE failed.worktree_id = worktrees.id \
                                    AND failed.action = 'remove_worktree' \
                                    AND failed.result = 'failed' \
                                    AND failed.id > started.id \
                              ) \
                        ) THEN 'removed' \
                        ELSE 'missing' \
                    END, last_seen_at = ? \
                     WHERE id = ?",
                )?;
                statement.bind_i64(1, now)?;
                statement.bind_i64(2, id)?;
                statement.expect_done()?;
                self.release_reservations_for_worktree(id, now)?;
            }
        }
        Ok(())
    }

    pub fn repository_by_common_git_dir(
        &self,
        common_git_dir: &Path,
    ) -> Result<Option<RepositoryRecord>, InventoryError> {
        let mut statement = self.connection.prepare(
            "SELECT id, common_git_dir, root_path, display_name, integration_branch, \
                    remote_integration_branch, added_at, last_seen_at \
             FROM repositories WHERE common_git_dir = ?",
        )?;
        statement.bind_text(1, &common_git_dir.to_string_lossy())?;
        if !statement.step()? {
            return Ok(None);
        }
        Ok(Some(repository_from_statement(&statement)?))
    }

    pub fn worktrees(&self, repository_id: i64) -> Result<Vec<WorktreeRecord>, InventoryError> {
        let mut statement = self.connection.prepare(
            "SELECT id, repository_id, path, branch, head, git_state, lifecycle_state, \
                    first_seen_at, created_at, last_seen_at, archived_at, removed_at \
             FROM worktrees WHERE repository_id = ? ORDER BY path",
        )?;
        statement.bind_i64(1, repository_id)?;
        let mut records = Vec::new();
        while statement.step()? {
            records.push(worktree_from_statement(&statement)?);
        }
        Ok(records)
    }

    pub fn worktree_by_path(
        &self,
        repository_id: i64,
        path: &Path,
    ) -> Result<Option<WorktreeRecord>, InventoryError> {
        let mut statement = self.connection.prepare(
            "SELECT id, repository_id, path, branch, head, git_state, lifecycle_state, \
                    first_seen_at, created_at, last_seen_at, archived_at, removed_at \
             FROM worktrees WHERE repository_id = ? AND path = ?",
        )?;
        statement.bind_i64(1, repository_id)?;
        statement.bind_text(2, &path.to_string_lossy())?;
        if !statement.step()? {
            return Ok(None);
        }
        Ok(Some(worktree_from_statement(&statement)?))
    }

    pub fn mark_created(&self, worktree_id: i64, created_at: i64) -> Result<(), InventoryError> {
        let mut statement = self.connection.prepare(
            "UPDATE worktrees SET created_at = ?, \
                    lifecycle_state = 'active', removed_at = NULL WHERE id = ?",
        )?;
        statement.bind_i64(1, created_at)?;
        statement.bind_i64(2, worktree_id)?;
        statement.expect_done()
    }

    pub fn release_reservations_for_worktree(
        &self,
        worktree_id: i64,
        released_at: i64,
    ) -> Result<(), InventoryError> {
        let mut statement = self.connection.prepare(
            "UPDATE reservations SET state = 'released', completed_at = ? \
             WHERE worktree_id = ? AND state = 'succeeded'",
        )?;
        statement.bind_i64(1, released_at)?;
        statement.bind_i64(2, worktree_id)?;
        statement.expect_done()
    }

    pub fn mark_removed(
        &self,
        repository_id: i64,
        path: &Path,
        removed_at: i64,
    ) -> Result<(), InventoryError> {
        let mut statement = self.connection.prepare(
            "UPDATE worktrees SET lifecycle_state = 'removed', removed_at = ?, \
                    last_seen_at = ? WHERE repository_id = ? AND path = ?",
        )?;
        statement.bind_i64(1, removed_at)?;
        statement.bind_i64(2, removed_at)?;
        statement.bind_i64(3, repository_id)?;
        statement.bind_text(4, &path.to_string_lossy())?;
        statement.expect_done()
    }

    pub fn active_lease_exists(&self, worktree_id: i64, now: i64) -> Result<bool, InventoryError> {
        let mut statement = self.connection.prepare(
            "SELECT 1 FROM leases WHERE worktree_id = ? AND state = 'active' \
                    AND expires_at > ? LIMIT 1",
        )?;
        statement.bind_i64(1, worktree_id)?;
        statement.bind_i64(2, now)?;
        statement.step()
    }

    pub fn reserve_creation(
        &self,
        repository_id: i64,
        path: &Path,
        branch: &str,
        idempotency_key: Option<&str>,
        now: i64,
    ) -> Result<ReservationRecord, InventoryError> {
        self.connection.execute_batch("BEGIN IMMEDIATE")?;
        let result = self.reserve_creation_inner(repository_id, path, branch, idempotency_key, now);
        match result {
            Ok(reservation) => match self.connection.execute_batch("COMMIT") {
                Ok(()) => Ok(reservation),
                Err(error) => {
                    let _ = self.connection.execute_batch("ROLLBACK");
                    Err(error)
                }
            },
            Err(error) => {
                let _ = self.connection.execute_batch("ROLLBACK");
                Err(error)
            }
        }
    }

    fn reserve_creation_inner(
        &self,
        repository_id: i64,
        path: &Path,
        branch: &str,
        idempotency_key: Option<&str>,
        now: i64,
    ) -> Result<ReservationRecord, InventoryError> {
        if let Some(key) = idempotency_key
            && let Some(existing) = self.reservation_by_key(repository_id, key)?
        {
            if existing.path != path.to_string_lossy() || existing.branch != branch {
                return Err(InventoryError::Conflict(format!(
                    "idempotency key {key:?} is already reserved for {} / {}",
                    existing.path, existing.branch
                )));
            }
            if matches!(existing.state.as_str(), "pending" | "succeeded") {
                return Ok(existing);
            }
            return Err(InventoryError::Conflict(format!(
                "idempotency key {key:?} was already completed with state {:?}",
                existing.state
            )));
        }

        if let Some(existing) = self.reservation_by_path_or_branch(repository_id, path, branch)? {
            return Err(InventoryError::Conflict(format!(
                "{} is already reserved by creation {}",
                if existing.path == path.to_string_lossy() {
                    "the requested path"
                } else {
                    "the requested branch"
                },
                existing.id
            )));
        }

        let mut statement = self.connection.prepare(
            "INSERT INTO reservations( \
                repository_id, path, branch, idempotency_key, state, created_at \
             ) VALUES (?, ?, ?, ?, 'pending', ?)",
        )?;
        statement.bind_i64(1, repository_id)?;
        statement.bind_text(2, &path.to_string_lossy())?;
        statement.bind_text(3, branch)?;
        statement.bind_optional_text(4, idempotency_key)?;
        statement.bind_i64(5, now)?;
        statement.expect_done()?;
        drop(statement);

        let id = self.connection.last_insert_rowid();
        self.reservation_by_id(id)?.ok_or_else(|| {
            InventoryError::InvalidData("reservation insert did not return a row".to_owned())
        })
    }

    pub fn set_reservation_state(
        &self,
        reservation_id: i64,
        state: &str,
        worktree_id: Option<i64>,
        completed_at: Option<i64>,
    ) -> Result<(), InventoryError> {
        let mut statement = self.connection.prepare(
            "UPDATE reservations SET state = ?, worktree_id = ?, completed_at = ? \
             WHERE id = ?",
        )?;
        statement.bind_text(1, state)?;
        statement.bind_i64_option(2, worktree_id)?;
        statement.bind_i64_option(3, completed_at)?;
        statement.bind_i64(4, reservation_id)?;
        statement.expect_done()
    }

    pub fn record_event(&self, input: EventInput<'_>) -> Result<EventRecord, InventoryError> {
        let mut statement = self.connection.prepare(
            "INSERT INTO events( \
                repository_id, worktree_id, occurred_at, actor, action, result, details_json \
             ) VALUES (?, ?, ?, ?, ?, ?, ?)",
        )?;
        statement.bind_i64(1, input.repository_id)?;
        statement.bind_i64_option(2, input.worktree_id)?;
        statement.bind_i64(3, input.occurred_at)?;
        statement.bind_text(4, input.actor)?;
        statement.bind_text(5, input.action)?;
        statement.bind_text(6, input.result)?;
        statement.bind_text(7, input.details_json)?;
        statement.expect_done()?;
        drop(statement);
        let id = self.connection.last_insert_rowid();
        self.event_by_id(id)?.ok_or_else(|| {
            InventoryError::InvalidData("event insert did not return a row".to_owned())
        })
    }

    pub fn events(&self, repository_id: i64) -> Result<Vec<EventRecord>, InventoryError> {
        let mut statement = self.connection.prepare(
            "SELECT id, repository_id, worktree_id, occurred_at, actor, action, result, details_json \
             FROM events WHERE repository_id = ? ORDER BY id",
        )?;
        statement.bind_i64(1, repository_id)?;
        let mut records = Vec::new();
        while statement.step()? {
            records.push(event_from_statement(&statement)?);
        }
        Ok(records)
    }

    fn reservation_by_key(
        &self,
        repository_id: i64,
        key: &str,
    ) -> Result<Option<ReservationRecord>, InventoryError> {
        let mut statement = self.connection.prepare(
            "SELECT id, repository_id, path, branch, idempotency_key, state, worktree_id, \
                    created_at, completed_at \
             FROM reservations WHERE repository_id = ? AND idempotency_key = ?",
        )?;
        statement.bind_i64(1, repository_id)?;
        statement.bind_text(2, key)?;
        if !statement.step()? {
            return Ok(None);
        }
        Ok(Some(reservation_from_statement(&statement)?))
    }

    fn reservation_by_path_or_branch(
        &self,
        repository_id: i64,
        path: &Path,
        branch: &str,
    ) -> Result<Option<ReservationRecord>, InventoryError> {
        let mut statement = self.connection.prepare(
            "SELECT id, repository_id, path, branch, idempotency_key, state, worktree_id, \
                    created_at, completed_at \
             FROM reservations \
             WHERE repository_id = ? AND state IN ('pending', 'succeeded') \
               AND (path = ? OR branch = ?) LIMIT 1",
        )?;
        statement.bind_i64(1, repository_id)?;
        statement.bind_text(2, &path.to_string_lossy())?;
        statement.bind_text(3, branch)?;
        if !statement.step()? {
            return Ok(None);
        }
        Ok(Some(reservation_from_statement(&statement)?))
    }

    fn reservation_by_id(&self, id: i64) -> Result<Option<ReservationRecord>, InventoryError> {
        let mut statement = self.connection.prepare(
            "SELECT id, repository_id, path, branch, idempotency_key, state, worktree_id, \
                    created_at, completed_at \
             FROM reservations WHERE id = ?",
        )?;
        statement.bind_i64(1, id)?;
        if !statement.step()? {
            return Ok(None);
        }
        Ok(Some(reservation_from_statement(&statement)?))
    }

    fn event_by_id(&self, id: i64) -> Result<Option<EventRecord>, InventoryError> {
        let mut statement = self.connection.prepare(
            "SELECT id, repository_id, worktree_id, occurred_at, actor, action, result, details_json \
             FROM events WHERE id = ?",
        )?;
        statement.bind_i64(1, id)?;
        if !statement.step()? {
            return Ok(None);
        }
        Ok(Some(event_from_statement(&statement)?))
    }

    fn worktree_identity_rows(
        &self,
        repository_id: i64,
    ) -> Result<Vec<(i64, String, String)>, InventoryError> {
        let mut statement = self
            .connection
            .prepare("SELECT id, path, lifecycle_state FROM worktrees WHERE repository_id = ?")?;
        statement.bind_i64(1, repository_id)?;
        let mut rows = Vec::new();
        while statement.step()? {
            rows.push((
                statement.column_i64(0),
                statement.column_text_required(1)?,
                statement.column_text_required(2)?,
            ));
        }
        Ok(rows)
    }
}

impl fmt::Debug for Inventory {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Inventory")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

struct Connection {
    raw: *mut Sqlite3,
}

impl Connection {
    fn open(path: &Path) -> Result<Self, InventoryError> {
        let filename = CString::new(path_bytes(path)).map_err(|_| {
            InventoryError::InvalidData(format!(
                "inventory path contains a NUL byte: {}",
                path.display()
            ))
        })?;
        let mut raw = std::ptr::null_mut();
        let code = unsafe {
            sqlite3_open_v2(
                filename.as_ptr(),
                &mut raw,
                SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE,
                std::ptr::null(),
            )
        };
        if code != SQLITE_OK {
            let message = sqlite_message(raw);
            if !raw.is_null() {
                unsafe {
                    sqlite3_close(raw);
                }
            }
            return Err(InventoryError::Sqlite {
                operation: format!("open inventory {}", path.display()),
                code,
                message,
            });
        }
        Ok(Self { raw })
    }

    fn execute_batch(&self, sql: &str) -> Result<(), InventoryError> {
        let sql = CString::new(sql).map_err(|_| {
            InventoryError::InvalidData("SQLite SQL contains a NUL byte".to_owned())
        })?;
        let mut error_message = std::ptr::null_mut();
        let code = unsafe {
            sqlite3_exec(
                self.raw,
                sql.as_ptr(),
                None,
                std::ptr::null_mut(),
                &mut error_message,
            )
        };
        if code == SQLITE_OK {
            return Ok(());
        }
        let message = if error_message.is_null() {
            sqlite_message(self.raw)
        } else {
            let message = unsafe { CStr::from_ptr(error_message) }
                .to_string_lossy()
                .into_owned();
            unsafe {
                sqlite3_free(error_message.cast());
            }
            message
        };
        Err(InventoryError::Sqlite {
            operation: "execute SQLite statements".to_owned(),
            code,
            message,
        })
    }

    fn prepare<'connection>(
        &'connection self,
        sql: &str,
    ) -> Result<Statement<'connection>, InventoryError> {
        let sql = CString::new(sql).map_err(|_| {
            InventoryError::InvalidData("SQLite SQL contains a NUL byte".to_owned())
        })?;
        let mut raw = std::ptr::null_mut();
        let code = unsafe {
            sqlite3_prepare_v2(self.raw, sql.as_ptr(), -1, &mut raw, std::ptr::null_mut())
        };
        if code != SQLITE_OK {
            return Err(InventoryError::Sqlite {
                operation: "prepare SQLite statement".to_owned(),
                code,
                message: sqlite_message(self.raw),
            });
        }
        Ok(Statement {
            connection: self,
            raw,
            bound_texts: Vec::new(),
        })
    }

    fn last_insert_rowid(&self) -> i64 {
        unsafe { sqlite3_last_insert_rowid(self.raw) }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        if !self.raw.is_null() {
            unsafe {
                sqlite3_close(self.raw);
            }
        }
    }
}

struct Statement<'connection> {
    connection: &'connection Connection,
    raw: *mut Sqlite3Stmt,
    bound_texts: Vec<CString>,
}

impl Statement<'_> {
    fn bind_text(&mut self, index: c_int, value: &str) -> Result<(), InventoryError> {
        self.bind_optional_text(index, Some(value))
    }

    fn bind_optional_text(
        &mut self,
        index: c_int,
        value: Option<&str>,
    ) -> Result<(), InventoryError> {
        let code = match value {
            Some(value) => {
                let owned = CString::new(value).map_err(|_| {
                    InventoryError::InvalidData("SQLite text value contains a NUL byte".to_owned())
                })?;
                let pointer = owned.as_ptr();
                let byte_count = c_int::try_from(owned.as_bytes().len()).map_err(|_| {
                    InventoryError::InvalidData("SQLite text value is too long".to_owned())
                })?;
                self.bound_texts.push(owned);
                unsafe { sqlite3_bind_text(self.raw, index, pointer, byte_count, None) }
            }
            None => unsafe { sqlite3_bind_null(self.raw, index) },
        };
        self.check(code, "bind SQLite text")
    }

    fn bind_i64(&mut self, index: c_int, value: i64) -> Result<(), InventoryError> {
        let code = unsafe { sqlite3_bind_int64(self.raw, index, value) };
        self.check(code, "bind SQLite integer")
    }

    fn bind_i64_option(&mut self, index: c_int, value: Option<i64>) -> Result<(), InventoryError> {
        match value {
            Some(value) => self.bind_i64(index, value),
            None => self.check(
                unsafe { sqlite3_bind_null(self.raw, index) },
                "bind SQLite null",
            ),
        }
    }

    fn step(&mut self) -> Result<bool, InventoryError> {
        let code = unsafe { sqlite3_step(self.raw) };
        match code {
            SQLITE_ROW => Ok(true),
            SQLITE_DONE => Ok(false),
            code => Err(InventoryError::Sqlite {
                operation: "step SQLite statement".to_owned(),
                code,
                message: sqlite_message(self.connection.raw),
            }),
        }
    }

    fn expect_done(&mut self) -> Result<(), InventoryError> {
        if self.step()? {
            return Err(InventoryError::InvalidData(
                "write SQLite statement unexpectedly returned a row".to_owned(),
            ));
        }
        Ok(())
    }

    fn column_i64(&self, column: c_int) -> i64 {
        unsafe { sqlite3_column_int64(self.raw, column) }
    }

    fn column_text_required(&self, column: c_int) -> Result<String, InventoryError> {
        self.column_text(column)?.ok_or_else(|| {
            InventoryError::InvalidData(format!("SQLite column {column} unexpectedly was NULL"))
        })
    }

    fn column_text(&self, column: c_int) -> Result<Option<String>, InventoryError> {
        let column_type = unsafe { sqlite3_column_type(self.raw, column) };
        match column_type {
            SQLITE_NULL => Ok(None),
            SQLITE_TEXT => {
                let pointer = unsafe { sqlite3_column_text(self.raw, column) };
                if pointer.is_null() {
                    return Err(InventoryError::InvalidData(format!(
                        "SQLite text column {column} returned a null pointer"
                    )));
                }
                Ok(Some(
                    unsafe { CStr::from_ptr(pointer.cast()) }
                        .to_string_lossy()
                        .into_owned(),
                ))
            }
            other => Err(InventoryError::InvalidData(format!(
                "SQLite column {column} has unexpected type {other}"
            ))),
        }
    }

    fn check(&self, code: c_int, operation: &str) -> Result<(), InventoryError> {
        if code == SQLITE_OK {
            Ok(())
        } else {
            Err(InventoryError::Sqlite {
                operation: operation.to_owned(),
                code,
                message: sqlite_message(self.connection.raw),
            })
        }
    }
}

impl Drop for Statement<'_> {
    fn drop(&mut self) {
        if !self.raw.is_null() {
            unsafe {
                sqlite3_finalize(self.raw);
            }
        }
    }
}

unsafe extern "C" {
    fn sqlite3_last_insert_rowid(database: *mut Sqlite3) -> i64;
}

fn path_bytes(path: &Path) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    {
        path.to_string_lossy().as_bytes().to_vec()
    }
}

fn sqlite_message(database: *mut Sqlite3) -> String {
    if database.is_null() {
        return "unknown SQLite error".to_owned();
    }
    let pointer = unsafe { sqlite3_errmsg(database) };
    if pointer.is_null() {
        "unknown SQLite error".to_owned()
    } else {
        unsafe { CStr::from_ptr(pointer) }
            .to_string_lossy()
            .into_owned()
    }
}

fn repository_from_statement(
    statement: &Statement<'_>,
) -> Result<RepositoryRecord, InventoryError> {
    Ok(RepositoryRecord {
        id: statement.column_i64(0),
        common_git_dir: statement.column_text_required(1)?,
        root_path: statement.column_text_required(2)?,
        display_name: statement.column_text_required(3)?,
        integration_branch: statement.column_text(4)?,
        remote_integration_branch: statement.column_text(5)?,
        added_at: statement.column_i64(6),
        last_seen_at: statement.column_i64(7),
    })
}

fn worktree_from_statement(statement: &Statement<'_>) -> Result<WorktreeRecord, InventoryError> {
    Ok(WorktreeRecord {
        id: statement.column_i64(0),
        repository_id: statement.column_i64(1),
        path: statement.column_text_required(2)?,
        branch: statement.column_text(3)?,
        head: statement.column_text(4)?,
        git_state: statement.column_text_required(5)?,
        lifecycle_state: statement.column_text_required(6)?,
        first_seen_at: statement.column_i64(7),
        created_at: statement.column_text_i64(8)?,
        last_seen_at: statement.column_i64(9),
        archived_at: statement.column_text_i64(10)?,
        removed_at: statement.column_text_i64(11)?,
    })
}

fn reservation_from_statement(
    statement: &Statement<'_>,
) -> Result<ReservationRecord, InventoryError> {
    Ok(ReservationRecord {
        id: statement.column_i64(0),
        repository_id: statement.column_i64(1),
        path: statement.column_text_required(2)?,
        branch: statement.column_text_required(3)?,
        idempotency_key: statement.column_text(4)?,
        state: statement.column_text_required(5)?,
        worktree_id: statement.column_text_i64(6)?,
        created_at: statement.column_i64(7),
        completed_at: statement.column_text_i64(8)?,
    })
}

fn event_from_statement(statement: &Statement<'_>) -> Result<EventRecord, InventoryError> {
    Ok(EventRecord {
        id: statement.column_i64(0),
        repository_id: statement.column_i64(1),
        worktree_id: statement.column_text_i64(2)?,
        occurred_at: statement.column_i64(3),
        actor: statement.column_text_required(4)?,
        action: statement.column_text_required(5)?,
        result: statement.column_text_required(6)?,
        details_json: statement.column_text_required(7)?,
    })
}

impl Statement<'_> {
    fn column_text_i64(&self, column: c_int) -> Result<Option<i64>, InventoryError> {
        let column_type = unsafe { sqlite3_column_type(self.raw, column) };
        match column_type {
            SQLITE_NULL => Ok(None),
            SQLITE_INTEGER => Ok(Some(self.column_i64(column))),
            other => Err(InventoryError::InvalidData(format!(
                "SQLite column {column} has unexpected integer type {other}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Inventory;
    use crate::git::GitRepository;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn creates_schema_and_records_repository_worktree_and_event() {
        let directory = temporary_directory();
        let database = directory.join("inventory.sqlite3");
        let repository_path = directory.join("repository");
        fs::create_dir(&repository_path).expect("repository directory should be created");
        run_git(&repository_path, &["init", "-q", "-b", "main"]);
        run_git(&repository_path, &["config", "user.name", "Inventory Test"]);
        run_git(
            &repository_path,
            &["config", "user.email", "inventory@example.test"],
        );
        fs::write(repository_path.join("README.md"), "inventory\n")
            .expect("fixture file should be written");
        run_git(&repository_path, &["add", "README.md"]);
        run_git(&repository_path, &["commit", "-q", "-m", "initial"]);

        let repository = GitRepository::discover(&repository_path).expect("repository discovered");
        let worktrees = repository.list_worktrees().expect("worktrees listed");
        let inventory = Inventory::open(&database).expect("inventory opened");
        let repository_record = inventory
            .register_repository(&repository, 100)
            .expect("repository registered");
        let records = inventory
            .reconcile_repository(&repository, &worktrees, 101)
            .expect("repository reconciled");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].lifecycle_state, "active");

        let event = inventory
            .record_event(super::EventInput {
                repository_id: repository_record.id,
                worktree_id: Some(records[0].id),
                occurred_at: 102,
                actor: "test",
                action: "inspect",
                result: "succeeded",
                details_json: r#"{"path":"repository"}"#,
            })
            .expect("event recorded");
        assert_eq!(inventory.events(repository_record.id).unwrap(), vec![event]);

        let _ = fs::remove_dir_all(directory);
    }

    fn temporary_directory() -> PathBuf {
        let base = std::env::temp_dir();
        for attempt in 0..100 {
            let timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock should be after Unix epoch")
                .as_nanos();
            let path = base.join(format!(
                "worktree-manager-inventory-test-{}-{timestamp}-{attempt}",
                std::process::id()
            ));
            if fs::create_dir(&path).is_ok() {
                return path;
            }
        }
        panic!("could not allocate a temporary directory");
    }

    fn run_git(cwd: &std::path::Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("git should be installed");
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
