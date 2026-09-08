use crate::git::GitRepository;
use crate::inventory::{Inventory, LeaseRecord, SessionRecord, WorktreeRecord};
use crate::json::{self, Value};
use crate::service::{
    CreateRequest, DEFAULT_CLEANUP_MINIMUM_AGE_SECONDS, RegisterSessionRequest, RepositoryService,
    ServiceError, acquire_file_lock, release_file_lock,
};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};

#[cfg(unix)]
unsafe extern "C" {
    fn geteuid() -> u32;
    fn umask(mask: u32) -> u32;
}

const PROTOCOL_VERSION: i64 = 1;
const MAX_REQUEST_BYTES: usize = 1024 * 1024;
const MAX_CONNECTIONS: usize = 64;
const MAX_UNIX_SOCKET_PATH_BYTES: usize = 100;
const DEFAULT_CLIENT_RETRIES: u32 = 3;

struct ConnectionGuard(Arc<AtomicUsize>);

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

struct RequestLockPaths {
    path: PathBuf,
    identity: PathBuf,
}

struct RequestJournalGuard(Vec<File>);

impl RequestJournalGuard {
    fn add_lock(&mut self, lock_path: &Path) -> Result<(), DaemonError> {
        let file = open_request_lock(lock_path)?;
        acquire_file_lock(&file).map_err(|source| DaemonError::Io {
            operation: format!("acquire request journal lock {}", lock_path.display()),
            source,
        })?;
        self.0.push(file);
        Ok(())
    }
}

impl Drop for RequestJournalGuard {
    fn drop(&mut self) {
        for file in &self.0 {
            release_file_lock(file);
        }
    }
}

#[derive(Debug)]
pub enum DaemonError {
    Io {
        operation: String,
        source: io::Error,
    },
    Git(crate::git::GitError),
    Service(ServiceError),
    Protocol(String),
    Remote {
        code: String,
        message: String,
    },
    Unsupported,
}

impl fmt::Display for DaemonError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { operation, source } => write!(formatter, "{operation}: {source}"),
            Self::Git(error) => error.fmt(formatter),
            Self::Service(error) => error.fmt(formatter),
            Self::Protocol(message) => write!(formatter, "daemon protocol error: {message}"),
            Self::Remote { code, message } => {
                write!(formatter, "daemon request failed ({code}): {message}")
            }
            Self::Unsupported => write!(formatter, "daemon IPC is unsupported on this platform"),
        }
    }
}

impl std::error::Error for DaemonError {}

impl From<crate::git::GitError> for DaemonError {
    fn from(error: crate::git::GitError) -> Self {
        Self::Git(error)
    }
}

impl From<ServiceError> for DaemonError {
    fn from(error: ServiceError) -> Self {
        Self::Service(error)
    }
}

impl From<crate::inventory::InventoryError> for DaemonError {
    fn from(error: crate::inventory::InventoryError) -> Self {
        Self::Service(ServiceError::Inventory(error))
    }
}

#[derive(Debug, Clone)]
pub struct DaemonConfig {
    pub repository: GitRepository,
    pub inventory_path: PathBuf,
    pub socket_path: PathBuf,
    pub minimum_cleanup_age_seconds: i64,
}

impl DaemonConfig {
    pub fn new(
        repository: GitRepository,
        inventory_path: impl Into<PathBuf>,
        socket_path: impl Into<PathBuf>,
    ) -> Self {
        Self {
            repository,
            inventory_path: inventory_path.into(),
            socket_path: socket_path.into(),
            minimum_cleanup_age_seconds: DEFAULT_CLEANUP_MINIMUM_AGE_SECONDS,
        }
    }
}

pub struct DaemonServer {
    service: RepositoryService,
    socket_path: PathBuf,
    worker_config: DaemonConfig,
    inventory_identity: String,
    request_lock_paths: RequestLockPaths,
    request_lock: Arc<Mutex<()>>,
    active_connections: Arc<AtomicUsize>,
}

impl fmt::Debug for DaemonServer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DaemonServer")
            .field("socket_path", &self.socket_path)
            .field("service", &self.service)
            .finish()
    }
}

impl DaemonServer {
    pub fn open(config: DaemonConfig) -> Result<Self, DaemonError> {
        if config.minimum_cleanup_age_seconds < 0 {
            return Err(DaemonError::Protocol(
                "minimum cleanup age cannot be negative".to_owned(),
            ));
        }
        let path_lock_path = request_path_lock_path(&config.inventory_path)?;
        let mut startup_request_guard = request_path_guard(&path_lock_path)?;
        let service = RepositoryService::open_with_minimum_age(
            config.repository.clone(),
            config.inventory_path.clone(),
            config.minimum_cleanup_age_seconds,
        )?;
        let inventory_identity = service.inventory().identity_key();
        let request_lock_paths = request_journal_lock_path(service.inventory())?;
        startup_request_guard.add_lock(&request_lock_paths.identity)?;
        // Reconciliation on startup turns interrupted Git operations into explicit inventory
        // states before the daemon accepts a retry.
        service.refresh()?;
        Ok(Self {
            service,
            socket_path: config.socket_path.clone(),
            worker_config: config,
            inventory_identity,
            request_lock_paths,
            request_lock: Arc::new(Mutex::new(())),
            active_connections: Arc::new(AtomicUsize::new(0)),
        })
    }

