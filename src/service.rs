use crate::git::{GitError, GitRepository, GitWorktree, GitWorktreeStatus};
use crate::inventory::{
    EventInput, EventMetadata, Inventory, InventoryError, LeaseRecord, RepositoryRecord,
    SessionInput, SessionRecord, WorktreeRecord,
};
use crate::json;
use crate::process::active_processes;
use std::collections::HashMap;
use std::ffi::OsString;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

type RepositoryLockRegistry = Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>;

static REPOSITORY_LOCKS: OnceLock<RepositoryLockRegistry> = OnceLock::new();

pub const DEFAULT_CLEANUP_MINIMUM_AGE_SECONDS: i64 = 24 * 60 * 60;

#[derive(Debug)]
pub enum ServiceError {
    Git(GitError),
    Inventory(InventoryError),
    Io {
        operation: String,
        source: io::Error,
    },
    InvalidRequest(String),
    NotFound(PathBuf),
    UnsafeRemoval {
        path: PathBuf,
        blockers: Vec<String>,
    },
    BranchDeletionFailed {
        branch: String,
        source: GitError,
    },
    LockPoisoned,
}

impl fmt::Display for ServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Git(error) => error.fmt(formatter),
            Self::Inventory(error) => error.fmt(formatter),
            Self::Io { operation, source } => write!(formatter, "{operation}: {source}"),
            Self::InvalidRequest(message) => write!(formatter, "invalid request: {message}"),
            Self::NotFound(path) => write!(formatter, "worktree not found: {}", path.display()),
            Self::UnsafeRemoval { path, blockers } => write!(
                formatter,
                "refusing to remove {}: {}",
                path.display(),
                blockers.join(", ")
            ),
            Self::BranchDeletionFailed { branch, source } => write!(
                formatter,
                "worktree was removed but branch {branch:?} could not be deleted: {source}"
            ),
            Self::LockPoisoned => write!(formatter, "repository mutation lock is poisoned"),
        }
    }
}

impl std::error::Error for ServiceError {}

impl From<GitError> for ServiceError {
    fn from(error: GitError) -> Self {
        Self::Git(error)
    }
}

impl From<InventoryError> for ServiceError {
    fn from(error: InventoryError) -> Self {
        Self::Inventory(error)
    }
}

#[derive(Debug, Clone)]
pub struct CreateRequest {
    pub branch: String,
    pub base: Option<String>,
    pub path: Option<PathBuf>,
    pub idempotency_key: Option<String>,
    pub actor: String,
}

