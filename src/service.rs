use crate::git::{GitError, GitRepository, GitWorktree, GitWorktreeStatus};
use crate::inventory::{EventInput, Inventory, InventoryError, RepositoryRecord, WorktreeRecord};
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

    pub fn refresh(&self) -> Result<(RepositoryRecord, Vec<WorktreeRecord>), ServiceError> {
        let _guard = self.mutation_guard()?;
        let worktrees = self.repository.list_worktrees()?;
        self.refresh_from_worktrees(&worktrees)
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
                self.assess_worktree(worktree, &status, record, now)
            })
            .collect()
    }

    pub fn remove(
        &self,
        path: impl AsRef<Path>,
        delete_branch: bool,
        actor: &str,
    ) -> Result<RemoveResult, ServiceError> {
        let _guard = self.mutation_guard()?;
        let requested = absolute_path(path.as_ref())?;
        let worktrees = self.repository.list_worktrees()?;
        let worktree = find_worktree(&worktrees, &requested)
            .ok_or_else(|| ServiceError::NotFound(requested.clone()))?
            .clone();
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
        let assessment = self.assess_worktree(&worktree, &status, Some(record), unix_now())?;
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
            if age_seconds < self.minimum_cleanup_age_seconds {
                blockers.push(format!(
                    "worktree age is {age_seconds}s; minimum cleanup age is {}s",
                    self.minimum_cleanup_age_seconds
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

fn acquire_file_lock(file: &File) -> io::Result<()> {
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

fn release_file_lock(file: &File) {
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
    if path.is_absolute() {
        if let Ok(canonical) = fs::canonicalize(path) {
            return Ok(canonical);
        }
        return Ok(path.to_path_buf());
    }
    let current = std::env::current_dir().map_err(|source| ServiceError::Io {
        operation: "resolve current directory".to_owned(),
        source,
    })?;
    let path = current.join(path);
    if let Ok(canonical) = fs::canonicalize(&path) {
        Ok(canonical)
    } else {
        Ok(path)
    }
}

fn find_worktree<'a>(worktrees: &'a [GitWorktree], requested: &Path) -> Option<&'a GitWorktree> {
    worktrees.iter().find(|worktree| {
        absolute_path_for_compare(&worktree.path) == absolute_path_for_compare(requested)
    })
}

fn absolute_path_for_compare(path: &Path) -> PathBuf {
    if let Ok(canonical) = fs::canonicalize(path) {
        canonical
    } else if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|current| current.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
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

fn create_details(path: &Path, branch: &str) -> String {
    json::Object::new()
        .string("path", &path.to_string_lossy())
        .string("branch", branch)
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
    use super::{CreateRequest, RepositoryService, ServiceError};
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