    pub fn service(&self) -> &RepositoryService {
        &self.service
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    #[cfg(unix)]
    pub fn serve_forever(&self) -> Result<(), DaemonError> {
        let shutdown = AtomicBool::new(false);
        self.serve(&shutdown)
    }

    #[cfg(unix)]
    pub fn serve(&self, shutdown: &AtomicBool) -> Result<(), DaemonError> {
        prepare_socket_path(&self.socket_path)?;
        let listener = bind_restricted_socket(&self.socket_path)?;
        set_socket_permissions(&self.socket_path)?;
        listener
            .set_nonblocking(true)
            .map_err(|source| DaemonError::Io {
                operation: format!("configure daemon socket {}", self.socket_path.display()),
                source,
            })?;

        let result = loop {
            if shutdown.load(Ordering::Relaxed) {
                break Ok(());
            }
            match listener.accept() {
                Ok((stream, _)) => {
                    if self
                        .active_connections
                        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                            (active < MAX_CONNECTIONS).then_some(active + 1)
                        })
                        .is_err()
                    {
                        drop(stream);
                        continue;
                    }
                    let config = self.worker_config.clone();
                    let initial_inventory_identity = self.inventory_identity.clone();
                    let request_lock = Arc::clone(&self.request_lock);
                    let active_connections = Arc::clone(&self.active_connections);
                    thread::spawn(move || {
                        let _connection_guard = ConnectionGuard(Arc::clone(&active_connections));
                        let mut stream = stream;
                        let Ok(Some(first_byte)) = wait_for_request_byte(&mut stream) else {
                            return;
                        };
                        let opened = (|| -> Result<
                            (RepositoryService, String, RequestLockPaths),
                            DaemonError,
                        > {
                            let Ok(_request_guard) = request_lock.lock() else {
                                return Err(DaemonError::Protocol(
                                    "daemon request lock is poisoned".to_owned(),
                                ));
                            };
                            let path_lock_path = request_path_lock_path(&config.inventory_path)?;
                            let mut open_guard = request_path_guard(&path_lock_path)?;
                            let service = RepositoryService::open_with_minimum_age(
                                config.repository.clone(),
                                config.inventory_path.clone(),
                                config.minimum_cleanup_age_seconds,
                            )?;
                            let inventory_identity = service.inventory().identity_key();
                            let request_lock_paths = request_journal_lock_path(service.inventory())?;
                            open_guard.add_lock(&request_lock_paths.identity)?;
                            if inventory_identity != initial_inventory_identity {
                                service.refresh()?;
                            }
                            Ok((service, inventory_identity, request_lock_paths))
                        })();
                        let Ok((service, inventory_identity, request_lock_paths)) = opened else {
                            return;
                        };
                        let server = Self {
                            service,
                            socket_path: config.socket_path.clone(),
                            worker_config: config,
                            inventory_identity,
                            request_lock_paths,
                            request_lock,
                            active_connections,
                        };
                        let _ = server.handle_connection_with_prefix(stream, Some(first_byte));
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(source) => {
                    break Err(DaemonError::Io {
                        operation: format!(
                            "accept daemon connection {}",
                            self.socket_path.display()
                        ),
                        source,
                    });
                }
            }
        };
        let _ = fs::remove_file(&self.socket_path);
        result
    }

    #[cfg(not(unix))]
    pub fn serve_forever(&self) -> Result<(), DaemonError> {
        Err(DaemonError::Unsupported)
    }

    #[cfg(not(unix))]
    pub fn serve(&self, _shutdown: &AtomicBool) -> Result<(), DaemonError> {
        Err(DaemonError::Unsupported)
    }

    #[cfg(unix)]
    fn handle_connection_with_prefix(
        &self,
        stream: UnixStream,
        mut prefix: Option<u8>,
    ) -> Result<(), DaemonError> {
        stream
            .set_read_timeout(Some(Duration::from_millis(250)))
            .map_err(|source| DaemonError::Io {
                operation: "set daemon client read timeout".to_owned(),
                source,
            })?;
        stream
            .set_write_timeout(Some(Duration::from_millis(250)))
            .map_err(|source| DaemonError::Io {
                operation: "set daemon client write timeout".to_owned(),
                source,
            })?;
        let mut writer = stream;
        loop {
            let line_result = {
                let mut reader = DeadlineReader::new(&mut writer, Duration::from_millis(250));
                read_request_line_with_prefix(&mut reader, prefix.take())
            };
            let line = match line_result {
                Ok(Some(line)) => line,
                Ok(None) => return Ok(()),
                Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                    let response =
                        error_response(None, "request_too_large", "request exceeds 1 MiB");
                    write_response(&mut writer, &response)?;
                    return Ok(());
                }
                Err(error) if error.kind() == io::ErrorKind::InvalidInput => {
                    let response = error_response(None, "invalid_request", &error.to_string());
                    write_response(&mut writer, &response)?;
                    return Ok(());
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                    ) =>
                {
                    return Ok(());
                }
                Err(source) => {
                    return Err(DaemonError::Io {
                        operation: "read daemon request".to_owned(),
                        source,
                    });
                }
            };
            let response = match self.handle_line(&line) {
                Ok(response) => response,
                Err(DaemonError::Protocol(message)) => {
                    let request_id = json::parse(&line).ok().and_then(|value| {
                        value
                            .optional_string("request_id")
                            .ok()
                            .flatten()
                            .map(str::to_owned)
                    });
                    error_response(request_id.as_deref(), "invalid_request", &message)
                }
                Err(error) => return Err(error),
            };
            write_response(&mut writer, &response)?;
        }
    }

    pub fn handle_line(&self, input: &str) -> Result<String, DaemonError> {
        let value = json::parse(input).map_err(|error| DaemonError::Protocol(error.to_string()))?;
        let request_id = value
            .optional_string("request_id")
            .map_err(DaemonError::Protocol)?
            .map(str::to_owned);
        let request_id = match request_id {
            Some(request_id) if !request_id.trim().is_empty() => request_id,
            _ => {
                return Ok(error_response(
                    None,
                    "invalid_request",
                    "request_id is required",
                ));
            }
        };
        let schema_version = value
            .optional_i64("schema_version")
            .map_err(DaemonError::Protocol)?
            .unwrap_or(PROTOCOL_VERSION);
        if schema_version != PROTOCOL_VERSION {
            return Ok(error_response(
                Some(&request_id),
                "unsupported_schema",
                "unsupported daemon protocol schema version",
            ));
        }
        let method = value
            .required_string("method")
            .map_err(DaemonError::Protocol)?
            .to_owned();
        let empty_params = Value::Object(Vec::new());
        let params = match value.object_field("params") {
            Some(params) => params,
            None => &empty_params,
        };
        params.as_object().map_err(DaemonError::Protocol)?;

        if is_mutating_method(&method) {
            return self.handle_idempotent(&request_id, &method, params);
        }
        match self.execute(&request_id, &method, params) {
            Ok(result) => Ok(success_response(&request_id, &result)),
            Err(error) => Ok(error_response(
                Some(&request_id),
                service_error_code(&error),
                &error.to_string(),
            )),
        }
    }

    fn handle_idempotent(
        &self,
        request_id: &str,
        method: &str,
        params: &Value,
    ) -> Result<String, DaemonError> {
        self.service.inventory().ensure_path_identity()?;
        let current_identity = self.service.inventory().identity_key();
        if current_identity != self.inventory_identity {
            return Ok(error_response(
                Some(request_id),
                "inventory_replaced",
                "inventory database changed while daemon was running",
            ));
        }
        let _request_guard = self
            .request_lock
            .lock()
            .map_err(|_| DaemonError::Protocol("daemon request lock is poisoned".to_owned()))?;
        let _request_file_guard = request_journal_guard(&self.request_lock_paths)?;
        self.service.inventory().ensure_path_identity()?;
        let current_identity = self.service.inventory().identity_key();
        if current_identity != self.inventory_identity {
            return Ok(error_response(
                Some(request_id),
                "inventory_replaced",
                "inventory database changed while daemon was running",
            ));
        }
        let operation = request_operation(method, params);
        let record =
            match self
                .service
                .inventory()
                .begin_request(request_id, &operation, unix_now())
            {
                Ok(record) => record,
                Err(error @ crate::inventory::InventoryError::Conflict(_)) => {
                    return Ok(error_response(
                        Some(request_id),
                        "conflict",
                        &error.to_string(),
                    ));
                }
                Err(error) => return Err(error.into()),
            };
        if record.state == "succeeded" {
            return record.response_json.ok_or_else(|| {
                DaemonError::Protocol("succeeded request has no stored response".to_owned())
            });
        }
        let response = match self.execute(request_id, method, params) {
            Ok(result) => success_response(request_id, &result),
            Err(error) => {
                let response = error_response(
                    Some(request_id),
                    service_error_code(&error),
                    &error.to_string(),
                );
                self.service
                    .inventory()
                    .fail_request(request_id, unix_now())?;
                return Ok(response);
            }
        };
        if response.len() > MAX_REQUEST_BYTES {
            let response = error_response(
                Some(request_id),
                "response_too_large",
                "daemon response exceeds 1 MiB; use a narrower request",
            );
            self.service
                .inventory()
                .complete_request(request_id, &response, unix_now())?;
            return Ok(response);
        }
        self.service
            .inventory()
            .complete_request(request_id, &response, unix_now())?;
        Ok(response)
    }

    fn execute(
        &self,
        request_id: &str,
        method: &str,
        params: &Value,
    ) -> Result<String, DaemonError> {
        match method {
            "ping" => Ok(json::Object::new()
                .string("status", "ok")
                .number("pid", u64::from(std::process::id()))
                .string(
                    "repository_root",
                    &self.service.repository().root.to_string_lossy(),
                )
                .string(
                    "inventory_path",
                    &self.service.inventory().path().to_string_lossy(),
                )
                .string("inventory_identity", &self.inventory_identity)
                .signed_number(
                    "minimum_cleanup_age_seconds",
                    self.service.minimum_cleanup_age_seconds(),
                )
                .finish()),
            "refresh" => {
                let (repository, worktrees) = self.service.refresh()?;
                Ok(json::Object::new()
                    .number("repository_id", repository.id as u64)
                    .raw(
                        "worktrees",
                        json::array(worktrees.iter().map(worktree_record_to_json)),
                    )
                    .finish())
            }
            "create" => {
                let branch = params
                    .required_string("branch")
                    .map_err(DaemonError::Protocol)?;
                let mut request = CreateRequest::new(branch);
                request.base = params
                    .optional_string("base")
                    .map_err(DaemonError::Protocol)?
                    .map(str::to_owned);
                request.path = params
                    .optional_string("path")
                    .map_err(DaemonError::Protocol)?
                    .map(PathBuf::from);
                request.idempotency_key = Some(
                    params
                        .optional_string("idempotency_key")
                        .map_err(DaemonError::Protocol)?
                        .unwrap_or(request_id)
                        .to_owned(),
                );
                request.actor = params
                    .optional_string("actor")
                    .map_err(DaemonError::Protocol)?
                    .unwrap_or("ipc")
                    .to_owned();
                Ok(self.service.create(request)?.to_json())
            }
            "lock" => {
                let path = PathBuf::from(
                    params
                        .required_string("path")
                        .map_err(DaemonError::Protocol)?,
                );
                let reason = params
                    .optional_string("reason")
                    .map_err(DaemonError::Protocol)?;
                let actor = params
                    .optional_string("actor")
                    .map_err(DaemonError::Protocol)?
                    .unwrap_or("ipc");
                Ok(self.service.lock(&path, reason, actor)?.to_json("lock"))
            }
            "unlock" => {
                let path = PathBuf::from(
                    params
                        .required_string("path")
                        .map_err(DaemonError::Protocol)?,
                );
                let actor = params
                    .optional_string("actor")
                    .map_err(DaemonError::Protocol)?
                    .unwrap_or("ipc");
                Ok(self.service.unlock(&path, actor)?.to_json("unlock"))
            }
            "cleanup_scan" => {
                let base = params
                    .optional_string("base")
                    .map_err(DaemonError::Protocol)?;
                let minimum_age_seconds =
                    requested_cleanup_age(params, self.service.minimum_cleanup_age_seconds())?;
                Ok(json::Object::new()
                    .number("schema_version", PROTOCOL_VERSION as u64)
                    .string("operation", "cleanup_scan")
                    .optional_string("base", base)
                    .signed_number("minimum_age_seconds", minimum_age_seconds)
                    .raw(
                        "candidates",
                        json::array(
                            self.service
                                .cleanup_scan_with_minimum_age_and_base(minimum_age_seconds, base)?
                                .iter()
                                .map(|candidate| candidate.to_json()),
                        ),
                    )
                    .finish())
            }
            "remove" => {
                let path = PathBuf::from(
                    params
                        .required_string("path")
                        .map_err(DaemonError::Protocol)?,
                );
                let delete_branch = params
                    .optional_bool("delete_branch")
                    .map_err(DaemonError::Protocol)?
                    .unwrap_or(false);
                let actor = params
                    .optional_string("actor")
                    .map_err(DaemonError::Protocol)?
                    .unwrap_or("ipc");
                let minimum_age_seconds =
                    requested_cleanup_age(params, self.service.minimum_cleanup_age_seconds())?;
                Ok(self
                    .service
                    .remove_with_minimum_age(&path, delete_branch, actor, minimum_age_seconds)?
                    .to_json())
            }
            "register_session" => {
                let session_id = params
                    .optional_string("session_id")
                    .map_err(DaemonError::Protocol)?
                    .unwrap_or(request_id)
                    .to_owned();
                let mut request = RegisterSessionRequest::new(
                    session_id,
                    PathBuf::from(
                        params
                            .required_string("path")
                            .map_err(DaemonError::Protocol)?,
                    ),
                );
                request.provider = params
                    .optional_string("provider")
                    .map_err(DaemonError::Protocol)?
                    .unwrap_or("terminal")
                    .to_owned();
                request.provider_session_id = params
                    .optional_string("provider_session_id")
                    .map_err(DaemonError::Protocol)?
                    .map(str::to_owned);
                request.pid = params.optional_i64("pid").map_err(DaemonError::Protocol)?;
                request.process_started_at = params
                    .optional_i64("process_started_at")
                    .map_err(DaemonError::Protocol)?;
                request.terminal_metadata = params
                    .optional_string("terminal_metadata")
                    .map_err(DaemonError::Protocol)?
                    .map(str::to_owned);
                request.lease_ttl_seconds = params
                    .optional_i64("lease_ttl_seconds")
                    .map_err(DaemonError::Protocol)?;
                request.actor = params
                    .optional_string("actor")
                    .map_err(DaemonError::Protocol)?
                    .unwrap_or("ipc")
                    .to_owned();
                let result = self.service.register_session(request)?;
                Ok(json::Object::new()
                    .string("session_id", &result.session.id)
                    .bool("already_registered", result.already_registered)
                    .raw("session", session_to_json(&result.session))
                    .raw(
                        "lease",
                        result
                            .lease
                            .as_ref()
                            .map(lease_to_json)
                            .unwrap_or_else(|| "null".to_owned()),
                    )
                    .finish())
            }
            "heartbeat_session" => {
                let session_id = params
                    .required_string("session_id")
                    .map_err(DaemonError::Protocol)?;
                let ttl = params
                    .optional_i64("lease_ttl_seconds")
                    .map_err(DaemonError::Protocol)?;
                let actor = params
                    .optional_string("actor")
                    .map_err(DaemonError::Protocol)?
                    .unwrap_or("ipc");
                let result = self.service.heartbeat_session(session_id, ttl, actor)?;
                Ok(json::Object::new()
                    .raw("session", session_to_json(&result.session))
                    .raw(
                        "lease",
                        result
                            .lease
                            .as_ref()
                            .map(lease_to_json)
                            .unwrap_or_else(|| "null".to_owned()),
                    )
                    .finish())
            }
            "acquire_lease" => {
                let path = PathBuf::from(
                    params
                        .required_string("path")
                        .map_err(DaemonError::Protocol)?,
                );
                let session_id = params
                    .required_string("session_id")
                    .map_err(DaemonError::Protocol)?;
                let ttl = params
                    .optional_i64("lease_ttl_seconds")
                    .map_err(DaemonError::Protocol)?
                    .ok_or_else(|| {
                        DaemonError::Protocol("lease_ttl_seconds is required".to_owned())
                    })?;
                let actor = params
                    .optional_string("actor")
                    .map_err(DaemonError::Protocol)?
                    .unwrap_or("ipc");
                let result = self.service.acquire_lease(&path, session_id, ttl, actor)?;
                Ok(json::Object::new()
                    .bool("already_active", result.already_active)
                    .raw("lease", lease_to_json(&result.lease))
                    .finish())
            }
            "release_lease" => {
                let session_id = params
                    .required_string("session_id")
                    .map_err(DaemonError::Protocol)?;
                let actor = params
                    .optional_string("actor")
                    .map_err(DaemonError::Protocol)?
                    .unwrap_or("ipc");
                let leases = self.service.release_lease(session_id, actor)?;
                Ok(json::Object::new()
                    .raw("leases", json::array(leases.iter().map(lease_to_json)))
                    .finish())
            }
            "release_session" => {
                let session_id = params
                    .required_string("session_id")
                    .map_err(DaemonError::Protocol)?;
                let actor = params
                    .optional_string("actor")
                    .map_err(DaemonError::Protocol)?
                    .unwrap_or("ipc");
                let session = self.service.release_session(session_id, actor)?;
                Ok(json::Object::new()
                    .raw(
                        "session",
                        session
                            .as_ref()
                            .map(session_to_json)
                            .unwrap_or_else(|| "null".to_owned()),
                    )
                    .finish())
            }
            other => Err(DaemonError::Protocol(format!(
                "unknown daemon method {other:?}"
            ))),
        }
    }
}

#[cfg(unix)]
fn read_request_line(reader: &mut impl Read) -> io::Result<Option<String>> {
    read_request_line_with_prefix(reader, None)
}

#[cfg(unix)]
fn read_request_line_with_prefix(
    reader: &mut impl Read,
    mut prefix: Option<u8>,
) -> io::Result<Option<String>> {
    let mut bytes = Vec::with_capacity(256);
    let mut byte = [0u8; 1];
    loop {
        let result = match prefix.take() {
            Some(prefix) => {
                byte[0] = prefix;
                Ok(1)
            }
            None => reader.read(&mut byte),
        };
        match result {
            Ok(0) if bytes.is_empty() => return Ok(None),
            Ok(0) => {
                return String::from_utf8(bytes).map(Some).map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidInput, "request is not UTF-8")
                });
            }
            Ok(_) if byte[0] == b'\n' => {
                if bytes.last() == Some(&b'\r') {
                    bytes.pop();
                }
                return String::from_utf8(bytes).map(Some).map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidInput, "request is not UTF-8")
                });
            }
            Ok(_) => {
                if bytes.len() >= MAX_REQUEST_BYTES {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "request exceeds maximum size",
                    ));
                }
                bytes.push(byte[0]);
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(unix)]
fn wait_for_request_byte(stream: &mut UnixStream) -> io::Result<Option<u8>> {
    stream.set_read_timeout(Some(Duration::from_millis(250)))?;
    let mut byte = [0u8; 1];
    match stream.read(&mut byte) {
        Ok(0) => Ok(None),
        Ok(_) => Ok(Some(byte[0])),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
            ) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
struct DeadlineReader<'stream> {
    stream: &'stream mut UnixStream,
    deadline: Instant,
}