impl CreateRequest {
    pub fn new(branch: impl Into<String>) -> Self {
        Self {
            branch: branch.into(),
            base: None,
            path: None,
            idempotency_key: None,
            actor: "cli".to_owned(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct CreateResult {
    pub worktree: GitWorktree,
    pub record: WorktreeRecord,
    pub already_existed: bool,
}

impl CreateResult {
    pub fn to_json(&self) -> String {
        json::Object::new()
            .number("schema_version", 1)
            .string("operation", "create")
            .bool("already_existed", self.already_existed)
            .string("path", &self.worktree.path.to_string_lossy())
            .optional_string("branch", self.worktree.branch.as_deref())
            .optional_string("head", self.worktree.head.as_deref())
            .string("lifecycle_state", &self.record.lifecycle_state)
            .finish()
    }
}

#[derive(Debug, Clone)]
pub struct LockResult {
    pub worktree: GitWorktree,
    pub already_in_requested_state: bool,
}

impl LockResult {
    pub fn to_json(&self, operation: &str) -> String {
        json::Object::new()
            .number("schema_version", 1)
            .string("operation", operation)
            .bool(
                "already_in_requested_state",
                self.already_in_requested_state,
            )
            .string("path", &self.worktree.path.to_string_lossy())
            .optional_string("branch", self.worktree.branch.as_deref())
            .bool("locked", self.worktree.locked)
            .optional_string("lock_reason", self.worktree.lock_reason.as_deref())
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanupCandidate {
    pub worktree_id: Option<i64>,
    pub path: PathBuf,
    pub branch: Option<String>,
    pub classification: String,
    pub blockers: Vec<String>,
    pub reclaimable_bytes: u64,
}

impl CleanupCandidate {
    pub fn to_json(&self) -> String {
        json::Object::new()
            .optional_number("worktree_id", self.worktree_id.map(|id| id as u64))
            .string("path", &self.path.to_string_lossy())
            .optional_string("branch", self.branch.as_deref())
            .string("classification", &self.classification)
            .raw(
                "blockers",
                json::array(self.blockers.iter().map(|blocker| json::string(blocker))),
            )
            .number("reclaimable_bytes", self.reclaimable_bytes)
            .finish()
    }
}

#[derive(Debug, Clone)]
pub struct RemoveResult {
    pub path: PathBuf,
    pub branch: Option<String>,
    pub branch_deleted: bool,
}

#[derive(Debug, Clone)]
pub struct RegisterSessionRequest {
    pub session_id: String,
    pub worktree: PathBuf,
    pub provider: String,
    pub provider_session_id: Option<String>,
    pub pid: Option<i64>,
    pub process_started_at: Option<i64>,
    pub terminal_metadata: Option<String>,
    pub lease_ttl_seconds: Option<i64>,
    pub actor: String,
}

impl RegisterSessionRequest {
    pub fn new(session_id: impl Into<String>, worktree: impl Into<PathBuf>) -> Self {
        Self {
            session_id: session_id.into(),
            worktree: worktree.into(),
            provider: "terminal".to_owned(),
            provider_session_id: None,
            pid: None,
            process_started_at: None,
            terminal_metadata: None,
            lease_ttl_seconds: None,
            actor: "daemon".to_owned(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionResult {
    pub session: SessionRecord,
    pub lease: Option<LeaseRecord>,
    pub already_registered: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeartbeatResult {
    pub session: SessionRecord,
    pub lease: Option<LeaseRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseResult {
    pub lease: LeaseRecord,
    pub already_active: bool,
}

impl RemoveResult {
    pub fn to_json(&self) -> String {
        json::Object::new()
            .number("schema_version", 1)
            .string("operation", "remove")
            .string("path", &self.path.to_string_lossy())
            .optional_string("branch", self.branch.as_deref())
            .bool("branch_deleted", self.branch_deleted)
            .finish()
    }
}

pub struct RepositoryService {
    repository: GitRepository,
    primary_root: PathBuf,
    minimum_cleanup_age_seconds: i64,
    inventory: Inventory,
    mutation_lock: Arc<Mutex<()>>,
}

struct MutationGuard<'a> {
    _thread_guard: MutexGuard<'a, ()>,
    file: File,
}

impl Drop for MutationGuard<'_> {
    fn drop(&mut self) {
        release_file_lock(&self.file);
    }
}

impl fmt::Debug for RepositoryService {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RepositoryService")
            .field("repository", &self.repository)
            .field("primary_root", &self.primary_root)
            .field(
                "minimum_cleanup_age_seconds",
                &self.minimum_cleanup_age_seconds,
            )
            .field("inventory", &self.inventory)
            .finish_non_exhaustive()
    }
}

impl RepositoryService {
    pub fn open(
        repository: GitRepository,
        inventory_path: impl AsRef<Path>,
    ) -> Result<Self, ServiceError> {
        Self::open_with_minimum_age(
            repository,
            inventory_path,
            DEFAULT_CLEANUP_MINIMUM_AGE_SECONDS,
        )
    }

    pub fn open_with_minimum_age(
        repository: GitRepository,
        inventory_path: impl AsRef<Path>,
        minimum_cleanup_age_seconds: i64,
    ) -> Result<Self, ServiceError> {
        if minimum_cleanup_age_seconds < 0 {
            return Err(ServiceError::InvalidRequest(
                "minimum cleanup age cannot be negative".to_owned(),
            ));
        }
        let primary_root = repository
            .list_worktrees()?
            .into_iter()
            .find(|worktree| !worktree.bare && !worktree.prunable)
            .map(|worktree| worktree.path)
            .unwrap_or_else(|| repository.root.clone());
        let registry = REPOSITORY_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
        let mut locks = registry.lock().map_err(|_| ServiceError::LockPoisoned)?;
        let mutation_lock = locks
            .entry(repository.common_git_dir.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        drop(locks);
        Ok(Self {
            repository,
            primary_root,
            minimum_cleanup_age_seconds,
            inventory: Inventory::open(inventory_path)?,
            mutation_lock,
        })
    }

    pub fn repository(&self) -> &GitRepository {
        &self.repository
    }

    pub fn inventory(&self) -> &Inventory {
        &self.inventory
    }

    pub fn minimum_cleanup_age_seconds(&self) -> i64 {
        self.minimum_cleanup_age_seconds
    }

    pub fn refresh(&self) -> Result<(RepositoryRecord, Vec<WorktreeRecord>), ServiceError> {
        let _guard = self.mutation_guard()?;
        let worktrees = self.repository.list_worktrees()?;
        self.refresh_from_worktrees(&worktrees)
    }

    pub fn register_session(
        &self,
        request: RegisterSessionRequest,
    ) -> Result<SessionResult, ServiceError> {
        let _guard = self.mutation_guard()?;
        validate_session_request(&request)?;
        let worktrees = self.repository.list_worktrees()?;
        let requested = absolute_path(&request.worktree)?;
        let worktree = find_worktree(&worktrees, &requested)
            .ok_or_else(|| ServiceError::NotFound(requested.clone()))?;
        if worktree.bare || worktree.prunable || !worktree.path.is_dir() {
            return Err(ServiceError::InvalidRequest(
                "session worktree must be an available directory".to_owned(),
            ));
        }
        let (repository_record, records) = self.refresh_from_worktrees(&worktrees)?;
        let record = records
            .iter()
            .find(|record| record.path == worktree.path.to_string_lossy())
            .ok_or_else(|| {
                ServiceError::InvalidRequest(
                    "session worktree was not persisted in the inventory".to_owned(),
                )
            })?;
        let existing = self.inventory.session_by_id(&request.session_id)?;
        let registration = self
            .inventory
            .register_session_with_lease_and_event_builder(
                SessionInput {
                    id: &request.session_id,
                    worktree_id: record.id,
                    provider: &request.provider,
                    provider_session_id: request.provider_session_id.as_deref(),
                    pid: request.pid,
                    process_started_at: request.process_started_at,
                    terminal_metadata: request.terminal_metadata.as_deref(),
                    now: unix_now(),
                },
                request.lease_ttl_seconds,
                EventMetadata {
                    repository_id: repository_record.id,
                    worktree_id: Some(record.id),
                    occurred_at: unix_now(),
                    actor: &request.actor,
                    action: "register_session",
                    result: if existing.is_some() {
                        "idempotent-replay"
                    } else {
                        "succeeded"
                    },
                },
                |_, lease| session_details(&request, &requested, lease),
            )?;
        Ok(SessionResult {
            session: registration.session,
            lease: registration.lease,
            already_registered: existing.is_some(),
        })
    }

    pub fn heartbeat_session(
        &self,
        session_id: &str,
        lease_ttl_seconds: Option<i64>,
        actor: &str,
    ) -> Result<HeartbeatResult, ServiceError> {
        let _guard = self.mutation_guard()?;
        if session_id.trim().is_empty() {
            return Err(ServiceError::InvalidRequest(
                "session id cannot be empty".to_owned(),
            ));
        }
        if lease_ttl_seconds.is_some_and(|ttl| ttl <= 0) {
            return Err(ServiceError::InvalidRequest(
                "lease TTL must be positive".to_owned(),
            ));
        }
        let now = unix_now();
        let session = self.inventory.session_by_id(session_id)?.ok_or_else(|| {
            ServiceError::InvalidRequest(format!("session {session_id:?} not found"))
        })?;
        let heartbeat = self
            .inventory
            .heartbeat_session_with_lease_and_event_builder(
                session_id,
                lease_ttl_seconds,
                now,
                EventMetadata {
                    repository_id: self.repository_record_id()?,
                    worktree_id: Some(session.worktree_id),
                    occurred_at: now,
                    actor,
                    action: "heartbeat_session",
                    result: "succeeded",
                },
                |_, lease| {
                    json::Object::new()
                        .string("session_id", session_id)
                        .optional_number(
                            "lease_id",
                            lease.and_then(|lease| u64::try_from(lease.id).ok()),
                        )
                        .finish()
                },
            )?;
        Ok(HeartbeatResult {
            session: heartbeat.session,
            lease: heartbeat.lease,
        })
    }

    pub fn acquire_lease(
        &self,
        worktree: impl AsRef<Path>,
        session_id: &str,
        ttl_seconds: i64,
        actor: &str,
    ) -> Result<LeaseResult, ServiceError> {
        let _guard = self.mutation_guard()?;
        if ttl_seconds <= 0 {
            return Err(ServiceError::InvalidRequest(
                "lease TTL must be positive".to_owned(),
            ));
        }
        let worktrees = self.repository.list_worktrees()?;
        let requested = absolute_path(worktree.as_ref())?;
        let git_worktree = find_worktree(&worktrees, &requested)
            .ok_or_else(|| ServiceError::NotFound(requested.clone()))?;
        let (repository_record, records) = self.refresh_from_worktrees(&worktrees)?;
        let record = records
            .iter()
            .find(|record| record.path == git_worktree.path.to_string_lossy())
            .ok_or_else(|| {
                ServiceError::InvalidRequest(
                    "lease worktree was not persisted in the inventory".to_owned(),
                )
            })?;
        let _session = self.inventory.session_by_id(session_id)?.ok_or_else(|| {
            ServiceError::InvalidRequest(format!("session {session_id:?} not found"))
        })?;
        let now = unix_now();
        let existing = self.inventory.active_lease_for_worktree(record.id)?;
        let lease = self.inventory.acquire_lease_with_event_builder(
            record.id,
            session_id,
            ttl_seconds,
            now,
            EventMetadata {
                repository_id: repository_record.id,
                worktree_id: Some(record.id),
                occurred_at: now,
                actor,
                action: "acquire_lease",
                result: "succeeded",
            },
            |lease| {
                json::Object::new()
                    .string("session_id", session_id)
                    .number("lease_id", lease.id as u64)
                    .number("ttl_seconds", ttl_seconds as u64)
                    .finish()
            },
        )?;
        Ok(LeaseResult {
            lease,
            already_active: existing.is_some_and(|existing| {
                existing.session_id == session_id && existing.expires_at > now
            }),
        })
    }

    pub fn release_lease(
        &self,
        session_id: &str,
        actor: &str,
    ) -> Result<Vec<LeaseRecord>, ServiceError> {
        let _guard = self.mutation_guard()?;
        if session_id.trim().is_empty() {
            return Err(ServiceError::InvalidRequest(
                "session id cannot be empty".to_owned(),
            ));
        }
        let session = self.inventory.session_by_id(session_id)?.ok_or_else(|| {
            ServiceError::InvalidRequest(format!("session {session_id:?} not found"))
        })?;
        let now = unix_now();
        let leases_before = self.inventory.leases_for_session(session_id)?;
        let details = json::Object::new()
            .string("session_id", session_id)
            .number(
                "released_count",
                leases_before
                    .iter()
                    .filter(|lease| lease.state == "active")
                    .count() as u64,
            )
            .finish();
        let leases = self.inventory.release_lease_with_event(
            session_id,
            now,
            EventInput {
                repository_id: self.repository_record_id()?,
                worktree_id: Some(session.worktree_id),
                occurred_at: now,
                actor,
                action: "release_lease",
                result: "succeeded",
                details_json: &details,
            },
        )?;
        Ok(leases)
    }

    pub fn release_session(
        &self,
        session_id: &str,
        actor: &str,
    ) -> Result<Option<SessionRecord>, ServiceError> {
        let _guard = self.mutation_guard()?;
        if session_id.trim().is_empty() {
            return Err(ServiceError::InvalidRequest(
                "session id cannot be empty".to_owned(),
            ));
        }
        let now = unix_now();
        let existing = self.inventory.session_by_id(session_id)?.ok_or_else(|| {
            ServiceError::InvalidRequest(format!("session {session_id:?} not found"))
        })?;
        let details = json::Object::new()
            .string("session_id", session_id)
            .finish();
        let session = self.inventory.release_session_with_event(
            session_id,
            now,
            EventInput {
                repository_id: self.repository_record_id()?,
                worktree_id: Some(existing.worktree_id),
                occurred_at: now,
                actor,
                action: "release_session",
                result: "succeeded",
                details_json: &details,
            },
        )?;
        Ok(session)
    }

    fn refresh_from_worktrees(
        &self,
        worktrees: &[GitWorktree],
    ) -> Result<(RepositoryRecord, Vec<WorktreeRecord>), ServiceError> {
        let now = unix_now();
        let inventory_repository = self.inventory_repository();
        let repository_record = self
            .inventory
            .register_repository(&inventory_repository, now)?;
        let records = self
            .inventory
            .reconcile_repository(&inventory_repository, worktrees, now)?;
        Ok((repository_record, records))
    }

    fn inventory_repository(&self) -> GitRepository {
        let mut repository = self.repository.clone();
        repository.root = self.primary_root.clone();
        repository
    }

    fn repository_record_id(&self) -> Result<i64, ServiceError> {
        Ok(self
            .inventory
            .register_repository(&self.inventory_repository(), unix_now())?
            .id)
    }

    pub fn create(&self, request: CreateRequest) -> Result<CreateResult, ServiceError> {
        let _guard = self.mutation_guard()?;
        validate_create_request(&request)?;
        let current_worktrees = self.repository.list_worktrees()?;
        let target_path = requested_path(
            &self.primary_root,
            &self.repository.common_git_dir,
            &request,
        )?;
        let (repository_record, _) = self.refresh_from_worktrees(&current_worktrees)?;
        let reservation = self.inventory.reserve_creation(
            repository_record.id,
            &target_path,
            &request.branch,
            request.idempotency_key.as_deref(),
            unix_now(),
        )?;

        if reservation.state == "succeeded" {
            if let Some(worktree) = find_worktree(&current_worktrees, &target_path) {
                let (_, records) = self.refresh_from_worktrees(&current_worktrees)?;
                let record = records
                    .iter()
                    .find(|record| record.path == target_path.to_string_lossy())
                    .cloned()
                    .ok_or_else(|| {
                        ServiceError::InvalidRequest(
                            "idempotent create has no matching inventory row".to_owned(),
                        )
                    })?;
                let _ = self.inventory.record_event(EventInput {
                    repository_id: repository_record.id,
                    worktree_id: Some(record.id),
                    occurred_at: unix_now(),
                    actor: &request.actor,
                    action: "create_worktree",
                    result: "idempotent-replay",
                    details_json: &create_details(&target_path, &request.branch),
                });
                return Ok(CreateResult {
                    worktree: worktree.clone(),
                    record,
                    already_existed: true,
                });
            }
            return Err(ServiceError::InvalidRequest(format!(
                "idempotency key already completed for {} but Git no longer lists that worktree",
                target_path.display()
            )));
        }

        if reservation.state != "pending" {
            return Err(ServiceError::InvalidRequest(format!(
                "unsupported creation reservation state {:?}",
                reservation.state
            )));
        }

        if let Some(worktree) = find_worktree(&current_worktrees, &target_path) {
            if worktree.branch.as_deref() == Some(request.branch.as_str()) {
                let (_, records) = self.refresh_from_worktrees(&current_worktrees)?;
                let record = records
                    .iter()
                    .find(|record| record.path == target_path.to_string_lossy())
                    .cloned()
                    .ok_or_else(|| {
                        ServiceError::InvalidRequest(
                            "pending create recovery has no inventory row".to_owned(),
                        )
                    })?;
                self.inventory.set_reservation_state(
                    reservation.id,
                    "succeeded",
                    Some(record.id),
                    Some(unix_now()),
                )?;
                return Ok(CreateResult {
                    worktree: worktree.clone(),
                    record,
                    already_existed: true,
                });
            }
            self.inventory.set_reservation_state(
                reservation.id,
                "failed",
                None,
                Some(unix_now()),
            )?;
            return Err(ServiceError::InvalidRequest(format!(
                "path {} is already linked to another branch",
                target_path.display()
            )));
        }

        if let Some(worktree) = current_worktrees
            .iter()
            .find(|worktree| worktree.branch.as_deref() == Some(request.branch.as_str()))
        {
            self.inventory.set_reservation_state(
                reservation.id,
                "failed",
                None,
                Some(unix_now()),
            )?;
            return Err(ServiceError::InvalidRequest(format!(
                "branch {:?} is already checked out at {}",
                request.branch,
                worktree.path.display()
            )));
        }

        if fs::symlink_metadata(&target_path).is_ok() {
            self.inventory.set_reservation_state(
                reservation.id,
                "failed",
                None,
                Some(unix_now()),
            )?;
            return Err(ServiceError::InvalidRequest(format!(
                "requested worktree path already exists: {}",
                target_path.display()
            )));
        }

        let details = create_details(&target_path, &request.branch);
        self.inventory.record_event(EventInput {
            repository_id: repository_record.id,
            worktree_id: None,
            occurred_at: unix_now(),
            actor: &request.actor,
            action: "create_worktree",
            result: "started",
            details_json: &details,
        })?;

        let mut args = vec![
            OsString::from("worktree"),
            OsString::from("add"),
            OsString::from("-b"),
            OsString::from(&request.branch),
            OsString::from("--"),
            target_path.as_os_str().to_owned(),
        ];
        if let Some(base) = request.base.as_deref() {
            args.push(OsString::from(base));
        }
        if let Err(error) = run_git(&self.primary_root, &args) {
            let _ = self.inventory.set_reservation_state(
                reservation.id,
                "failed",
                None,
                Some(unix_now()),
            );
            let failure_details = json::Object::new()
                .string("path", &target_path.to_string_lossy())
                .string("branch", &request.branch)
                .string("error", &error.to_string())
                .finish();
            let _ = self.inventory.record_event(EventInput {
                repository_id: repository_record.id,
                worktree_id: None,
                occurred_at: unix_now(),
                actor: &request.actor,
                action: "create_worktree",
                result: "failed",
                details_json: &failure_details,
            });
            return Err(error.into());
        }

        let refreshed_worktrees = match self.repository.list_worktrees() {
            Ok(worktrees) => worktrees,
            Err(error) => {
                let _ = self.inventory.set_reservation_state(
                    reservation.id,
                    "failed",
                    None,
                    Some(unix_now()),
                );
                return Err(error.into());
            }
        };
        let created_worktree = find_worktree(&refreshed_worktrees, &target_path)
            .filter(|worktree| worktree.branch.as_deref() == Some(request.branch.as_str()))
            .cloned()
            .ok_or_else(|| {
                ServiceError::InvalidRequest(
                    "Git worktree add succeeded but the new worktree was not found after rescan"
                        .to_owned(),
                )
            })?;
        let (_, records) = self.refresh_from_worktrees(&refreshed_worktrees)?;
        let record = records
            .iter()
            .find(|record| record.path == target_path.to_string_lossy())
            .cloned()
            .ok_or_else(|| {
                ServiceError::InvalidRequest(
                    "created worktree was not persisted in the inventory".to_owned(),
                )
            })?;
        self.inventory.mark_created(record.id, unix_now())?;
        self.inventory.set_reservation_state(
            reservation.id,
            "succeeded",
            Some(record.id),
            Some(unix_now()),
        )?;
        self.inventory.record_event(EventInput {
            repository_id: repository_record.id,
            worktree_id: Some(record.id),
            occurred_at: unix_now(),
            actor: &request.actor,
            action: "create_worktree",
            result: "succeeded",
            details_json: &details,
        })?;
        Ok(CreateResult {
            worktree: created_worktree,
            record,
            already_existed: false,
        })
    }

    pub fn lock(
        &self,
        path: impl AsRef<Path>,
        reason: Option<&str>,
        actor: &str,
    ) -> Result<LockResult, ServiceError> {
        self.set_lock(path.as_ref(), reason, true, actor)
    }

    pub fn unlock(&self, path: impl AsRef<Path>, actor: &str) -> Result<LockResult, ServiceError> {
        self.set_lock(path.as_ref(), None, false, actor)
    }

    fn set_lock(
        &self,
        path: &Path,
        reason: Option<&str>,
        lock: bool,
        actor: &str,
    ) -> Result<LockResult, ServiceError> {
        let _guard = self.mutation_guard()?;
        let current_worktrees = self.repository.list_worktrees()?;
        let requested = absolute_path(path)?;
        let worktree = find_worktree(&current_worktrees, &requested)
            .ok_or_else(|| ServiceError::NotFound(requested.clone()))?;
        if lock && worktree.locked {
            return Ok(LockResult {
                worktree: worktree.clone(),
                already_in_requested_state: true,
            });
        }
        if !lock && !worktree.locked {
            return Ok(LockResult {
                worktree: worktree.clone(),
                already_in_requested_state: true,
            });
        }

        let (repository_record, records) = self.refresh_from_worktrees(&current_worktrees)?;
        let record = records
            .iter()
            .find(|record| record.path == worktree.path.to_string_lossy())
            .cloned();
        let operation = if lock {
            "lock_worktree"
        } else {
            "unlock_worktree"
        };
        let details = json::Object::new()
            .string("path", &worktree.path.to_string_lossy())
            .optional_string("reason", reason)
            .finish();
        self.inventory.record_event(EventInput {
            repository_id: repository_record.id,
            worktree_id: record.as_ref().map(|record| record.id),
            occurred_at: unix_now(),
            actor,
            action: operation,
            result: "started",
            details_json: &details,
        })?;

        let mut args = vec![OsString::from("worktree")];
        if lock {
            args.push(OsString::from("lock"));
            if let Some(reason) = reason {
                args.push(OsString::from("--reason"));
                args.push(OsString::from(reason));
            }
        } else {
            args.push(OsString::from("unlock"));
        }
        args.push(OsString::from("--"));
        args.push(worktree.path.as_os_str().to_owned());
        if let Err(error) = run_git(&self.primary_root, &args) {
            let failure_details = json::Object::new()
                .string("path", &worktree.path.to_string_lossy())
                .string("error", &error.to_string())
                .finish();
            let _ = self.inventory.record_event(EventInput {
                repository_id: repository_record.id,
                worktree_id: record.as_ref().map(|record| record.id),
                occurred_at: unix_now(),
                actor,
                action: operation,
                result: "failed",
                details_json: &failure_details,
            });
            return Err(error.into());
        }

        let refreshed_worktrees = self.repository.list_worktrees()?;
        let refreshed = find_worktree(&refreshed_worktrees, &requested)
            .ok_or_else(|| ServiceError::NotFound(requested.clone()))?
            .clone();
        self.refresh_from_worktrees(&refreshed_worktrees)?;
        self.inventory.record_event(EventInput {
            repository_id: repository_record.id,
            worktree_id: record.as_ref().map(|record| record.id),
            occurred_at: unix_now(),
            actor,
            action: operation,
            result: "succeeded",
            details_json: &details,
        })?;
        Ok(LockResult {
            worktree: refreshed,
            already_in_requested_state: false,
        })
    }

    pub fn cleanup_scan(&self) -> Result<Vec<CleanupCandidate>, ServiceError> {
        self.cleanup_scan_with_minimum_age(self.minimum_cleanup_age_seconds)
    }

    pub fn cleanup_scan_with_minimum_age(
        &self,
        minimum_cleanup_age_seconds: i64,
    ) -> Result<Vec<CleanupCandidate>, ServiceError> {
        validate_cleanup_age(minimum_cleanup_age_seconds)?;
        let _guard = self.mutation_guard()?;
        let worktrees = self.repository.list_worktrees()?;
        let (_, records) = self.refresh_from_worktrees(&worktrees)?;
        let records_by_path: HashMap<_, _> = records
            .iter()
            .map(|record| (record.path.clone(), record))
            .collect();
        let now = unix_now();
        worktrees
            .iter()
            .map(|worktree| {
                let status = self.repository.inspect_worktree(worktree, None);
                let record = records_by_path
                    .get(&worktree.path.to_string_lossy().into_owned())
                    .copied();
                self.assess_worktree(worktree, &status, record, now, minimum_cleanup_age_seconds)
            })
            .collect()
    }

    pub fn remove(
        &self,
        path: impl AsRef<Path>,
        delete_branch: bool,
        actor: &str,
    ) -> Result<RemoveResult, ServiceError> {
        self.remove_with_minimum_age(path, delete_branch, actor, self.minimum_cleanup_age_seconds)
    }

    pub fn remove_with_minimum_age(
        &self,
        path: impl AsRef<Path>,
        delete_branch: bool,
        actor: &str,
        minimum_cleanup_age_seconds: i64,
    ) -> Result<RemoveResult, ServiceError> {
        validate_cleanup_age(minimum_cleanup_age_seconds)?;
        let _guard = self.mutation_guard()?;
        let requested = absolute_path(path.as_ref())?;
        let worktrees = self.repository.list_worktrees()?;
        let worktree = match find_worktree(&worktrees, &requested) {
            Some(worktree) => worktree.clone(),
            None => {
                // A daemon can restart after Git removed the directory but before the
                // inventory response was committed. Reconciliation recognizes the
                // in-flight remove event and makes a retry idempotently successful.
                let (repository_record, records) = self.refresh_from_worktrees(&worktrees)?;
                if let Some(record) = records.iter().find(|record| {
                    record.path == requested.to_string_lossy()
                        && record.lifecycle_state == "removed"
                }) {
                    let branch = record.branch.clone();
                    let branch_deleted = if delete_branch {
                        if let Some(branch) = branch.as_deref() {
                            let existed = local_branch_exists(&self.primary_root, branch)?;
                            if existed {
                                let branch_args = vec![
                                    OsString::from("branch"),
                                    OsString::from("-d"),
                                    OsString::from("--"),
                                    OsString::from(branch),
                                ];
                                run_git(&self.primary_root, &branch_args)?;
                            }
                            let details = json::Object::new().string("branch", branch).finish();
                            self.inventory.record_event(EventInput {
                                repository_id: repository_record.id,
                                worktree_id: Some(record.id),
                                occurred_at: unix_now(),
                                actor,
                                action: "delete_branch",
                                result: if existed {
                                    "succeeded"
                                } else {
                                    "idempotent-replay"
                                },
                                details_json: &details,
                            })?;
                            true
                        } else {
                            false
                        }
                    } else {
                        false
                    };
                    return Ok(RemoveResult {
                        path: requested,
                        branch,
                        branch_deleted,
                    });
                }
                return Err(ServiceError::NotFound(requested));
            }
        };
        let (repository_record, records) = self.refresh_from_worktrees(&worktrees)?;
        let record = records
            .iter()
            .find(|record| record.path == worktree.path.to_string_lossy())
            .ok_or_else(|| {
                ServiceError::InvalidRequest(
                    "worktree was found by Git but not in the inventory".to_owned(),
                )
            })?;
        let status = self.repository.inspect_worktree(&worktree, None);
        let assessment = self.assess_worktree(
            &worktree,
            &status,
            Some(record),
            unix_now(),
            minimum_cleanup_age_seconds,
        )?;
        if assessment.classification != "eligible" {
            return Err(ServiceError::UnsafeRemoval {
                path: requested.clone(),
                blockers: assessment.blockers,
            });
        }

        let details = json::Object::new()
            .string("path", &worktree.path.to_string_lossy())
            .optional_string("branch", worktree.branch.as_deref())
            .bool("delete_branch_requested", delete_branch)
            .finish();
        self.inventory.record_event(EventInput {
            repository_id: repository_record.id,
            worktree_id: Some(record.id),
            occurred_at: unix_now(),
            actor,
            action: "remove_worktree",
            result: "started",
            details_json: &details,
        })?;
        let rechecked_worktrees = self.repository.list_worktrees()?;
        let rechecked_worktree = find_worktree(&rechecked_worktrees, &requested)
            .ok_or_else(|| ServiceError::NotFound(requested.clone()))?;
        if rechecked_worktree.branch != worktree.branch || rechecked_worktree.head != worktree.head
        {
            let failure_details = json::Object::new()
                .string("path", &worktree.path.to_string_lossy())
                .string(
                    "error",
                    "worktree identity changed during removal preflight",
                )
                .finish();
            let _ = self.inventory.record_event(EventInput {
                repository_id: repository_record.id,
                worktree_id: Some(record.id),
                occurred_at: unix_now(),
                actor,
                action: "remove_worktree",
                result: "failed",
                details_json: &failure_details,
            });
            return Err(ServiceError::InvalidRequest(
                "worktree identity changed during removal preflight".to_owned(),
            ));
        }
        let rechecked_status = self.repository.inspect_worktree(rechecked_worktree, None);
        let rechecked_assessment = self.assess_worktree(
            rechecked_worktree,
            &rechecked_status,
            Some(record),
            unix_now(),
            minimum_cleanup_age_seconds,
        )?;
        if rechecked_assessment.classification != "eligible" {
            let failure_details = json::Object::new()
                .string("path", &worktree.path.to_string_lossy())
                .raw(
                    "blockers",
                    json::array(
                        rechecked_assessment
                            .blockers
                            .iter()
                            .map(|blocker| json::string(blocker)),
                    ),
                )
                .finish();
            let _ = self.inventory.record_event(EventInput {
                repository_id: repository_record.id,
                worktree_id: Some(record.id),
                occurred_at: unix_now(),
                actor,
                action: "remove_worktree",
                result: "failed",
                details_json: &failure_details,
            });
            return Err(ServiceError::UnsafeRemoval {
                path: requested,
                blockers: rechecked_assessment.blockers,
            });
        }
        let args = vec![
            OsString::from("worktree"),
            OsString::from("remove"),
            OsString::from("--"),
            worktree.path.as_os_str().to_owned(),
        ];
        if let Err(error) = run_git(&self.primary_root, &args) {
            let failure_details = json::Object::new()
                .string("path", &worktree.path.to_string_lossy())
                .string("error", &error.to_string())
                .finish();
            let _ = self.inventory.record_event(EventInput {
                repository_id: repository_record.id,
                worktree_id: Some(record.id),
                occurred_at: unix_now(),
                actor,
                action: "remove_worktree",
                result: "failed",
                details_json: &failure_details,
            });
            return Err(error.into());
        }

        self.inventory
            .mark_removed(repository_record.id, &worktree.path, unix_now())?;
        self.inventory
            .release_reservations_for_worktree(record.id, unix_now())?;
        self.inventory.record_event(EventInput {
            repository_id: repository_record.id,
            worktree_id: Some(record.id),
            occurred_at: unix_now(),
            actor,
            action: "remove_worktree",
            result: "succeeded",
            details_json: &details,
        })?;

        let mut branch_deleted = false;
        if delete_branch && let Some(branch) = worktree.branch.as_deref() {
            let branch_args = vec![
                OsString::from("branch"),
                OsString::from("-d"),
                OsString::from("--"),
                OsString::from(branch),
            ];
            if let Err(error) = run_git(&self.primary_root, &branch_args) {
                let failure_details = json::Object::new()
                    .string("branch", branch)
                    .string("error", &error.to_string())
                    .finish();
                let _ = self.inventory.record_event(EventInput {
                    repository_id: repository_record.id,
                    worktree_id: Some(record.id),
                    occurred_at: unix_now(),
                    actor,
                    action: "delete_branch",
                    result: "failed",
                    details_json: &failure_details,
                });
                return Err(ServiceError::BranchDeletionFailed {
                    branch: branch.to_owned(),
                    source: error,
                });
            }
            branch_deleted = true;
            let details = json::Object::new().string("branch", branch).finish();
            let _ = self.inventory.record_event(EventInput {
                repository_id: repository_record.id,
                worktree_id: Some(record.id),
                occurred_at: unix_now(),
                actor,
                action: "delete_branch",
                result: "succeeded",
                details_json: &details,
            });
        }

        Ok(RemoveResult {
            path: worktree.path,
            branch: worktree.branch,
            branch_deleted,
        })
    }

    fn assess_worktree(
        &self,
        worktree: &GitWorktree,
        status: &GitWorktreeStatus,
        record: Option<&WorktreeRecord>,
        now: i64,
        minimum_cleanup_age_seconds: i64,
    ) -> Result<CleanupCandidate, ServiceError> {
        let mut blockers = Vec::new();
        let mut classification = "eligible";
        let worktree_id = record.map(|record| record.id);

        if worktree.path == self.primary_root {
            blockers.push("primary repository worktree cannot be removed".to_owned());
            promote_classification(&mut classification, "review");
        }
        if worktree.prunable || !worktree.path.is_dir() {
            blockers.push("Git reports the worktree as unavailable".to_owned());
            promote_classification(&mut classification, "review");
        }
        if worktree.locked {
            blockers.push("Git worktree is locked".to_owned());
            promote_classification(&mut classification, "review");
        }
        if worktree.path.is_dir() {
            match active_processes(&worktree.path) {
                Ok(processes) if !processes.is_empty() => {
                    let process_list = processes
                        .iter()
                        .map(|process| format!("{} ({})", process.command, process.pid))
                        .collect::<Vec<_>>()
                        .join(", ");
                    blockers.push(format!(
                        "active process(es) use this worktree: {process_list}"
                    ));
                    promote_classification(&mut classification, "in-use");
                }
                Ok(_) => {}
                Err(error) => {
                    blockers.push(format!("process ownership is unknown: {error}"));
                    promote_classification(&mut classification, "review");
                }
            }
        }
        if let Some(record) = record
            && self.inventory.active_lease_exists(record.id, now)?
        {
            blockers.push("an active ownership lease exists".to_owned());
            promote_classification(&mut classification, "in-use");
        }
        if let Some(record) = record {
            let observed_at = record.created_at.unwrap_or(record.first_seen_at);
            let age_seconds = now.saturating_sub(observed_at);
            if age_seconds < minimum_cleanup_age_seconds {
                blockers.push(format!(
                    "worktree age is {age_seconds}s; minimum cleanup age is {}s",
                    minimum_cleanup_age_seconds
                ));
                promote_classification(&mut classification, "review");
            }
        }
        if let Some(error) = &status.observation_error {
            blockers.push(format!("observation is incomplete: {error}"));
            promote_classification(&mut classification, "review");
        }
        if status.data.changes.dirty() {
            blockers.push(format!(
                "working tree has {} dirty file(s)",
                status.data.changes.dirty_files()
            ));
            promote_classification(&mut classification, "unsafe");
        }
        if let Some(unique_commits) = status.data.merge.unique_commits {
            if unique_commits > 0 {
                blockers.push(format!("branch has {unique_commits} unique commit(s)"));
                promote_classification(&mut classification, "unsafe");
            }
        } else {
            blockers.push("unique commit count is unknown".to_owned());
            promote_classification(&mut classification, "review");
        }
        if status.data.merge.merged_locally != Some(true) {
            blockers.push(
                "branch is not verified as merged into the local integration branch".to_owned(),
            );
            promote_classification(&mut classification, "review");
        }
        if status.data.merge.remote_integration_branch.is_some()
            && status.data.merge.merged_remotely != Some(true)
        {
            blockers.push(
                "branch is not verified as merged into the configured remote integration branch"
                    .to_owned(),
            );
            promote_classification(&mut classification, "review");
        }

        Ok(CleanupCandidate {
            worktree_id,
            path: worktree.path.clone(),
            branch: worktree.branch.clone(),
            classification: classification.to_owned(),
            blockers,
            reclaimable_bytes: status.data.disk_usage.worktree_bytes,
        })
    }

    fn mutation_guard(&self) -> Result<MutationGuard<'_>, ServiceError> {
        self.inventory.ensure_path_identity()?;
        let thread_guard = self
            .mutation_lock
            .lock()
            .map_err(|_| ServiceError::LockPoisoned)?;
        let lock_path = self
            .repository
            .common_git_dir
            .join("worktree-manager.mutation.lock");
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .map_err(|source| ServiceError::Io {
                operation: format!("open mutation lock {}", lock_path.display()),
                source,
            })?;
        if let Err(source) = acquire_file_lock(&file) {
            drop(thread_guard);
            return Err(ServiceError::Io {
                operation: format!("acquire mutation lock {}", lock_path.display()),
                source,
            });
        }
        Ok(MutationGuard {
            _thread_guard: thread_guard,
            file,
        })
    }
}

pub(crate) fn acquire_file_lock(file: &File) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;

        loop {
            let result = unsafe { flock(file.as_raw_fd(), 2) };
            if result == 0 {
                return Ok(());
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = file;
        Ok(())
    }
}

pub(crate) fn release_file_lock(file: &File) {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        unsafe {
            flock(file.as_raw_fd(), 8);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = file;
    }
}

#[cfg(unix)]
unsafe extern "C" {
    fn flock(
        file_descriptor: std::os::raw::c_int,
        operation: std::os::raw::c_int,
    ) -> std::os::raw::c_int;
}

fn validate_create_request(request: &CreateRequest) -> Result<(), ServiceError> {
    if request.branch.trim().is_empty() {
        return Err(ServiceError::InvalidRequest(
            "branch name cannot be empty".to_owned(),
        ));
    }
    if request.branch.starts_with('-') || request.branch.contains('\0') {
        return Err(ServiceError::InvalidRequest(
            "branch name cannot begin with '-' or contain NUL".to_owned(),
        ));
    }
    if request
        .base
        .as_deref()
        .is_some_and(|base| base.starts_with('-') || base.contains('\0'))
    {
        return Err(ServiceError::InvalidRequest(
            "base ref cannot begin with '-' or contain NUL".to_owned(),
        ));
    }
    if request
        .idempotency_key
        .as_deref()
        .is_some_and(|key| key.is_empty() || key.contains('\0'))
    {
        return Err(ServiceError::InvalidRequest(
            "idempotency key cannot be empty or contain NUL".to_owned(),
        ));
    }
    Ok(())
}

fn validate_session_request(request: &RegisterSessionRequest) -> Result<(), ServiceError> {
    if request.session_id.trim().is_empty() || request.session_id.contains('\0') {
        return Err(ServiceError::InvalidRequest(
            "session id cannot be empty or contain NUL".to_owned(),
        ));
    }
    if request.provider.trim().is_empty() || request.provider.contains('\0') {
        return Err(ServiceError::InvalidRequest(
            "session provider cannot be empty or contain NUL".to_owned(),
        ));
    }
    if request
        .provider_session_id
        .as_deref()
        .is_some_and(|value| value.contains('\0'))
        || request
            .terminal_metadata
            .as_deref()
            .is_some_and(|value| value.contains('\0'))
    {
        return Err(ServiceError::InvalidRequest(
            "session metadata cannot contain NUL".to_owned(),
        ));
    }
    if request.lease_ttl_seconds.is_some_and(|ttl| ttl <= 0) {
        return Err(ServiceError::InvalidRequest(
            "lease TTL must be positive".to_owned(),
        ));
    }
    if request.pid.is_some_and(|pid| pid <= 0) {
        return Err(ServiceError::InvalidRequest(
            "session process id must be positive".to_owned(),
        ));
    }
    Ok(())
}

fn requested_path(
    repository_root: &Path,
    common_git_dir: &Path,
    request: &CreateRequest,
) -> Result<PathBuf, ServiceError> {
    let candidate = request
        .path
        .clone()
        .unwrap_or_else(|| default_worktree_path(repository_root, &request.branch));
    let candidate = if candidate.is_absolute() {
        candidate
    } else {
        repository_root.join(candidate)
    };
    let parent = candidate.parent().ok_or_else(|| {
        ServiceError::InvalidRequest("worktree path has no parent directory".to_owned())
    })?;
    let parent = fs::canonicalize(parent).map_err(|source| ServiceError::Io {
        operation: format!("resolve worktree parent {}", parent.display()),
        source,
    })?;
    let allowed_parent = repository_root
        .parent()
        .ok_or_else(|| {
            ServiceError::InvalidRequest(
                "repository root has no sibling directory for worktrees".to_owned(),
            )
        })
        .and_then(|parent| {
            fs::canonicalize(parent).map_err(|source| ServiceError::Io {
                operation: format!("resolve worktree directory {}", parent.display()),
                source,
            })
        })?;
    if parent != allowed_parent {
        return Err(ServiceError::InvalidRequest(format!(
            "worktree paths must be direct siblings of {}",
            repository_root.display()
        )));
    }
    let file_name = candidate.file_name().ok_or_else(|| {
        ServiceError::InvalidRequest("worktree path has no final component".to_owned())
    })?;
    let path = parent.join(file_name);
    if path == repository_root || path == common_git_dir {
        return Err(ServiceError::InvalidRequest(
            "a repository root or common Git directory cannot be a linked worktree path".to_owned(),
        ));
    }
    Ok(path)
}

fn default_worktree_path(repository_root: &Path, branch: &str) -> PathBuf {
    let repository_name = repository_root
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repository".to_owned());
    let branch_name = branch
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '-'
            }
        })
        .collect::<String>();
    repository_root
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!("{repository_name}-{branch_name}"))
}

fn promote_classification(current: &mut &'static str, candidate: &'static str) {
    if classification_priority(candidate) > classification_priority(current) {
        *current = candidate;
    }
}

fn validate_cleanup_age(minimum_cleanup_age_seconds: i64) -> Result<(), ServiceError> {
    if minimum_cleanup_age_seconds < 0 {
        return Err(ServiceError::InvalidRequest(
            "minimum cleanup age cannot be negative".to_owned(),
        ));
    }
    Ok(())
}

fn classification_priority(classification: &str) -> u8 {
    match classification {
        "in-use" => 4,
        "unsafe" => 3,
        "review" => 2,
        "eligible" => 1,
        _ => 0,
    }
}

fn absolute_path(path: &Path) -> Result<PathBuf, ServiceError> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|source| ServiceError::Io {
                operation: "resolve current directory".to_owned(),
                source,
            })?
            .join(path)
    };
    Ok(canonicalize_with_missing_parent(&absolute).unwrap_or(absolute))
}

fn find_worktree<'a>(worktrees: &'a [GitWorktree], requested: &Path) -> Option<&'a GitWorktree> {
    worktrees.iter().find(|worktree| {
        absolute_path_for_compare(&worktree.path) == absolute_path_for_compare(requested)
    })
}

