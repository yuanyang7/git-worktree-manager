use crate::git::{GitRepository, GitWorktree};
use std::ffi::{CStr, CString};
use std::fmt;
use std::fs::{self, OpenOptions};
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

const MIGRATION_2_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS requests (
    id TEXT PRIMARY KEY,
    operation TEXT NOT NULL,
    state TEXT NOT NULL,
    response_json TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

UPDATE leases SET state = 'expired', renewed_at = strftime('%s', 'now')
 WHERE state = 'active'
   AND id NOT IN (
       SELECT MAX(id) FROM leases WHERE state = 'active' GROUP BY worktree_id
   );

CREATE UNIQUE INDEX IF NOT EXISTS leases_active_worktree
    ON leases(worktree_id)
    WHERE state = 'active';

INSERT OR IGNORE INTO schema_migrations(version, applied_at)
VALUES (2, strftime('%s', 'now'));
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

#[derive(Debug, Clone, Copy)]
pub struct EventMetadata<'a> {
    pub repository_id: i64,
    pub worktree_id: Option<i64>,
    pub occurred_at: i64,
    pub actor: &'a str,
    pub action: &'a str,
    pub result: &'a str,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRecord {
    pub id: String,
    pub worktree_id: i64,
    pub provider: String,
    pub provider_session_id: Option<String>,
    pub pid: Option<i64>,
    pub process_started_at: Option<i64>,
    pub terminal_metadata: Option<String>,
    pub state: String,
    pub created_at: i64,
    pub last_seen_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseRecord {
    pub id: i64,
    pub worktree_id: i64,
    pub session_id: String,
    pub acquired_at: i64,
    pub renewed_at: i64,
    pub expires_at: i64,
    pub state: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestRecord {
    pub id: String,
    pub operation: String,
    pub state: String,
    pub response_json: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Copy)]
pub struct SessionInput<'a> {
    pub id: &'a str,
    pub worktree_id: i64,
    pub provider: &'a str,
    pub provider_session_id: Option<&'a str>,
    pub pid: Option<i64>,
    pub process_started_at: Option<i64>,
    pub terminal_metadata: Option<&'a str>,
    pub now: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionLeaseResult {
    pub session: SessionRecord,
    pub lease: Option<LeaseRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum InventoryFileIdentity {
    #[cfg(target_os = "macos")]
    Mac {
        device: u64,
        inode: u64,
        birth_time: i64,
        birth_time_nanoseconds: i64,
    },
    #[cfg(all(unix, not(target_os = "macos")))]
    Unix {
        device: u64,
        inode: u64,
        change_time: i64,
        change_time_nanoseconds: i64,
    },
    #[cfg(not(unix))]
    Path(PathBuf),
}

impl InventoryFileIdentity {
    fn key(&self) -> String {
        match self {
            #[cfg(target_os = "macos")]
            Self::Mac {
                device,
                inode,
                birth_time,
                birth_time_nanoseconds,
            } => format!("macos:{device:x}:{inode:x}:{birth_time}:{birth_time_nanoseconds}"),
            #[cfg(all(unix, not(target_os = "macos")))]
            Self::Unix { device, inode, .. } => format!("unix:{device:x}:{inode:x}"),
            #[cfg(not(unix))]
            Self::Path(path) => path.to_string_lossy().into_owned(),
        }
    }
}

pub struct Inventory {
    path: PathBuf,
    file_identity: InventoryFileIdentity,
    _file: std::fs::File,
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
        match fs::symlink_metadata(&path) {
            Ok(_) => restrict_inventory_permissions(&path)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(InventoryError::Io {
                    operation: format!("inspect inventory file {}", path.display()),
                    source,
                });
            }
        }
        let file = prepare_inventory_file(&path)?;
        restrict_inventory_permissions(&path)?;
        let connection = Connection::open(&path)?;
        #[cfg(unix)]
        let opened_file_identity =
            inventory_file_identity_from_metadata(&file.metadata().map_err(|source| {
                InventoryError::Io {
                    operation: format!("inspect inventory file {}", path.display()),
                    source,
                }
            })?);
        #[cfg(not(unix))]
        let opened_file_identity = inventory_file_identity(&path)?;
        if inventory_file_identity(&path)? != opened_file_identity {
            return Err(InventoryError::InvalidData(
                "inventory database changed while it was opening".to_owned(),
            ));
        }
        connection.execute_batch(SCHEMA_SQL)?;
        apply_migrations(&connection)?;
        #[cfg(unix)]
        let file_identity =
            inventory_file_identity_from_metadata(&file.metadata().map_err(|source| {
                InventoryError::Io {
                    operation: format!("inspect inventory file {}", path.display()),
                    source,
                }
            })?);
        #[cfg(not(unix))]
        let file_identity = inventory_file_identity(&path)?;
        if inventory_file_identity(&path)? != file_identity {
            return Err(InventoryError::InvalidData(
                "inventory database changed while it was opening".to_owned(),
            ));
        }
        Ok(Self {
            path,
            file_identity,
            _file: file,
            connection,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn identity_key(&self) -> String {
        self.file_identity.key()
    }

    pub(crate) fn ensure_path_identity(&self) -> Result<(), InventoryError> {
        #[cfg(unix)]
        let opened_file_identity =
            inventory_file_identity_from_metadata(&self._file.metadata().map_err(|source| {
                InventoryError::Io {
                    operation: format!("inspect inventory file {}", self.path.display()),
                    source,
                }
            })?);
        #[cfg(not(unix))]
        let opened_file_identity = self.file_identity.clone();
        let current = inventory_file_identity(&self.path)?;
        if current != opened_file_identity {
            return Err(InventoryError::InvalidData(
                "inventory database was replaced while it was open".to_owned(),
            ));
        }
        Ok(())
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
                self.mark_sessions_stale_for_worktree(id, now)?;
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
                self.mark_sessions_stale_for_worktree(id, now)?;
            }
        }
        Ok(())
    }

    fn mark_sessions_stale_for_worktree(
        &self,
        worktree_id: i64,
        now: i64,
    ) -> Result<(), InventoryError> {
        let mut leases = self.connection.prepare(
            "UPDATE leases SET state = 'expired', renewed_at = ? \
             WHERE worktree_id = ? AND state = 'active'",
        )?;
        leases.bind_i64(1, now)?;
        leases.bind_i64(2, worktree_id)?;
        leases.expect_done()?;
        let mut sessions = self.connection.prepare(
            "UPDATE sessions SET state = 'stale', last_seen_at = ? \
             WHERE worktree_id = ? AND state = 'active'",
        )?;
        sessions.bind_i64(1, now)?;
        sessions.bind_i64(2, worktree_id)?;
        sessions.expect_done()
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

    pub fn begin_request(
        &self,
        id: &str,
        operation: &str,
        now: i64,
    ) -> Result<RequestRecord, InventoryError> {
        self.ensure_path_identity()?;
        if id.is_empty() || operation.is_empty() {
            return Err(InventoryError::InvalidData(
                "request id and operation cannot be empty".to_owned(),
            ));
        }
        self.connection.execute_batch("BEGIN IMMEDIATE")?;
        let result = self.begin_request_inner(id, operation, now);
        match result {
            Ok(record) => match self.connection.execute_batch("COMMIT") {
                Ok(()) => Ok(record),
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

    fn begin_request_inner(
        &self,
        id: &str,
        operation: &str,
        now: i64,
    ) -> Result<RequestRecord, InventoryError> {
        if let Some(existing) = self.request_by_id(id)? {
            if existing.operation != operation {
                return Err(InventoryError::Conflict(format!(
                    "request id {id:?} was already used for operation {:?}",
                    existing.operation
                )));
            }
            if existing.state == "succeeded" {
                return Ok(existing);
            }
            let mut statement = self.connection.prepare(
                "UPDATE requests SET state = 'started', response_json = NULL, updated_at = ? \
                 WHERE id = ?",
            )?;
            statement.bind_i64(1, now)?;
            statement.bind_text(2, id)?;
            statement.expect_done()?;
            return self.request_by_id(id)?.ok_or_else(|| {
                InventoryError::InvalidData("request restart did not return a row".to_owned())
            });
        }

        let mut statement = self.connection.prepare(
            "INSERT INTO requests(id, operation, state, created_at, updated_at) \
             VALUES (?, ?, 'started', ?, ?)",
        )?;
        statement.bind_text(1, id)?;
        statement.bind_text(2, operation)?;
        statement.bind_i64(3, now)?;
        statement.bind_i64(4, now)?;
        statement.expect_done()?;
        self.request_by_id(id)?.ok_or_else(|| {
            InventoryError::InvalidData("request insert did not return a row".to_owned())
        })
    }

    pub fn complete_request(
        &self,
        id: &str,
        response_json: &str,
        now: i64,
    ) -> Result<RequestRecord, InventoryError> {
        self.ensure_path_identity()?;
        let mut statement = self.connection.prepare(
            "UPDATE requests SET state = 'succeeded', response_json = ?, updated_at = ? \
             WHERE id = ?",
        )?;
        statement.bind_text(1, response_json)?;
        statement.bind_i64(2, now)?;
        statement.bind_text(3, id)?;
        statement.expect_done()?;
        self.request_by_id(id)?.ok_or_else(|| {
            InventoryError::InvalidData("completed request did not return a row".to_owned())
        })
    }

    pub fn fail_request(&self, id: &str, now: i64) -> Result<(), InventoryError> {
        self.ensure_path_identity()?;
        let mut statement = self.connection.prepare(
            "UPDATE requests SET state = 'failed', response_json = NULL, updated_at = ? \
             WHERE id = ?",
        )?;
        statement.bind_i64(1, now)?;
        statement.bind_text(2, id)?;
        statement.expect_done()
    }

    pub fn request_by_id(&self, id: &str) -> Result<Option<RequestRecord>, InventoryError> {
        let mut statement = self.connection.prepare(
            "SELECT id, operation, state, response_json, created_at, updated_at \
             FROM requests WHERE id = ?",
        )?;
        statement.bind_text(1, id)?;
        if !statement.step()? {
            return Ok(None);
        }
        Ok(Some(request_from_statement(&statement)?))
    }

    pub fn register_session(
        &self,
        input: SessionInput<'_>,
    ) -> Result<SessionRecord, InventoryError> {
        if input.id.is_empty() || input.provider.is_empty() {
            return Err(InventoryError::InvalidData(
                "session id and provider cannot be empty".to_owned(),
            ));
        }
        self.connection.execute_batch("BEGIN IMMEDIATE")?;
        let result = self.register_session_inner(input);
        match result {
            Ok(session) => match self.connection.execute_batch("COMMIT") {
                Ok(()) => Ok(session),
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

    pub fn register_session_with_lease_and_event(
        &self,
        input: SessionInput<'_>,
        lease_ttl_seconds: Option<i64>,
        event: EventInput<'_>,
    ) -> Result<SessionLeaseResult, InventoryError> {
        self.register_session_with_lease_and_event_builder(
            input,
            lease_ttl_seconds,
            EventMetadata {
                repository_id: event.repository_id,
                worktree_id: event.worktree_id,
                occurred_at: event.occurred_at,
                actor: event.actor,
                action: event.action,
                result: event.result,
            },
            |_, _| event.details_json.to_owned(),
        )
    }

    pub fn register_session_with_lease_and_event_builder<F>(
        &self,
        input: SessionInput<'_>,
        lease_ttl_seconds: Option<i64>,
        event: EventMetadata<'_>,
        details: F,
    ) -> Result<SessionLeaseResult, InventoryError>
    where
        F: FnOnce(&SessionRecord, Option<&LeaseRecord>) -> String,
    {
        if input.id.is_empty() || input.provider.is_empty() {
            return Err(InventoryError::InvalidData(
                "session id and provider cannot be empty".to_owned(),
            ));
        }
        if lease_ttl_seconds.is_some_and(|ttl| ttl <= 0) {
            return Err(InventoryError::InvalidData(
                "lease TTL must be positive".to_owned(),
            ));
        }
        self.connection.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| {
            let session = self.register_session_inner(input)?;
            let lease = match lease_ttl_seconds {
                Some(ttl_seconds) => Some(self.acquire_lease_inner(
                    input.worktree_id,
                    input.id,
                    ttl_seconds,
                    input.now,
                )?),
                None => None,
            };
            let details_json = details(&session, lease.as_ref());
            self.record_event_inner(EventInput {
                repository_id: event.repository_id,
                worktree_id: event.worktree_id,
                occurred_at: event.occurred_at,
                actor: event.actor,
                action: event.action,
                result: event.result,
                details_json: &details_json,
            })?;
            Ok(SessionLeaseResult { session, lease })
        })();
        match result {
            Ok(result) => match self.connection.execute_batch("COMMIT") {
                Ok(()) => Ok(result),
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

    fn register_session_inner(
        &self,
        input: SessionInput<'_>,
    ) -> Result<SessionRecord, InventoryError> {
        if let Some(existing) = self.session_by_id(input.id)? {
            if existing.worktree_id != input.worktree_id || existing.provider != input.provider {
                return Err(InventoryError::Conflict(format!(
                    "session {:?} is already registered to another worktree or provider",
                    input.id
                )));
            }
            let mut statement = self.connection.prepare(
                "UPDATE sessions SET provider_session_id = ?, pid = ?, \
                    process_started_at = ?, terminal_metadata = ?, state = 'active', \
                    last_seen_at = ? WHERE id = ?",
            )?;
            statement.bind_optional_text(1, input.provider_session_id)?;
            statement.bind_i64_option(2, input.pid)?;
            statement.bind_i64_option(3, input.process_started_at)?;
            statement.bind_optional_text(4, input.terminal_metadata)?;
            statement.bind_i64(5, input.now)?;
            statement.bind_text(6, input.id)?;
            statement.expect_done()?;
        } else {
            let mut statement = self.connection.prepare(
                "INSERT INTO sessions( \
                    id, worktree_id, provider, provider_session_id, pid, process_started_at, \
                    terminal_metadata, state, created_at, last_seen_at \
                 ) VALUES (?, ?, ?, ?, ?, ?, ?, 'active', ?, ?)",
            )?;
            statement.bind_text(1, input.id)?;
            statement.bind_i64(2, input.worktree_id)?;
            statement.bind_text(3, input.provider)?;
            statement.bind_optional_text(4, input.provider_session_id)?;
            statement.bind_i64_option(5, input.pid)?;
            statement.bind_i64_option(6, input.process_started_at)?;
            statement.bind_optional_text(7, input.terminal_metadata)?;
            statement.bind_i64(8, input.now)?;
            statement.bind_i64(9, input.now)?;
            statement.expect_done()?;
        }
        self.session_by_id(input.id)?.ok_or_else(|| {
            InventoryError::InvalidData("session upsert did not return a row".to_owned())
        })
    }

    pub fn session_by_id(&self, id: &str) -> Result<Option<SessionRecord>, InventoryError> {
        let mut statement = self.connection.prepare(
            "SELECT id, worktree_id, provider, provider_session_id, pid, process_started_at, \
                    terminal_metadata, state, created_at, last_seen_at \
             FROM sessions WHERE id = ?",
        )?;
        statement.bind_text(1, id)?;
        if !statement.step()? {
            return Ok(None);
        }
        Ok(Some(session_from_statement(&statement)?))
    }

    pub fn touch_session(
        &self,
        session_id: &str,
        now: i64,
    ) -> Result<Option<SessionRecord>, InventoryError> {
        let mut statement = self
            .connection
            .prepare("UPDATE sessions SET last_seen_at = ? WHERE id = ? AND state = 'active'")?;
        statement.bind_i64(1, now)?;
        statement.bind_text(2, session_id)?;
        statement.expect_done()?;
        self.session_by_id(session_id)
    }

    pub fn heartbeat_session_with_lease_and_event(
        &self,
        session_id: &str,
        lease_ttl_seconds: Option<i64>,
        now: i64,
        event: EventInput<'_>,
    ) -> Result<SessionLeaseResult, InventoryError> {
        self.heartbeat_session_with_lease_and_event_builder(
            session_id,
            lease_ttl_seconds,
            now,
            EventMetadata {
                repository_id: event.repository_id,
                worktree_id: event.worktree_id,
                occurred_at: event.occurred_at,
                actor: event.actor,
                action: event.action,
                result: event.result,
            },
            |_, _| event.details_json.to_owned(),
        )
    }

    pub fn heartbeat_session_with_lease_and_event_builder<F>(
        &self,
        session_id: &str,
        lease_ttl_seconds: Option<i64>,
        now: i64,
        event: EventMetadata<'_>,
        details: F,
    ) -> Result<SessionLeaseResult, InventoryError>
    where
        F: FnOnce(&SessionRecord, Option<&LeaseRecord>) -> String,
    {
        if lease_ttl_seconds.is_some_and(|ttl| ttl <= 0) {
            return Err(InventoryError::InvalidData(
                "lease TTL must be positive".to_owned(),
            ));
        }
        self.connection.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| {
            self.expire_leases_inner(now)?;
            let session = self.session_by_id(session_id)?.ok_or_else(|| {
                InventoryError::Conflict(format!("session {session_id:?} is not registered"))
            })?;
            if session.state != "active" {
                return Err(InventoryError::Conflict(format!(
                    "session {session_id:?} is not active (state {:?})",
                    session.state
                )));
            }
            let mut touch = self.connection.prepare(
                "UPDATE sessions SET last_seen_at = ? WHERE id = ? AND state = 'active'",
            )?;
            touch.bind_i64(1, now)?;
            touch.bind_text(2, session_id)?;
            touch.expect_done()?;
            let session = self.session_by_id(session_id)?.ok_or_else(|| {
                InventoryError::InvalidData("heartbeat session disappeared".to_owned())
            })?;
            let lease = match lease_ttl_seconds {
                Some(ttl_seconds) => match self.renew_lease_inner(session_id, ttl_seconds, now)? {
                    Some(lease) => Some(lease),
                    None => Some(self.acquire_lease_inner(
                        session.worktree_id,
                        session_id,
                        ttl_seconds,
                        now,
                    )?),
                },
                None => self.active_lease_for_session(session_id, now)?,
            };
            let details_json = details(&session, lease.as_ref());
            self.record_event_inner(EventInput {
                repository_id: event.repository_id,
                worktree_id: event.worktree_id,
                occurred_at: event.occurred_at,
                actor: event.actor,
                action: event.action,
                result: event.result,
                details_json: &details_json,
            })?;
            Ok(SessionLeaseResult { session, lease })
        })();
        match result {
            Ok(result) => match self.connection.execute_batch("COMMIT") {
                Ok(()) => Ok(result),
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

    pub fn release_session(
        &self,
        session_id: &str,
        now: i64,
    ) -> Result<Option<SessionRecord>, InventoryError> {
        self.connection.execute_batch("BEGIN IMMEDIATE")?;
        let result = self.release_session_inner(session_id, now);
        match result {
            Ok(session) => match self.connection.execute_batch("COMMIT") {
                Ok(()) => Ok(session),
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

    pub fn release_session_with_event(
        &self,
        session_id: &str,
        now: i64,
        event: EventInput<'_>,
    ) -> Result<Option<SessionRecord>, InventoryError> {
        self.connection.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| {
            let session = self.release_session_inner(session_id, now)?;
            if session.is_some() {
                self.record_event_inner(event)?;
            }
            Ok(session)
        })();
        match result {
            Ok(session) => match self.connection.execute_batch("COMMIT") {
                Ok(()) => Ok(session),
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

    fn release_session_inner(
        &self,
        session_id: &str,
        now: i64,
    ) -> Result<Option<SessionRecord>, InventoryError> {
        let mut leases = self.connection.prepare(
            "UPDATE leases SET state = 'released', renewed_at = ? \
             WHERE session_id = ? AND state = 'active'",
        )?;
        leases.bind_i64(1, now)?;
        leases.bind_text(2, session_id)?;
        leases.expect_done()?;
        let mut session = self
            .connection
            .prepare("UPDATE sessions SET state = 'released', last_seen_at = ? WHERE id = ?")?;
        session.bind_i64(1, now)?;
        session.bind_text(2, session_id)?;
        session.expect_done()?;
        self.session_by_id(session_id)
    }

    pub fn acquire_lease(
        &self,
        worktree_id: i64,
        session_id: &str,
        ttl_seconds: i64,
        now: i64,
    ) -> Result<LeaseRecord, InventoryError> {
        if ttl_seconds <= 0 {
            return Err(InventoryError::InvalidData(
                "lease TTL must be positive".to_owned(),
            ));
        }
        self.connection.execute_batch("BEGIN IMMEDIATE")?;
        let result = self.acquire_lease_inner(worktree_id, session_id, ttl_seconds, now);
        match result {
            Ok(lease) => match self.connection.execute_batch("COMMIT") {
                Ok(()) => Ok(lease),
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

    pub fn acquire_lease_with_event(
        &self,
        worktree_id: i64,
        session_id: &str,
        ttl_seconds: i64,
        now: i64,
        event: EventInput<'_>,
    ) -> Result<LeaseRecord, InventoryError> {
        self.acquire_lease_with_event_builder(
            worktree_id,
            session_id,
            ttl_seconds,
            now,
            EventMetadata {
                repository_id: event.repository_id,
                worktree_id: event.worktree_id,
                occurred_at: event.occurred_at,
                actor: event.actor,
                action: event.action,
                result: event.result,
            },
            |_| event.details_json.to_owned(),
        )
    }

    pub fn acquire_lease_with_event_builder<F>(
        &self,
        worktree_id: i64,
        session_id: &str,
        ttl_seconds: i64,
        now: i64,
        event: EventMetadata<'_>,
        details: F,
    ) -> Result<LeaseRecord, InventoryError>
    where
        F: FnOnce(&LeaseRecord) -> String,
    {
        if ttl_seconds <= 0 {
            return Err(InventoryError::InvalidData(
                "lease TTL must be positive".to_owned(),
            ));
        }
        self.connection.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| {
            let lease = self.acquire_lease_inner(worktree_id, session_id, ttl_seconds, now)?;
            let details_json = details(&lease);
            self.record_event_inner(EventInput {
                repository_id: event.repository_id,
                worktree_id: event.worktree_id,
                occurred_at: event.occurred_at,
                actor: event.actor,
                action: event.action,
                result: event.result,
                details_json: &details_json,
            })?;
            Ok(lease)
        })();
        match result {
            Ok(lease) => match self.connection.execute_batch("COMMIT") {
                Ok(()) => Ok(lease),
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

    fn acquire_lease_inner(
        &self,
        worktree_id: i64,
        session_id: &str,
        ttl_seconds: i64,
        now: i64,
    ) -> Result<LeaseRecord, InventoryError> {
        let session = self.session_by_id(session_id)?.ok_or_else(|| {
            InventoryError::Conflict(format!("session {session_id:?} is not registered"))
        })?;
        if session.state != "active" {
            return Err(InventoryError::Conflict(format!(
                "session {session_id:?} is not active (state {:?})",
                session.state
            )));
        }
        if session.worktree_id != worktree_id {
            return Err(InventoryError::Conflict(format!(
                "session {session_id:?} belongs to worktree {}, not {worktree_id}",
                session.worktree_id
            )));
        }
        let mut expire = self.connection.prepare(
            "UPDATE leases SET state = 'expired', renewed_at = ? \
             WHERE worktree_id = ? AND state = 'active' AND expires_at <= ?",
        )?;
        expire.bind_i64(1, now)?;
        expire.bind_i64(2, worktree_id)?;
        expire.bind_i64(3, now)?;
        expire.expect_done()?;

        if let Some(existing) = self.active_lease_for_worktree(worktree_id)? {
            if existing.session_id != session_id {
                return Err(InventoryError::Conflict(format!(
                    "worktree {worktree_id} is leased by session {:?}",
                    existing.session_id
                )));
            }
            let expires_at = now.saturating_add(ttl_seconds);
            let mut statement = self.connection.prepare(
                "UPDATE leases SET renewed_at = ?, expires_at = ?, state = 'active' \
                 WHERE id = ?",
            )?;
            statement.bind_i64(1, now)?;
            statement.bind_i64(2, expires_at)?;
            statement.bind_i64(3, existing.id)?;
            statement.expect_done()?;
            return self.lease_by_id(existing.id)?.ok_or_else(|| {
                InventoryError::InvalidData("renewed lease did not return a row".to_owned())
            });
        }

        let mut statement = self.connection.prepare(
            "INSERT INTO leases( \
                worktree_id, session_id, acquired_at, renewed_at, expires_at, state \
             ) VALUES (?, ?, ?, ?, ?, 'active')",
        )?;
        statement.bind_i64(1, worktree_id)?;
        statement.bind_text(2, session_id)?;
        statement.bind_i64(3, now)?;
        statement.bind_i64(4, now)?;
        statement.bind_i64(5, now.saturating_add(ttl_seconds))?;
        statement.expect_done()?;
        let id = self.connection.last_insert_rowid();
        self.lease_by_id(id)?.ok_or_else(|| {
            InventoryError::InvalidData("lease insert did not return a row".to_owned())
        })
    }

    pub fn renew_lease(
        &self,
        session_id: &str,
        ttl_seconds: i64,
        now: i64,
    ) -> Result<Option<LeaseRecord>, InventoryError> {
        if ttl_seconds <= 0 {
            return Err(InventoryError::InvalidData(
                "lease TTL must be positive".to_owned(),
            ));
        }
        self.renew_lease_inner(session_id, ttl_seconds, now)
    }

    fn renew_lease_inner(
        &self,
        session_id: &str,
        ttl_seconds: i64,
        now: i64,
    ) -> Result<Option<LeaseRecord>, InventoryError> {
        let mut statement = self.connection.prepare(
            "UPDATE leases SET renewed_at = ?, expires_at = ? \
             WHERE session_id = ? AND state = 'active' AND expires_at > ?",
        )?;
        statement.bind_i64(1, now)?;
        statement.bind_i64(2, now.saturating_add(ttl_seconds))?;
        statement.bind_text(3, session_id)?;
        statement.bind_i64(4, now)?;
        statement.expect_done()?;
        self.active_lease_for_session(session_id, now)
    }

    pub fn release_lease(
        &self,
        session_id: &str,
        now: i64,
    ) -> Result<Vec<LeaseRecord>, InventoryError> {
        self.connection.execute_batch("BEGIN IMMEDIATE")?;
        let result = self.release_lease_inner(session_id, now);
        match result {
            Ok(leases) => match self.connection.execute_batch("COMMIT") {
                Ok(()) => Ok(leases),
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

    pub fn release_lease_with_event(
        &self,
        session_id: &str,
        now: i64,
        event: EventInput<'_>,
    ) -> Result<Vec<LeaseRecord>, InventoryError> {
        self.connection.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| {
            let leases = self.release_lease_inner(session_id, now)?;
            self.record_event_inner(event)?;
            Ok(leases)
        })();
        match result {
            Ok(leases) => match self.connection.execute_batch("COMMIT") {
                Ok(()) => Ok(leases),
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

    fn release_lease_inner(
        &self,
        session_id: &str,
        now: i64,
    ) -> Result<Vec<LeaseRecord>, InventoryError> {
        let mut statement = self.connection.prepare(
            "UPDATE leases SET state = 'released', renewed_at = ? \
             WHERE session_id = ? AND state = 'active'",
        )?;
        statement.bind_i64(1, now)?;
        statement.bind_text(2, session_id)?;
        statement.expect_done()?;
        self.leases_for_session(session_id)
    }

    pub fn expire_leases(&self, now: i64) -> Result<u64, InventoryError> {
        self.expire_leases_inner(now)
    }

    fn expire_leases_inner(&self, now: i64) -> Result<u64, InventoryError> {
        let mut statement = self.connection.prepare(
            "UPDATE leases SET state = 'expired', renewed_at = ? \
             WHERE state = 'active' AND expires_at <= ?",
        )?;
        statement.bind_i64(1, now)?;
        statement.bind_i64(2, now)?;
        statement.expect_done()?;
        Ok(self.connection.changes())
    }

    pub fn active_lease_for_worktree(
        &self,
        worktree_id: i64,
    ) -> Result<Option<LeaseRecord>, InventoryError> {
        let mut statement = self.connection.prepare(
            "SELECT id, worktree_id, session_id, acquired_at, renewed_at, expires_at, state \
             FROM leases WHERE worktree_id = ? AND state = 'active' LIMIT 1",
        )?;
        statement.bind_i64(1, worktree_id)?;
        if !statement.step()? {
            return Ok(None);
        }
        Ok(Some(lease_from_statement(&statement)?))
    }

    pub fn active_lease_for_session(
        &self,
        session_id: &str,
        now: i64,
    ) -> Result<Option<LeaseRecord>, InventoryError> {
        let mut statement = self.connection.prepare(
            "SELECT id, worktree_id, session_id, acquired_at, renewed_at, expires_at, state \
             FROM leases WHERE session_id = ? AND state = 'active' AND expires_at > ? LIMIT 1",
        )?;
        statement.bind_text(1, session_id)?;
        statement.bind_i64(2, now)?;
        if !statement.step()? {
            return Ok(None);
        }
        Ok(Some(lease_from_statement(&statement)?))
    }

    pub fn leases_for_session(&self, session_id: &str) -> Result<Vec<LeaseRecord>, InventoryError> {
        let mut statement = self.connection.prepare(
            "SELECT id, worktree_id, session_id, acquired_at, renewed_at, expires_at, state \
             FROM leases WHERE session_id = ? ORDER BY id",
        )?;
        statement.bind_text(1, session_id)?;
        let mut records = Vec::new();
        while statement.step()? {
            records.push(lease_from_statement(&statement)?);
        }
        Ok(records)
    }

    pub fn lease_by_id(&self, id: i64) -> Result<Option<LeaseRecord>, InventoryError> {
        let mut statement = self.connection.prepare(
            "SELECT id, worktree_id, session_id, acquired_at, renewed_at, expires_at, state \
             FROM leases WHERE id = ?",
        )?;
        statement.bind_i64(1, id)?;
        if !statement.step()? {
            return Ok(None);
        }
        Ok(Some(lease_from_statement(&statement)?))
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
        self.record_event_inner(input)
    }

    fn record_event_inner(&self, input: EventInput<'_>) -> Result<EventRecord, InventoryError> {
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

fn apply_migrations(connection: &Connection) -> Result<(), InventoryError> {
    let current_version = {
        let mut statement =
            connection.prepare("SELECT COALESCE(MAX(version), 0) FROM schema_migrations")?;
        if statement.step()? {
            statement.column_i64(0)
        } else {
            0
        }
    };
    if current_version >= 2 {
        return Ok(());
    }

    connection.execute_batch("BEGIN IMMEDIATE")?;
    match connection.execute_batch(MIGRATION_2_SQL) {
        Ok(()) => match connection.execute_batch("COMMIT") {
            Ok(()) => Ok(()),
            Err(error) => {
                let _ = connection.execute_batch("ROLLBACK");
                Err(error)
            }
        },
        Err(error) => {
            let _ = connection.execute_batch("ROLLBACK");
            Err(error)
        }
    }
}

fn prepare_inventory_file(path: &Path) -> Result<std::fs::File, InventoryError> {
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path).map_err(|source| InventoryError::Io {
        operation: format!("prepare inventory file {}", path.display()),
        source,
    })
}

fn inventory_file_identity(path: &Path) -> Result<InventoryFileIdentity, InventoryError> {
    #[cfg(unix)]
    {
        let metadata = fs::metadata(path).map_err(|source| InventoryError::Io {
            operation: format!("inspect inventory identity {}", path.display()),
            source,
        })?;
        Ok(inventory_file_identity_from_metadata(&metadata))
    }
    #[cfg(not(unix))]
    {
        Ok(InventoryFileIdentity::Path(
            fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()),
        ))
    }
}

#[cfg(target_os = "macos")]
fn inventory_file_identity_from_metadata(metadata: &fs::Metadata) -> InventoryFileIdentity {
    use std::os::macos::fs::MetadataExt as MacMetadataExt;
    use std::os::unix::fs::MetadataExt as UnixMetadataExt;
    InventoryFileIdentity::Mac {
        device: metadata.dev(),
        inode: metadata.ino(),
        birth_time: metadata.st_birthtime(),
        birth_time_nanoseconds: metadata.st_birthtime_nsec(),
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn inventory_file_identity_from_metadata(metadata: &fs::Metadata) -> InventoryFileIdentity {
    use std::os::unix::fs::MetadataExt;
    InventoryFileIdentity::Unix {
        device: metadata.dev(),
        inode: metadata.ino(),
        change_time: metadata.ctime(),
        change_time_nanoseconds: metadata.ctime_nsec(),
    }
}

fn restrict_inventory_permissions(path: &Path) -> Result<(), InventoryError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mut permissions = fs::metadata(path)
            .map_err(|source| InventoryError::Io {
                operation: format!("inspect inventory permissions {}", path.display()),
                source,
            })?
            .permissions();
        permissions.set_mode(0o600);
        fs::set_permissions(path, permissions).map_err(|source| InventoryError::Io {
            operation: format!("restrict inventory permissions {}", path.display()),
            source,
        })?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
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

    fn changes(&self) -> u64 {
        unsafe { sqlite3_changes(self.raw) as u64 }
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
    fn sqlite3_changes(database: *mut Sqlite3) -> c_int;
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

fn request_from_statement(statement: &Statement<'_>) -> Result<RequestRecord, InventoryError> {
    Ok(RequestRecord {
        id: statement.column_text_required(0)?,
        operation: statement.column_text_required(1)?,
        state: statement.column_text_required(2)?,
        response_json: statement.column_text(3)?,
        created_at: statement.column_i64(4),
        updated_at: statement.column_i64(5),
    })
}

fn session_from_statement(statement: &Statement<'_>) -> Result<SessionRecord, InventoryError> {
    Ok(SessionRecord {
        id: statement.column_text_required(0)?,
        worktree_id: statement.column_i64(1),
        provider: statement.column_text_required(2)?,
        provider_session_id: statement.column_text(3)?,
        pid: statement.column_text_i64(4)?,
        process_started_at: statement.column_text_i64(5)?,
        terminal_metadata: statement.column_text(6)?,
        state: statement.column_text_required(7)?,
        created_at: statement.column_i64(8),
        last_seen_at: statement.column_i64(9),
    })
}

fn lease_from_statement(statement: &Statement<'_>) -> Result<LeaseRecord, InventoryError> {
    Ok(LeaseRecord {
        id: statement.column_i64(0),
        worktree_id: statement.column_i64(1),
        session_id: statement.column_text_required(2)?,
        acquired_at: statement.column_i64(3),
        renewed_at: statement.column_i64(4),
        expires_at: statement.column_i64(5),
        state: statement.column_text_required(6)?,
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
    use super::{Connection, Inventory, SCHEMA_SQL, SessionInput, apply_migrations};
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

    #[test]
    fn migrates_duplicate_active_leases_before_creating_the_unique_index() {
        let directory = temporary_directory();
        let database = directory.join("inventory.sqlite3");
        {
            let connection = Connection::open(&database).expect("SQLite connection opened");
            connection
                .execute_batch(SCHEMA_SQL)
                .expect("base schema created");
            connection
                .execute_batch(
                    "INSERT INTO repositories(id, common_git_dir, root_path, display_name, added_at, last_seen_at) \
                     VALUES (1, '/repo/.git', '/repo', 'repo', 1, 1); \
                     INSERT INTO worktrees(id, repository_id, path, git_state, lifecycle_state, first_seen_at, last_seen_at) \
                     VALUES (1, 1, '/repo', 'available', 'active', 1, 1); \
                     INSERT INTO sessions(id, worktree_id, provider, state, created_at, last_seen_at) \
                     VALUES ('session-1', 1, 'test', 'active', 1, 1); \
                     INSERT INTO leases(worktree_id, session_id, acquired_at, renewed_at, expires_at, state) \
                     VALUES (1, 'session-1', 1, 1, 100, 'active'), \
                            (1, 'session-1', 2, 2, 200, 'active');",
                )
                .expect("legacy duplicate leases inserted");
            apply_migrations(&connection).expect("schema migration applied");
            let mut statement = connection
                .prepare("SELECT COUNT(*) FROM leases WHERE state = 'active'")
                .expect("lease count prepared");
            assert!(statement.step().expect("lease count queried"));
            assert_eq!(statement.column_i64(0), 1);
        }
        Inventory::open(&database).expect("migrated inventory reopened");
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn request_journal_and_leases_recover_after_interruption() {
        let directory = temporary_directory();
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
        let inventory =
            Inventory::open(directory.join("inventory.sqlite3")).expect("inventory opened");
        let repository_record = inventory
            .register_repository(&repository, 100)
            .expect("repository registered");
        let records = inventory
            .reconcile_repository(&repository, &worktrees, 100)
            .expect("repository reconciled");
        let worktree = &records[0];
        inventory
            .register_session(SessionInput {
                id: "session-1",
                worktree_id: worktree.id,
                provider: "test",
                provider_session_id: Some("provider-1"),
                pid: None,
                process_started_at: None,
                terminal_metadata: None,
                now: 100,
            })
            .expect("session registered");
        let lease = inventory
            .acquire_lease(worktree.id, "session-1", 10, 100)
            .expect("lease acquired");
        assert!(inventory.active_lease_exists(worktree.id, 109).unwrap());
        assert!(!inventory.active_lease_exists(worktree.id, 110).unwrap());
        assert_eq!(inventory.expire_leases(110).unwrap(), 1);
        assert_eq!(
            inventory.lease_by_id(lease.id).unwrap().unwrap().state,
            "expired"
        );

        let started = inventory
            .begin_request("request-1", "create:abc", 100)
            .expect("request started");
        assert_eq!(started.state, "started");
        let restarted = inventory
            .begin_request("request-1", "create:abc", 101)
            .expect("request restarted");
        assert_eq!(restarted.state, "started");
        inventory
            .complete_request("request-1", "{\"ok\":true}", 102)
            .expect("request completed");
        let replay = inventory
            .begin_request("request-1", "create:abc", 103)
            .expect("request replayed");
        assert_eq!(replay.state, "succeeded");
        assert_eq!(replay.response_json.as_deref(), Some("{\"ok\":true}"));
        assert_eq!(repository_record.id, worktree.repository_id);

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