#[cfg(unix)]
impl<'stream> DeadlineReader<'stream> {
    fn new(stream: &'stream mut UnixStream, timeout: Duration) -> Self {
        Self {
            stream,
            deadline: Instant::now() + timeout,
        }
    }
}

#[cfg(unix)]
impl Read for DeadlineReader<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "deadline reached while reading daemon line",
            ));
        }
        self.stream.set_read_timeout(Some(remaining))?;
        self.stream.read(buffer)
    }
}

pub struct DaemonClient {
    socket_path: PathBuf,
    retries: u32,
    retry_delay: Duration,
}

impl fmt::Debug for DaemonClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DaemonClient")
            .field("socket_path", &self.socket_path)
            .field("retries", &self.retries)
            .field("retry_delay", &self.retry_delay)
            .finish()
    }
}

impl DaemonClient {
    pub fn new(socket_path: impl Into<PathBuf>) -> Self {
        Self {
            socket_path: socket_path.into(),
            retries: DEFAULT_CLIENT_RETRIES,
            retry_delay: Duration::from_millis(50),
        }
    }

    pub fn with_retries(mut self, retries: u32) -> Self {
        self.retries = retries;
        self
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    #[cfg(unix)]
    pub fn request_result(
        &self,
        request_id: &str,
        method: &str,
        params_json: &str,
    ) -> Result<String, DaemonError> {
        let response = self.request(request_id, method, params_json)?;
        let value =
            json::parse(&response).map_err(|error| DaemonError::Protocol(error.to_string()))?;
        value
            .object_field("result")
            .map(Value::to_json)
            .ok_or_else(|| DaemonError::Protocol("daemon response has no result".to_owned()))
    }

    #[cfg(not(unix))]
    pub fn request_result(
        &self,
        _request_id: &str,
        _method: &str,
        _params_json: &str,
    ) -> Result<String, DaemonError> {
        Err(DaemonError::Unsupported)
    }

    #[cfg(unix)]
    pub fn request(
        &self,
        request_id: &str,
        method: &str,
        params_json: &str,
    ) -> Result<String, DaemonError> {
        self.request_with_sender(request_id, method, params_json, send_once)
    }

    #[cfg(unix)]
    fn request_with_sender<F>(
        &self,
        request_id: &str,
        method: &str,
        params_json: &str,
        mut sender: F,
    ) -> Result<String, DaemonError>
    where
        F: FnMut(&Path, &str) -> Result<String, DaemonError>,
    {
        if request_id.trim().is_empty() || method.trim().is_empty() {
            return Err(DaemonError::Protocol(
                "request id and method cannot be empty".to_owned(),
            ));
        }
        let params = json::parse(params_json)
            .map_err(|error| DaemonError::Protocol(format!("invalid params: {error}")))?;
        params.as_object().map_err(DaemonError::Protocol)?;
        let request = json::Object::new()
            .number("schema_version", PROTOCOL_VERSION as u64)
            .string("request_id", request_id)
            .string("method", method)
            .raw("params", params_json.to_owned())
            .finish();

        let mut last_error = None;
        for attempt in 0..=self.retries {
            match sender(&self.socket_path, &request) {
                Ok(response) => match interpret_response(response, request_id) {
                    Ok(response) => return Ok(response),
                    Err(error) if is_retryable(&error) => last_error = Some(error),
                    Err(error) => return Err(error),
                },
                Err(error) => {
                    last_error = Some(error);
                }
            }
            if attempt < self.retries {
                thread::sleep(self.retry_delay);
            }
        }
        Err(last_error.unwrap_or_else(|| DaemonError::Protocol("request failed".to_owned())))
    }

    #[cfg(not(unix))]
    pub fn request(
        &self,
        _request_id: &str,
        _method: &str,
        _params_json: &str,
    ) -> Result<String, DaemonError> {
        Err(DaemonError::Unsupported)
    }
}

pub(crate) fn inventory_identity(inventory_path: &Path) -> Result<String, DaemonError> {
    #[cfg(target_os = "macos")]
    {
        use std::os::macos::fs::MetadataExt as MacMetadataExt;
        use std::os::unix::fs::MetadataExt as UnixMetadataExt;

        let metadata = fs::metadata(inventory_path).map_err(|source| DaemonError::Io {
            operation: "inspect inventory identity".to_owned(),
            source,
        })?;
        Ok(format!(
            "macos:{:x}:{:x}:{}:{}",
            metadata.dev(),
            metadata.ino(),
            metadata.st_birthtime(),
            metadata.st_birthtime_nsec()
        ))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        use std::os::unix::fs::MetadataExt;

        let metadata = fs::metadata(inventory_path).map_err(|source| DaemonError::Io {
            operation: "inspect inventory identity".to_owned(),
            source,
        })?;
        Ok(format!("unix:{:x}:{:x}", metadata.dev(), metadata.ino()))
    }
    #[cfg(not(unix))]
    {
        fs::canonicalize(inventory_path)
            .unwrap_or_else(|_| inventory_path.to_path_buf())
            .into_os_string()
            .into_string()
            .map_err(|_| DaemonError::Protocol("inventory path is not valid UTF-8".to_owned()))
    }
}

#[cfg(unix)]
fn bind_restricted_socket(path: &Path) -> Result<UnixListener, DaemonError> {
    // UnixListener::bind creates the socket before callers can chmod it. Set a
    // restrictive umask for this short, single-threaded bind window so the
    // socket is never world-accessible while it is already listening.
    let previous_umask = unsafe { umask(0o077) };
    let result = UnixListener::bind(path);
    unsafe {
        umask(previous_umask);
    }
    result.map_err(|source| DaemonError::Io {
        operation: format!("bind daemon socket {}", path.display()),
        source,
    })
}

fn request_journal_guard(
    lock_paths: &RequestLockPaths,
) -> Result<RequestJournalGuard, DaemonError> {
    let mut guard = request_path_guard(&lock_paths.path)?;
    guard.add_lock(&lock_paths.identity)?;
    Ok(guard)
}

fn request_path_guard(lock_path: &Path) -> Result<RequestJournalGuard, DaemonError> {
    let mut guard = RequestJournalGuard(Vec::new());
    guard.add_lock(lock_path)?;
    Ok(guard)
}

fn request_journal_lock_path(inventory: &Inventory) -> Result<RequestLockPaths, DaemonError> {
    #[cfg(unix)]
    {
        let directory = request_lock_directory()?;
        Ok(RequestLockPaths {
            path: request_path_lock_path(inventory.path())?,
            identity: directory.join(format!(
                "identity-{:016x}.request.lock",
                stable_string_hash(&inventory.identity_key())
            )),
        })
    }
    #[cfg(not(unix))]
    {
        Ok(RequestLockPaths {
            path: request_path_lock_path(inventory.path())?,
            identity: inventory.path().with_extension("identity.request.lock"),
        })
    }
}

fn request_path_lock_path(inventory_path: &Path) -> Result<PathBuf, DaemonError> {
    #[cfg(unix)]
    {
        let directory = request_lock_directory()?;
        let path_hash = stable_path_hash(inventory_path);
        Ok(directory.join(format!("path-{path_hash:016x}.request.lock")))
    }
    #[cfg(not(unix))]
    {
        Ok(inventory_path.with_extension("request.lock"))
    }
}

fn stable_path_hash(path: &Path) -> u64 {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|current| current.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    stable_string_hash(&path.to_string_lossy())
}

fn stable_string_hash(value: &str) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[cfg(unix)]
fn request_lock_directory() -> Result<PathBuf, DaemonError> {
    use std::os::unix::fs::DirBuilderExt;

    let user_id = unsafe { geteuid() };
    let directory = std::env::temp_dir().join(format!("worktree-manager-locks-{user_id}"));
    match fs::symlink_metadata(&directory) {
        Ok(metadata) => validate_request_lock_directory(&directory, &metadata)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let mut builder = fs::DirBuilder::new();
            builder.mode(0o700);
            match builder.create(&directory) {
                Ok(()) => {
                    // DirBuilder's mode is applied atomically by mkdir, so another
                    // user cannot observe a permissive directory during startup.
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(source) => {
                    return Err(DaemonError::Io {
                        operation: format!("create request lock directory {}", directory.display()),
                        source,
                    });
                }
            }
            let metadata = fs::symlink_metadata(&directory).map_err(|source| DaemonError::Io {
                operation: format!("inspect request lock directory {}", directory.display()),
                source,
            })?;
            validate_request_lock_directory(&directory, &metadata)?;
        }
        Err(source) => {
            return Err(DaemonError::Io {
                operation: format!("inspect request lock directory {}", directory.display()),
                source,
            });
        }
    }
    Ok(directory)
}

#[cfg(unix)]
fn validate_request_lock_directory(
    directory: &Path,
    metadata: &fs::Metadata,
) -> Result<(), DaemonError> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(DaemonError::Io {
            operation: format!("inspect request lock directory {}", directory.display()),
            source: io::Error::new(
                io::ErrorKind::PermissionDenied,
                "request lock directory must be a real directory",
            ),
        });
    }
    if metadata.uid() != unsafe { geteuid() } {
        return Err(DaemonError::Io {
            operation: format!("inspect request lock directory {}", directory.display()),
            source: io::Error::new(
                io::ErrorKind::PermissionDenied,
                "request lock directory is owned by another user",
            ),
        });
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(DaemonError::Io {
            operation: format!("restrict request lock directory {}", directory.display()),
            source: io::Error::new(
                io::ErrorKind::PermissionDenied,
                "request lock directory is accessible by another user",
            ),
        });
    }
    Ok(())
}