fn absolute_path_for_compare(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|current| current.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    canonicalize_with_missing_parent(&absolute).unwrap_or(absolute)
}

fn canonicalize_with_missing_parent(path: &Path) -> Option<PathBuf> {
    let mut current = path;
    let mut missing = Vec::new();
    loop {
        if let Ok(mut canonical) = fs::canonicalize(current) {
            for component in missing.iter().rev() {
                canonical.push(component);
            }
            return Some(canonical);
        }
        let name = current.file_name()?.to_owned();
        missing.push(name);
        current = current.parent()?;
    }
}

fn run_git(cwd: &Path, args: &[OsString]) -> Result<(), GitError> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_PAGER", "cat")
        .env("LC_ALL", "C")
        .output()
        .map_err(|source| GitError::Io {
            operation: format!("run Git in {}", cwd.display()),
            source,
        })?;
    if output.status.success() {
        return Ok(());
    }
    Err(GitError::Command {
        args: args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" "),
        code: output.status.code(),
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
    })
}

fn local_branch_exists(cwd: &Path, branch: &str) -> Result<bool, GitError> {
    let args = [
        OsString::from("show-ref"),
        OsString::from("--verify"),
        OsString::from("--quiet"),
        OsString::from("--"),
        OsString::from(format!("refs/heads/{branch}")),
    ];
    let output = Command::new("git")
        .args(args.iter())
        .current_dir(cwd)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_PAGER", "cat")
        .env("LC_ALL", "C")
        .output()
        .map_err(|source| GitError::Io {
            operation: format!("check Git branch in {}", cwd.display()),
            source,
        })?;
    if output.status.success() {
        return Ok(true);
    }
    if output.status.code() == Some(1) {
        return Ok(false);
    }
    Err(GitError::Command {
        args: args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" "),
        code: output.status.code(),
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
    })
}