fn open_request_lock(lock_path: &Path) -> Result<File, DaemonError> {
    #[cfg(unix)]
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

    let mut create = OpenOptions::new();
    create.create_new(true).read(true).write(true);
    #[cfg(unix)]
    create.mode(0o600);
    match create.open(lock_path) {
        Ok(file) => Ok(file),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let metadata = fs::symlink_metadata(lock_path).map_err(|source| DaemonError::Io {
                operation: format!("inspect request journal lock {}", lock_path.display()),
                source,
            })?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(DaemonError::Io {
                    operation: format!("open request journal lock {}", lock_path.display()),
                    source: io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "request journal lock must be a regular file",
                    ),
                });
            }
            #[cfg(unix)]
            {
                if metadata.uid() != unsafe { geteuid() }
                    || metadata.permissions().mode() & 0o077 != 0
                {
                    return Err(DaemonError::Io {
                        operation: format!("open request journal lock {}", lock_path.display()),
                        source: io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            "request journal lock has unsafe ownership or permissions",
                        ),
                    });
                }
            }
            OpenOptions::new()
                .read(true)
                .write(true)
                .open(lock_path)
                .map_err(|source| DaemonError::Io {
                    operation: format!("open request journal lock {}", lock_path.display()),
                    source,
                })
        }
        Err(source) => Err(DaemonError::Io {
            operation: format!("create request journal lock {}", lock_path.display()),
            source,
        }),
    }
}

#[cfg(unix)]
fn send_once(socket_path: &Path, request: &str) -> Result<String, DaemonError> {
    let mut stream = UnixStream::connect(socket_path).map_err(|source| DaemonError::Io {
        operation: format!("connect to daemon socket {}", socket_path.display()),
        source,
    })?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|source| DaemonError::Io {
            operation: "set daemon read timeout".to_owned(),
            source,
        })?;
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .map_err(|source| DaemonError::Io {
            operation: "set daemon write timeout".to_owned(),
            source,
        })?;
    stream
        .write_all(request.as_bytes())
        .and_then(|_| stream.write_all(b"\n"))
        .map_err(|source| DaemonError::Io {
            operation: "write daemon request".to_owned(),
            source,
        })?;
    stream.flush().map_err(|source| DaemonError::Io {
        operation: "flush daemon request".to_owned(),
        source,
    })?;
    let mut reader = DeadlineReader::new(&mut stream, Duration::from_secs(5));
    match read_request_line(&mut reader) {
        Ok(Some(response)) => Ok(response),
        Ok(None) => Err(DaemonError::Io {
            operation: "read daemon response".to_owned(),
            source: io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "daemon closed before responding",
            ),
        }),
        Err(source) if source.kind() == io::ErrorKind::InvalidData => Err(DaemonError::Protocol(
            "daemon response is too large".to_owned(),
        )),
        Err(source) => Err(DaemonError::Io {
            operation: "read daemon response".to_owned(),
            source,
        }),
    }
}