fn create_details(path: &Path, branch: &str) -> String {
    json::Object::new()
        .string("path", &path.to_string_lossy())
        .string("branch", branch)
        .finish()
}

fn session_details(
    request: &RegisterSessionRequest,
    path: &Path,
    lease: Option<&LeaseRecord>,
) -> String {
    json::Object::new()
        .string("session_id", &request.session_id)
        .string("path", &path.to_string_lossy())
        .string("provider", &request.provider)
        .optional_string(
            "provider_session_id",
            request.provider_session_id.as_deref(),
        )
        .optional_number(
            "lease_id",
            lease.and_then(|lease| u64::try_from(lease.id).ok()),
        )
        .finish()
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_secs()).ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::{CreateRequest, RegisterSessionRequest, RepositoryService, ServiceError};
    use crate::git::GitRepository;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn creates_idempotently_and_records_lifecycle_events() {
        let directory = temporary_directory();
        let root = directory.join("repository");
        initialize_repository(&root);
        let path = directory.join("feature");
        let repository = GitRepository::discover(&root).expect("repository discovered");
        let service = RepositoryService::open_with_minimum_age(
            repository,
            directory.join("inventory.sqlite3"),
            0,
        )
        .expect("service opened");
        let mut request = CreateRequest::new("feature");
        request.path = Some(path.clone());
        request.idempotency_key = Some("create-feature".to_owned());
        let created = service.create(request.clone()).expect("worktree created");
        assert!(!created.already_existed);
        assert_eq!(created.worktree.branch.as_deref(), Some("feature"));
        let canonical_path = fs::canonicalize(&path).expect("created path should be canonical");
        let replay = service.create(request).expect("create replayed");
        assert!(replay.already_existed);
        assert_eq!(replay.worktree.path, canonical_path);

        let events = service
            .inventory()
            .events(created.record.repository_id)
            .expect("events listed");
        assert!(
            events
                .iter()
                .any(|event| { event.action == "create_worktree" && event.result == "succeeded" })
        );
        service
            .remove(&path, true, "test")
            .expect("clean worktree removed");
        assert!(!path.exists());
        let mut recreate = CreateRequest::new("feature");
        recreate.path = Some(path.clone());
        recreate.idempotency_key = Some("create-feature-again".to_owned());
        let recreated = service.create(recreate).expect("worktree recreated");
        assert!(!recreated.already_existed);
        service
            .remove(&path, true, "test")
            .expect("recreated worktree removed");

        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn blocks_dirty_worktree_removal() {
        let directory = temporary_directory();
        let root = directory.join("repository");
        initialize_repository(&root);
        let path = directory.join("dirty");
        let repository = GitRepository::discover(&root).expect("repository discovered");
        let service = RepositoryService::open_with_minimum_age(
            repository,
            directory.join("inventory.sqlite3"),
            0,
        )
        .expect("service opened");
        let mut request = CreateRequest::new("dirty");
        request.path = Some(path.clone());
        service.create(request).expect("worktree created");
        fs::write(path.join("untracked.txt"), "keep me\n").expect("file written");
        let error = service
            .remove(&path, false, "test")
            .expect_err("dirty worktree should be blocked");
        assert!(matches!(error, ServiceError::UnsafeRemoval { .. }));
        assert!(path.exists());

        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn active_session_lease_blocks_removal_until_released() {
        let directory = temporary_directory();
        let root = directory.join("repository");
        initialize_repository(&root);
        let path = directory.join("leased");
        let repository = GitRepository::discover(&root).expect("repository discovered");
        let service = RepositoryService::open_with_minimum_age(
            repository,
            directory.join("inventory.sqlite3"),
            0,
        )
        .expect("service opened");
        let mut create = CreateRequest::new("leased");
        create.path = Some(path.clone());
        let created = service.create(create).expect("worktree created");

        let mut session = RegisterSessionRequest::new("session-1", path.clone());
        session.provider = "test".to_owned();
        session.lease_ttl_seconds = Some(3600);
        service
            .register_session(session)
            .expect("session registered");
        let events = service
            .inventory()
            .events(created.record.repository_id)
            .expect("session event listed");
        assert!(events.iter().any(|event| {
            event.action == "register_session"
                && event.details_json.contains("\"lease_id\":")
                && !event.details_json.contains("\"lease_id\":null")
        }));
        let error = service
            .remove(&path, false, "test")
            .expect_err("leased worktree should be blocked");
        match error {
            ServiceError::UnsafeRemoval { blockers, .. } => {
                assert!(blockers.iter().any(|blocker| blocker.contains("lease")));
            }
            other => panic!("unexpected removal error: {other:?}"),
        }
        service
            .release_session("session-1", "test")
            .expect("session released");
        assert!(
            service
                .heartbeat_session("session-1", Some(3600), "test")
                .is_err()
        );
        assert!(
            service
                .acquire_lease(&path, "session-1", 3600, "test")
                .is_err()
        );
        service
            .remove(&path, false, "test")
            .expect("released worktree should be removable");

        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn reconciles_an_external_removal_as_missing() {
        let directory = temporary_directory();
        let root = directory.join("repository");
        initialize_repository(&root);
        let path = directory.join("external");
        let repository = GitRepository::discover(&root).expect("repository discovered");
        let service = RepositoryService::open_with_minimum_age(
            repository,
            directory.join("inventory.sqlite3"),
            0,
        )
        .expect("service opened");
        let mut request = CreateRequest::new("external");
        request.path = Some(path.clone());
        let created = service.create(request).expect("worktree created");

        let output = Command::new("git")
            .args(["worktree", "remove", "--"])
            .arg(&created.worktree.path)
            .current_dir(&root)
            .output()
            .expect("git should be installed");
        assert!(
            output.status.success(),
            "external removal failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let (_, records) = service.refresh().expect("repository refreshed");
        let record = records
            .iter()
            .find(|record| record.path == created.worktree.path.to_string_lossy())
            .expect("removed worktree remains in inventory");
        assert_eq!(record.lifecycle_state, "missing");

        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn retries_an_interrupted_remove_after_git_already_removed_the_worktree() {
        let directory = temporary_directory();
        let root = directory.join("repository");
        initialize_repository(&root);
        let path = directory.join("interrupted");
        let repository = GitRepository::discover(&root).expect("repository discovered");
        let service = RepositoryService::open_with_minimum_age(
            repository,
            directory.join("inventory.sqlite3"),
            0,
        )
        .expect("service opened");
        let mut create = CreateRequest::new("interrupted");
        create.path = Some(path.clone());
        let created = service.create(create).expect("worktree created");
        service
            .inventory()
            .record_event(crate::inventory::EventInput {
                repository_id: created.record.repository_id,
                worktree_id: Some(created.record.id),
                occurred_at: 200,
                actor: "test",
                action: "remove_worktree",
                result: "started",
                details_json: "{}",
            })
            .expect("remove start event recorded");
        run_git(
            &root,
            &[
                "worktree",
                "remove",
                "--",
                path.to_str().expect("path should be UTF-8"),
            ],
        );
        let (_, retry_records) = service.refresh().expect("inventory should reconcile");
        assert_eq!(
            retry_records
                .iter()
                .find(|record| record.id == created.record.id)
                .map(|record| record.lifecycle_state.as_str()),
            Some("removed")
        );

        let result = service
            .remove(&path, true, "retry")
            .expect("interrupted remove should be replayable");
        assert_eq!(
            result.path,
            fs::canonicalize(&directory)
                .expect("fixture directory should be canonical")
                .join("interrupted")
        );
        assert!(result.branch_deleted);

        let _ = fs::remove_dir_all(directory);
    }

    fn initialize_repository(root: &Path) {
        fs::create_dir(root).expect("repository directory created");
        run_git(root, &["init", "-q", "-b", "main"]);
        run_git(root, &["config", "user.name", "Service Test"]);
        run_git(root, &["config", "user.email", "service@example.test"]);
        fs::write(root.join("README.md"), "service\n").expect("file written");
        run_git(root, &["add", "README.md"]);
        run_git(root, &["commit", "-q", "-m", "initial"]);
    }

    fn temporary_directory() -> PathBuf {
        let base = std::env::temp_dir();
        for attempt in 0..100 {
            let timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock should be after Unix epoch")
                .as_nanos();
            let path = base.join(format!(
                "worktree-manager-service-test-{}-{timestamp}-{attempt}",
                std::process::id()
            ));
            if fs::create_dir(&path).is_ok() {
                return path;
            }
        }
        panic!("could not allocate a temporary directory");
    }

    fn run_git(cwd: &Path, args: &[&str]) {
        let output = Command::new("git")
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