fn interpret_response(response: String, expected_request_id: &str) -> Result<String, DaemonError> {
    let value = json::parse(&response).map_err(|error| DaemonError::Protocol(error.to_string()))?;
    let schema_version = value
        .optional_i64("schema_version")
        .map_err(DaemonError::Protocol)?
        .ok_or_else(|| DaemonError::Protocol("daemon response has no schema_version".to_owned()))?;
    if schema_version != PROTOCOL_VERSION {
        return Err(DaemonError::Protocol(
            "daemon response schema_version does not match client".to_owned(),
        ));
    }
    let response_request_id = value
        .optional_string("request_id")
        .map_err(DaemonError::Protocol)?
        .ok_or_else(|| DaemonError::Protocol("daemon response has no request_id".to_owned()))?;
    if response_request_id != expected_request_id {
        return Err(DaemonError::Protocol(
            "daemon response request_id does not match request".to_owned(),
        ));
    }
    match value.object_field("ok") {
        Some(Value::Bool(true)) => Ok(response),
        Some(Value::Bool(false)) => {
            let error = value.object_field("error").ok_or_else(|| {
                DaemonError::Protocol("error response has no error field".to_owned())
            })?;
            let code = error
                .required_string("code")
                .map_err(DaemonError::Protocol)?
                .to_owned();
            let message = error
                .required_string("message")
                .map_err(DaemonError::Protocol)?
                .to_owned();
            Err(DaemonError::Remote { code, message })
        }
        _ => Err(DaemonError::Protocol(
            "daemon response has no boolean ok field".to_owned(),
        )),
    }
}

fn is_retryable(error: &DaemonError) -> bool {
    match error {
        DaemonError::Io { .. } | DaemonError::Protocol(_) => true,
        DaemonError::Remote { code, .. } => code == "inventory_replaced",
        _ => false,
    }
}

fn is_mutating_method(method: &str) -> bool {
    matches!(
        method,
        "refresh"
            | "create"
            | "lock"
            | "unlock"
            | "cleanup_scan"
            | "remove"
            | "register_session"
            | "heartbeat_session"
            | "acquire_lease"
            | "release_lease"
            | "release_session"
    )
}

fn requested_cleanup_age(params: &Value, default: i64) -> Result<i64, DaemonError> {
    params
        .optional_i64("minimum_age_seconds")
        .map_err(DaemonError::Protocol)
        .map(|value| value.unwrap_or(default))
}

fn request_operation(method: &str, params: &Value) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    hash = stable_hash_bytes(hash, method.as_bytes());
    hash = stable_hash_value(hash, params);
    format!("{method}:{hash:016x}")
}

fn stable_hash_bytes(mut hash: u64, bytes: &[u8]) -> u64 {
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn stable_hash_value(mut hash: u64, value: &Value) -> u64 {
    match value {
        Value::Null => hash = stable_hash_bytes(hash, b"n"),
        Value::Bool(value) => {
            hash = stable_hash_bytes(hash, if *value { b"bt" } else { b"bf" });
        }
        Value::Number(value) => {
            hash = stable_hash_bytes(hash, b"i");
            hash = stable_hash_string(hash, &value.to_string());
        }
        Value::String(value) => {
            hash = stable_hash_bytes(hash, b"s");
            hash = stable_hash_string(hash, value);
        }
        Value::Array(values) => {
            hash = stable_hash_bytes(hash, b"a");
            hash = stable_hash_usize(hash, values.len());
            for value in values {
                hash = stable_hash_value(hash, value);
            }
        }
        Value::Object(fields) => {
            let mut sorted_fields = fields.iter().collect::<Vec<_>>();
            sorted_fields.sort_by(|left, right| left.0.cmp(&right.0));
            hash = stable_hash_bytes(hash, b"o");
            hash = stable_hash_usize(hash, sorted_fields.len());
            for (key, value) in sorted_fields {
                hash = stable_hash_bytes(hash, b"o");
                hash = stable_hash_string(hash, key);
                hash = stable_hash_value(hash, value);
            }
        }
    }
    hash
}

fn stable_hash_string(hash: u64, value: &str) -> u64 {
    stable_hash_usize(stable_hash_bytes(hash, value.as_bytes()), value.len())
}

fn stable_hash_usize(hash: u64, value: usize) -> u64 {
    stable_hash_bytes(hash, &value.to_le_bytes())
}

fn success_response(request_id: &str, result: &str) -> String {
    json::Object::new()
        .number("schema_version", PROTOCOL_VERSION as u64)
        .string("request_id", request_id)
        .bool("ok", true)
        .raw("result", result.to_owned())
        .finish()
}

fn error_response(request_id: Option<&str>, code: &str, message: &str) -> String {
    json::Object::new()
        .number("schema_version", PROTOCOL_VERSION as u64)
        .optional_string("request_id", request_id)
        .bool("ok", false)
        .raw(
            "error",
            json::Object::new()
                .string("code", code)
                .string("message", message)
                .finish(),
        )
        .finish()
}

fn service_error_code(error: &DaemonError) -> &'static str {
    match error {
        DaemonError::Service(ServiceError::UnsafeRemoval { .. }) => "unsafe_removal",
        DaemonError::Service(ServiceError::NotFound(_)) => "not_found",
        DaemonError::Service(ServiceError::InvalidRequest(_)) => "invalid_request",
        DaemonError::Service(ServiceError::Inventory(
            crate::inventory::InventoryError::Conflict(_),
        )) => "conflict",
        DaemonError::Protocol(_) => "invalid_request",
        DaemonError::Git(_) => "git_error",
        DaemonError::Service(_) => "service_error",
        _ => "daemon_error",
    }
}

fn write_response(writer: &mut impl Write, response: &str) -> Result<(), DaemonError> {
    if response.len() > MAX_REQUEST_BYTES {
        return Err(DaemonError::Protocol(
            "daemon response exceeds 1 MiB".to_owned(),
        ));
    }
    writer
        .write_all(response.as_bytes())
        .and_then(|_| writer.write_all(b"\n"))
        .and_then(|_| writer.flush())
        .map_err(|source| DaemonError::Io {
            operation: "write daemon response".to_owned(),
            source,
        })
}

fn worktree_record_to_json(record: &WorktreeRecord) -> String {
    json::Object::new()
        .number("id", record.id as u64)
        .number("repository_id", record.repository_id as u64)
        .string("path", &record.path)
        .optional_string("branch", record.branch.as_deref())
        .optional_string("head", record.head.as_deref())
        .string("git_state", &record.git_state)
        .string("lifecycle_state", &record.lifecycle_state)
        .signed_number("first_seen_at", record.first_seen_at)
        .optional_signed_number("created_at", record.created_at)
        .signed_number("last_seen_at", record.last_seen_at)
        .optional_signed_number("archived_at", record.archived_at)
        .optional_signed_number("removed_at", record.removed_at)
        .finish()
}

fn session_to_json(session: &SessionRecord) -> String {
    json::Object::new()
        .string("id", &session.id)
        .number("worktree_id", session.worktree_id as u64)
        .string("provider", &session.provider)
        .optional_string(
            "provider_session_id",
            session.provider_session_id.as_deref(),
        )
        .optional_signed_number("pid", session.pid)
        .optional_signed_number("process_started_at", session.process_started_at)
        .optional_string("terminal_metadata", session.terminal_metadata.as_deref())
        .string("state", &session.state)
        .signed_number("created_at", session.created_at)
        .signed_number("last_seen_at", session.last_seen_at)
        .finish()
}

fn lease_to_json(lease: &LeaseRecord) -> String {
    json::Object::new()
        .number("id", lease.id as u64)
        .number("worktree_id", lease.worktree_id as u64)
        .string("session_id", &lease.session_id)
        .signed_number("acquired_at", lease.acquired_at)
        .signed_number("renewed_at", lease.renewed_at)
        .signed_number("expires_at", lease.expires_at)
        .string("state", &lease.state)
        .finish()
}

#[cfg(unix)]
fn prepare_socket_path(path: &Path) -> Result<(), DaemonError> {
    use std::os::unix::ffi::OsStrExt;

    if path.as_os_str().as_bytes().len() >= MAX_UNIX_SOCKET_PATH_BYTES {
        return Err(DaemonError::Protocol(format!(
            "daemon socket path exceeds the safe Unix limit of {MAX_UNIX_SOCKET_PATH_BYTES} bytes: {}",
            path.display()
        )));
    }
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).map_err(|source| DaemonError::Io {
            operation: format!("create daemon socket directory {}", parent.display()),
            source,
        })?;
    }
    if let Ok(metadata) = fs::symlink_metadata(path) {
        use std::os::unix::fs::FileTypeExt;
        if !metadata.file_type().is_socket() {
            return Err(DaemonError::Io {
                operation: format!("bind daemon socket {}", path.display()),
                source: io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "daemon socket path is not a Unix socket",
                ),
            });
        }
        match UnixStream::connect(path) {
            Ok(_) => {
                return Err(DaemonError::Io {
                    operation: format!("bind daemon socket {}", path.display()),
                    source: io::Error::new(io::ErrorKind::AddrInUse, "daemon is already running"),
                });
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
                ) =>
            {
                fs::remove_file(path).map_err(|source| DaemonError::Io {
                    operation: format!("remove stale daemon socket {}", path.display()),
                    source,
                })?;
            }
            Err(_) => {
                return Err(DaemonError::Io {
                    operation: format!("inspect daemon socket {}", path.display()),
                    source: io::Error::new(
                        io::ErrorKind::AddrInUse,
                        "daemon socket is unavailable",
                    ),
                });
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
fn set_socket_permissions(path: &Path) -> Result<(), DaemonError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|source| DaemonError::Io {
        operation: format!("restrict daemon socket {}", path.display()),
        source,
    })
}

#[cfg(unix)]
pub fn clean_stale_socket(path: &Path) -> Result<bool, DaemonError> {
    use std::os::unix::fs::FileTypeExt;

    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(source) => {
            return Err(DaemonError::Io {
                operation: format!("inspect daemon socket {}", path.display()),
                source,
            });
        }
    };
    if !metadata.file_type().is_socket() {
        return Err(DaemonError::Io {
            operation: format!("clean daemon socket {}", path.display()),
            source: io::Error::new(
                io::ErrorKind::AlreadyExists,
                "path exists but is not a Unix socket",
            ),
        });
    }
    match UnixStream::connect(path) {
        Ok(_) => Err(DaemonError::Io {
            operation: format!("clean daemon socket {}", path.display()),
            source: io::Error::new(
                io::ErrorKind::AddrInUse,
                "daemon is running; refusing to remove its socket",
            ),
        }),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
            ) =>
        {
            fs::remove_file(path).map_err(|source| DaemonError::Io {
                operation: format!("remove stale daemon socket {}", path.display()),
                source,
            })?;
            Ok(true)
        }
        Err(source) => Err(DaemonError::Io {
            operation: format!("inspect daemon socket {}", path.display()),
            source,
        }),
    }
}

#[cfg(not(unix))]
pub fn clean_stale_socket(_path: &Path) -> Result<bool, DaemonError> {
    Err(DaemonError::Unsupported)
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_secs()).ok())
        .unwrap_or(0)
}

#[cfg(all(test, unix))]
mod tests {
    use super::{
        DaemonClient, DaemonConfig, DaemonError, DaemonServer, clean_stale_socket, is_retryable,
    };
    use crate::git::GitRepository;
    use crate::json;
    use std::fs;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn handles_idempotent_create_and_session_leases() {
        let directory = temporary_directory();
        let root = directory.join("repository");
        initialize_repository(&root);
        let repository = GitRepository::discover(&root).expect("repository discovered");
        let config = DaemonConfig::new(
            repository,
            directory.join("inventory.sqlite3"),
            directory.join("daemon.sock"),
        );
        let server = DaemonServer::open(config).expect("daemon opened");
        let create_params = json::Object::new()
            .string("branch", "feature")
            .string("path", &directory.join("feature").to_string_lossy())
            .finish();
        let request = |request_id: &str, method: &str, params: &str| {
            json::Object::new()
                .number("schema_version", 1)
                .string("request_id", request_id)
                .string("method", method)
                .raw("params", params.to_owned())
                .finish()
        };
        let first = server
            .handle_line(&request("create-1", "create", &create_params))
            .expect("create should succeed");
        let second = server
            .handle_line(&request("create-1", "create", &create_params))
            .expect("create replay should succeed");
        assert_eq!(first, second);
        let conflicting_params = json::Object::new()
            .string("branch", "other")
            .string("path", &directory.join("other").to_string_lossy())
            .finish();
        let conflict = server
            .handle_line(&request("create-1", "create", &conflicting_params))
            .expect("request conflict should be encoded in the response");
        assert!(conflict.contains("\"ok\":false"));
        assert!(conflict.contains("\"code\":\"conflict\""));

        let default_cleanup = server
            .handle_line(&request("cleanup-default", "cleanup_scan", "{}"))
            .expect("default cleanup scan should succeed");
        assert!(default_cleanup.contains("\"minimum_age_seconds\":86400"));
        assert!(default_cleanup.contains("minimum cleanup age is 86400s"));

        let cleanup_params = json::Object::new()
            .signed_number("minimum_age_seconds", 0)
            .string("base", "main")
            .finish();
        let cleanup = server
            .handle_line(&request("cleanup-1", "cleanup_scan", &cleanup_params))
            .expect("cleanup scan should accept a per-request age");
        assert!(cleanup.contains("\"minimum_age_seconds\":0"));
        assert!(cleanup.contains("\"base\":\"main\""));
        assert!(!cleanup.contains("minimum cleanup age is 86400s"));

        let register_params = json::Object::new()
            .string("session_id", "session-1")
            .string("path", &directory.join("feature").to_string_lossy())
            .string("provider", "terminal")
            .number("lease_ttl_seconds", 60)
            .finish();
        let session = server
            .handle_line(&request(
                "session-1-register",
                "register_session",
                &register_params,
            ))
            .expect("session should register");
        assert!(session.contains("\"lease\":{"));
        let heartbeat_params = json::Object::new()
            .string("session_id", "session-1")
            .number("lease_ttl_seconds", 60)
            .finish();
        let heartbeat = server
            .handle_line(&request(
                "session-1-heartbeat",
                "heartbeat_session",
                &heartbeat_params,
            ))
            .expect("heartbeat should succeed");
        assert!(heartbeat.contains("\"state\":\"active\""));
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn retries_only_transport_and_protocol_errors() {
        assert!(is_retryable(&DaemonError::Protocol("malformed".to_owned())));
        assert!(is_retryable(&DaemonError::Io {
            operation: "read".to_owned(),
            source: io::Error::new(io::ErrorKind::UnexpectedEof, "closed"),
        }));
        assert!(!is_retryable(&DaemonError::Remote {
            code: "conflict".to_owned(),
            message: "already exists".to_owned(),
        }));
        assert!(is_retryable(&DaemonError::Remote {
            code: "inventory_replaced".to_owned(),
            message: "database changed".to_owned(),
        }));
    }

    #[test]
    fn client_retries_transport_failures_but_not_remote_errors() {
        let client = DaemonClient::new("unused.sock").with_retries(3);
        let remote_response = super::error_response(
            Some("request-1"),
            "conflict",
            "request already completed with different parameters",
        );
        let mut remote_attempts = 0;
        let remote_error = client
            .request_with_sender("request-1", "create", "{}", |_, _| {
                remote_attempts += 1;
                Ok(remote_response.clone())
            })
            .expect_err("application errors should be returned");
        assert_eq!(remote_attempts, 1);
        assert!(matches!(remote_error, DaemonError::Remote { .. }));

        let success_response = super::success_response("request-2", "{}");
        let client = DaemonClient::new("unused.sock").with_retries(1);
        let mut transport_attempts = 0;
        let result = client
            .request_with_sender("request-2", "create", "{}", |_, _| {
                transport_attempts += 1;
                if transport_attempts == 1 {
                    Err(DaemonError::Io {
                        operation: "read".to_owned(),
                        source: io::Error::new(io::ErrorKind::UnexpectedEof, "closed"),
                    })
                } else {
                    Ok(success_response.clone())
                }
            })
            .expect("transport retry should succeed");
        assert_eq!(transport_attempts, 2);
        assert_eq!(result, success_response);
    }

    #[test]
    fn cleans_missing_socket_and_rejects_non_socket_paths() {
        let directory = temporary_directory();
        let socket = directory.join("daemon.sock");
        assert!(!clean_stale_socket(&socket).expect("missing socket is cleanable"));

        fs::write(&socket, "not a socket").expect("fixture file should be written");
        let error = clean_stale_socket(&socket).expect_err("regular files must not be removed");
        assert!(error.to_string().contains("not a Unix socket"));

        let _ = fs::remove_dir_all(directory);
    }

    fn temporary_directory() -> PathBuf {
        let base = std::env::temp_dir();
        for attempt in 0..100 {
            let timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock should be after epoch")
                .as_nanos();
            let path = base.join(format!(
                "worktree-manager-daemon-test-{}-{timestamp}-{attempt}",
                std::process::id()
            ));
            if fs::create_dir(&path).is_ok() {
                return path;
            }
        }
        panic!("could not allocate temporary directory");
    }

    fn initialize_repository(root: &Path) {
        fs::create_dir(root).expect("repository directory should be created");
        run_git(root, &["init", "-q", "-b", "main"]);
        run_git(root, &["config", "user.name", "Daemon Test"]);
        run_git(root, &["config", "user.email", "daemon@example.test"]);
        fs::write(root.join("README.md"), "daemon\n").expect("fixture file should be written");
        run_git(root, &["add", "README.md"]);
        run_git(root, &["commit", "-q", "-m", "initial"]);
    }

    fn run_git(cwd: &Path, args: &[&str]) {
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
