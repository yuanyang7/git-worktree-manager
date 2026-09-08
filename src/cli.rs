use crate::daemon::{DaemonClient, DaemonConfig, DaemonError, DaemonServer, inventory_identity};
use crate::git::{GitError, GitRepository, GitWorktreeStatus};
use crate::json;
use crate::service::{CreateRequest, DEFAULT_CLEANUP_MINIMUM_AGE_SECONDS, ServiceError};
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Debug)]
pub enum CliError {
    Usage(String),
    Git(GitError),
    Daemon(DaemonError),
    Service(ServiceError),
    Io(String),
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage(message) => write!(formatter, "{message}"),
            Self::Git(error) => error.fmt(formatter),
            Self::Daemon(error) => error.fmt(formatter),
            Self::Service(error) => error.fmt(formatter),
            Self::Io(message) => write!(formatter, "{message}"),
        }
    }
}

impl std::error::Error for CliError {}

impl From<GitError> for CliError {
    fn from(error: GitError) -> Self {
        Self::Git(error)
    }
}

impl From<DaemonError> for CliError {
    fn from(error: DaemonError) -> Self {
        Self::Daemon(error)
    }
}

impl From<ServiceError> for CliError {
    fn from(error: ServiceError) -> Self {
        Self::Service(error)
    }
}

#[derive(Debug, Default)]
struct OutputOptions {
    json: bool,
    base: Option<String>,
}

#[derive(Debug)]
struct CreateCliOptions {
    repo_path: PathBuf,
    db_path: Option<PathBuf>,
    socket_path: Option<PathBuf>,
    request: CreateRequest,
    json: bool,
}

#[derive(Debug, Default)]
struct PathMutationOptions {
    path: Option<PathBuf>,
    repo_path: Option<PathBuf>,
    db_path: Option<PathBuf>,
    socket_path: Option<PathBuf>,
    minimum_age_seconds: Option<i64>,
    reason: Option<String>,
    delete_branch: bool,
    json: bool,
}

#[derive(Debug, Default)]
struct RepositoryMutationOptions {
    repo_path: PathBuf,
    db_path: Option<PathBuf>,
    socket_path: Option<PathBuf>,
    minimum_age_seconds: Option<i64>,
    json: bool,
}

#[derive(Debug, Default)]
struct DaemonOptions {
    repo_path: PathBuf,
    db_path: Option<PathBuf>,
    socket_path: Option<PathBuf>,
    minimum_age_seconds: Option<i64>,
}

pub fn run<I, T>(args: I) -> Result<(), CliError>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString>,
{
    let args: Vec<String> = args
        .into_iter()
        .map(|arg| arg.into().to_string_lossy().into_owned())
        .collect();
    let Some(command) = args.first().map(String::as_str) else {
        print_help();
        return Ok(());
    };

    match command {
        "list" => run_list(&args[1..]),
        "status" => run_status(&args[1..]),
        "create" => run_create(&args[1..]),
        "lock" => run_lock(&args[1..], true),
        "unlock" => run_lock(&args[1..], false),
        "remove" => run_remove(&args[1..]),
        "cleanup" => run_cleanup(&args[1..]),
        "daemon" => run_daemon(&args[1..]),
        "help" | "--help" | "-h" => {
            print_help();
            Ok(())
        }
        "--version" | "-V" | "version" => {
            println!("wtm {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        other => Err(CliError::Usage(format!(
            "unknown command {other:?}\n\n{}",
            usage()
        ))),
    }
}

fn run_daemon(args: &[String]) -> Result<(), CliError> {
    let options = parse_daemon_options(args)?;
    let repository = GitRepository::discover(&options.repo_path)?;
    let db_path = options
        .db_path
        .unwrap_or_else(|| default_inventory_path(&repository));
    let socket_path = options
        .socket_path
        .unwrap_or_else(|| default_socket_path(&repository));
    let mut config = DaemonConfig::new(repository, db_path, socket_path);
    if let Some(minimum_age_seconds) = options.minimum_age_seconds {
        config.minimum_cleanup_age_seconds = minimum_age_seconds;
    }
    DaemonServer::open(config)?.serve_forever()?;
    Ok(())
}

fn run_list(args: &[String]) -> Result<(), CliError> {
    let (repo_path, options) = parse_repository_options(args, "list")?;
    let repository = GitRepository::discover(repo_path)?;
    let statuses = repository.inspect_all(options.base.as_deref())?;
    if options.json {
        println!("{}", repository.to_json(&statuses));
    } else {
        print_list(&repository, &statuses);
    }
    Ok(())
}

fn run_status(args: &[String]) -> Result<(), CliError> {
    let (worktree_path, options) = parse_status_options(args)?;
    let repository = GitRepository::discover(&worktree_path)?;
    let worktrees = repository.list_worktrees()?;
    let requested = find_worktree(&worktrees, &worktree_path).ok_or_else(|| {
        CliError::Usage(format!(
            "path is not a linked worktree in this repository: {}",
            worktree_path.display()
        ))
    })?;
    let status = repository.inspect_worktree(requested, options.base.as_deref());
    if options.json {
        println!("{}", repository.to_json(&[status]));
    } else {
        print_status(&repository, &status);
    }
    Ok(())
}

fn run_create(args: &[String]) -> Result<(), CliError> {
    let options = parse_create_options(args)?;
    let repository = GitRepository::discover(&options.repo_path)?;
    let db_path = options
        .db_path
        .unwrap_or_else(|| default_inventory_path(&repository));
    let path_text = options
        .request
        .path
        .as_ref()
        .map(|path| path.to_string_lossy().into_owned());
    let params = json::Object::new()
        .string("branch", &options.request.branch)
        .optional_string("base", options.request.base.as_deref())
        .optional_string("path", path_text.as_deref())
        .optional_string(
            "idempotency_key",
            options.request.idempotency_key.as_deref(),
        )
        .string("actor", &options.request.actor)
        .finish();
    let request_id = options
        .request
        .idempotency_key
        .clone()
        .unwrap_or_else(|| next_request_id("create"));
    let result_json = daemon_request_result(
        &repository,
        &db_path,
        None,
        options.socket_path.as_deref(),
        &request_id,
        "create",
        &params,
    )?;
    print_create_result(&result_json, options.json)?;
    Ok(())
}

fn run_lock(args: &[String], lock: bool) -> Result<(), CliError> {
    let options = parse_path_mutation_options(args, if lock { "lock" } else { "unlock" })?;
    if !lock && options.reason.is_some() {
        return Err(CliError::Usage(format!(
            "unlock does not accept --reason\n\n{}",
            usage()
        )));
    }
    let path = options.path.clone().ok_or_else(|| {
        CliError::Usage(format!(
            "{} requires a worktree path\n\n{}",
            if lock { "lock" } else { "unlock" },
            usage()
        ))
    })?;
    let repository = GitRepository::discover(&path)?;
    let db_path = options
        .db_path
        .unwrap_or_else(|| default_inventory_path(&repository));
    let params = json::Object::new()
        .string("path", &path.to_string_lossy())
        .optional_string("reason", options.reason.as_deref())
        .string("actor", "cli")
        .finish();
    let operation = if lock { "lock" } else { "unlock" };
    let request_id = next_request_id(operation);
    let result_json = daemon_request_result(
        &repository,
        &db_path,
        None,
        options.socket_path.as_deref(),
        &request_id,
        operation,
        &params,
    )?;
    print_lock_result(&result_json, lock, options.json)?;
    Ok(())
}

fn run_remove(args: &[String]) -> Result<(), CliError> {
    let options = parse_path_mutation_options(args, "remove")?;
    let path = options.path.clone().ok_or_else(|| {
        CliError::Usage(format!("remove requires a worktree path\n\n{}", usage()))
    })?;
    let repository = match GitRepository::discover(&path) {
        Ok(repository) => repository,
        Err(error) => {
            let Some(repo_path) = options.repo_path.as_ref() else {
                return Err(error.into());
            };
            GitRepository::discover(repo_path)?
        }
    };
    let db_path = options
        .db_path
        .unwrap_or_else(|| default_inventory_path(&repository));
    let minimum_age_seconds = options
        .minimum_age_seconds
        .unwrap_or(DEFAULT_CLEANUP_MINIMUM_AGE_SECONDS);
    let params = json::Object::new()
        .string("path", &path.to_string_lossy())
        .bool("delete_branch", options.delete_branch)
        .signed_number("minimum_age_seconds", minimum_age_seconds)
        .string("actor", "cli")
        .finish();
    let request_id = next_request_id("remove");
    let result_json = daemon_request_result(
        &repository,
        &db_path,
        Some(minimum_age_seconds),
        options.socket_path.as_deref(),
        &request_id,
        "remove",
        &params,
    )?;
    print_remove_result(&result_json, options.json)?;
    Ok(())
}

fn run_cleanup(args: &[String]) -> Result<(), CliError> {
    let Some(subcommand) = args.first().map(String::as_str) else {
        return Err(CliError::Usage(format!(
            "cleanup requires a subcommand\n\n{}",
            usage()
        )));
    };
    if subcommand != "scan" {
        return Err(CliError::Usage(format!(
            "unknown cleanup subcommand {subcommand:?}\n\n{}",
            usage()
        )));
    }
    let options = parse_repository_mutation_options(&args[1..], "cleanup scan")?;
    let repository = GitRepository::discover(&options.repo_path)?;
    let db_path = options
        .db_path
        .unwrap_or_else(|| default_inventory_path(&repository));
    let minimum_age_seconds = options
        .minimum_age_seconds
        .unwrap_or(DEFAULT_CLEANUP_MINIMUM_AGE_SECONDS);
    let params = json::Object::new()
        .signed_number("minimum_age_seconds", minimum_age_seconds)
        .string("actor", "cli")
        .finish();
    let request_id = next_request_id("cleanup");
    let result_json = daemon_request_result(
        &repository,
        &db_path,
        Some(minimum_age_seconds),
        options.socket_path.as_deref(),
        &request_id,
        "cleanup_scan",
        &params,
    )?;
    print_cleanup_result(&result_json, options.json)?;
    Ok(())
}

fn parse_repository_options(
    args: &[String],
    command: &str,
) -> Result<(PathBuf, OutputOptions), CliError> {
    let mut options = OutputOptions::default();
    let mut repo_path = PathBuf::from(".");
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--json" => options.json = true,
            "--base" => {
                index += 1;
                options.base = Some(required_value(args, index, "--base")?);
            }
            "--repo" => {
                index += 1;
                repo_path = PathBuf::from(required_value(args, index, "--repo")?);
            }
            value if value.starts_with('-') => {
                return Err(CliError::Usage(format!(
                    "unknown option {value:?} for {command}\n\n{}",
                    usage()
                )));
            }
            value => {
                if repo_path != Path::new(".") {
                    return Err(CliError::Usage(format!(
                        "{command} accepts only one repository path\n\n{}",
                        usage()
                    )));
                }
                repo_path = PathBuf::from(value);
            }
        }
        index += 1;
    }
    Ok((repo_path, options))
}

fn parse_status_options(args: &[String]) -> Result<(PathBuf, OutputOptions), CliError> {
    let mut options = OutputOptions::default();
    let mut path = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--json" => options.json = true,
            "--base" => {
                index += 1;
                options.base = Some(required_value(args, index, "--base")?);
            }
            value if value.starts_with('-') => {
                return Err(CliError::Usage(format!(
                    "unknown option {value:?} for status\n\n{}",
                    usage()
                )));
            }
            value => {
                if path.is_some() {
                    return Err(CliError::Usage(format!(
                        "status accepts one worktree path\n\n{}",
                        usage()
                    )));
                }
                path = Some(PathBuf::from(value));
            }
        }
        index += 1;
    }
    let path = path.ok_or_else(|| {
        CliError::Usage(format!("status requires a worktree path\n\n{}", usage()))
    })?;
    Ok((path, options))
}

fn parse_create_options(args: &[String]) -> Result<CreateCliOptions, CliError> {
    let mut repo_path = PathBuf::from(".");
    let mut db_path = None;
    let mut socket_path = None;
    let mut branch = None;
    let mut base = None;
    let mut path = None;
    let mut idempotency_key = None;
    let mut json = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--repo" => {
                index += 1;
                repo_path = PathBuf::from(required_value(args, index, "--repo")?);
            }
            "--db" => {
                index += 1;
                db_path = Some(PathBuf::from(required_value(args, index, "--db")?));
            }
            "--socket" => {
                index += 1;
                socket_path = Some(PathBuf::from(required_value(args, index, "--socket")?));
            }
            "--base" => {
                index += 1;
                base = Some(required_value(args, index, "--base")?);
            }
            "--path" => {
                index += 1;
                path = Some(PathBuf::from(required_value(args, index, "--path")?));
            }
            "--idempotency-key" => {
                index += 1;
                idempotency_key = Some(required_value(args, index, "--idempotency-key")?);
            }
            "--json" => json = true,
            value if value.starts_with('-') => {
                return Err(CliError::Usage(format!(
                    "unknown option {value:?} for create\n\n{}",
                    usage()
                )));
            }
            value => {
                if branch.is_some() {
                    return Err(CliError::Usage(format!(
                        "create accepts one branch name\n\n{}",
                        usage()
                    )));
                }
                branch = Some(value.to_owned());
            }
        }
        index += 1;
    }
    let branch = branch
        .ok_or_else(|| CliError::Usage(format!("create requires a branch name\n\n{}", usage())))?;
    let mut request = CreateRequest::new(branch);
    request.base = base;
    request.path = path;
    request.idempotency_key = idempotency_key;
    Ok(CreateCliOptions {
        repo_path,
        db_path,
        socket_path,
        request,
        json,
    })
}

fn parse_path_mutation_options(
    args: &[String],
    command: &str,
) -> Result<PathMutationOptions, CliError> {
    let mut options = PathMutationOptions::default();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--repo" if command == "remove" => {
                index += 1;
                options.repo_path = Some(PathBuf::from(required_value(args, index, "--repo")?));
            }
            "--db" => {
                index += 1;
                options.db_path = Some(PathBuf::from(required_value(args, index, "--db")?));
            }
            "--socket" => {
                index += 1;
                options.socket_path = Some(PathBuf::from(required_value(args, index, "--socket")?));
            }
            "--minimum-age-seconds" if command == "remove" => {
                index += 1;
                options.minimum_age_seconds =
                    Some(required_integer(args, index, "--minimum-age-seconds")?);
            }
            "--reason" if command == "lock" => {
                index += 1;
                options.reason = Some(required_value(args, index, "--reason")?);
            }
            "--delete-branch" if command == "remove" => options.delete_branch = true,
            "--json" => options.json = true,
            value if value.starts_with('-') => {
                return Err(CliError::Usage(format!(
                    "unknown option {value:?} for {command}\n\n{}",
                    usage()
                )));
            }
            value => {
                if options.path.is_some() {
                    return Err(CliError::Usage(format!(
                        "{command} accepts one worktree path\n\n{}",
                        usage()
                    )));
                }
                options.path = Some(PathBuf::from(value));
            }
        }
        index += 1;
    }
    Ok(options)
}

fn parse_repository_mutation_options(
    args: &[String],
    command: &str,
) -> Result<RepositoryMutationOptions, CliError> {
    let mut options = RepositoryMutationOptions {
        repo_path: PathBuf::from("."),
        ..RepositoryMutationOptions::default()
    };
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--repo" => {
                index += 1;
                options.repo_path = PathBuf::from(required_value(args, index, "--repo")?);
            }
            "--db" => {
                index += 1;
                options.db_path = Some(PathBuf::from(required_value(args, index, "--db")?));
            }
            "--socket" => {
                index += 1;
                options.socket_path = Some(PathBuf::from(required_value(args, index, "--socket")?));
            }
            "--minimum-age-seconds" => {
                index += 1;
                options.minimum_age_seconds =
                    Some(required_integer(args, index, "--minimum-age-seconds")?);
            }
            "--json" => options.json = true,
            value if value.starts_with('-') => {
                return Err(CliError::Usage(format!(
                    "unknown option {value:?} for {command}\n\n{}",
                    usage()
                )));
            }
            value => {
                return Err(CliError::Usage(format!(
                    "{command} does not accept positional argument {value:?}\n\n{}",
                    usage()
                )));
            }
        }
        index += 1;
    }
    Ok(options)
}

fn parse_daemon_options(args: &[String]) -> Result<DaemonOptions, CliError> {
    let mut options = DaemonOptions {
        repo_path: PathBuf::from("."),
        ..DaemonOptions::default()
    };
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--repo" => {
                index += 1;
                options.repo_path = PathBuf::from(required_value(args, index, "--repo")?);
            }
            "--db" => {
                index += 1;
                options.db_path = Some(PathBuf::from(required_value(args, index, "--db")?));
            }
            "--socket" => {
                index += 1;
                options.socket_path = Some(PathBuf::from(required_value(args, index, "--socket")?));
            }
            "--minimum-age-seconds" => {
                index += 1;
                options.minimum_age_seconds =
                    Some(required_integer(args, index, "--minimum-age-seconds")?);
            }
            value => {
                return Err(CliError::Usage(format!(
                    "unknown option {value:?} for daemon\n\n{}",
                    usage()
                )));
            }
        }
        index += 1;
    }
    Ok(options)
}

fn default_inventory_path(repository: &GitRepository) -> PathBuf {
    repository.common_git_dir.join("worktree-manager.sqlite3")
}

fn required_integer(args: &[String], index: usize, option: &str) -> Result<i64, CliError> {
    let value = required_value(args, index, option)?;
    value.parse::<i64>().map_err(|error| {
        CliError::Usage(format!(
            "{option} requires an integer value, got {value:?}: {error}\n\n{}",
            usage()
        ))
    })
}

fn required_value(args: &[String], index: usize, option: &str) -> Result<String, CliError> {
    args.get(index)
        .cloned()
        .filter(|value| !value.starts_with('-'))
        .ok_or_else(|| CliError::Usage(format!("{option} requires a value\n\n{}", usage())))
}

fn find_worktree<'a>(
    worktrees: &'a [crate::git::GitWorktree],
    requested: &Path,
) -> Option<&'a crate::git::GitWorktree> {
    let requested = absolute_or_canonical(requested);
    worktrees
        .iter()
        .find(|worktree| absolute_or_canonical(&worktree.path) == requested)
}

fn absolute_or_canonical(path: &Path) -> PathBuf {
    if let Ok(path) = fs::canonicalize(path) {
        return path;
    }
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|current| current.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}

fn print_list(repository: &GitRepository, statuses: &[GitWorktreeStatus]) {
    println!(
        "Repository: {}\nRoot: {}\nIntegration: {}",
        repository.display_name,
        repository.root.display(),
        repository
            .integration_branch
            .as_deref()
            .unwrap_or("unknown")
    );
    println!();
    println!(
        "{:<32} {:<20} {:<16} {:<16} {:<18} {:>10}",
        "WORKTREE", "BRANCH", "STATE", "CHANGES", "MERGE", "SIZE"
    );
    for status in statuses {
        let path = display_worktree_path(&repository.root, &status.worktree.path);
        let branch = status.worktree.branch.as_deref().unwrap_or("(detached)");
        let changes = if status.data.changes.dirty() {
            format!("dirty ({})", status.data.changes.entries.len())
        } else {
            status.data.changes.state().to_owned()
        };
        let merge = if status.observation_error.is_some() {
            "unknown".to_owned()
        } else {
            status.data.merge.classification.clone()
        };
        let size = format_bytes(
            status
                .data
                .disk_usage
                .worktree_bytes
                .saturating_add(status.data.disk_usage.git_common_bytes),
        );
        println!(
            "{:<32} {:<20} {:<16} {:<16} {:<18} {:>10}",
            truncate(&path, 32),
            truncate(branch, 20),
            status.state(),
            truncate(&changes, 16),
            truncate(&merge, 18),
            size
        );
        if let Some(error) = &status.observation_error {
            println!("  observation error: {error}");
        }
    }
}

fn print_status(repository: &GitRepository, status: &GitWorktreeStatus) {
    let worktree = &status.worktree;
    println!("Worktree: {}", worktree.path.display());
    println!("Repository: {}", repository.display_name);
    println!(
        "Branch: {}",
        worktree.branch.as_deref().unwrap_or("(detached)")
    );
    println!("HEAD: {}", worktree.head.as_deref().unwrap_or("unknown"));
    println!("Git state: {}", worktree.state());
    println!("Observed state: {}", status.state());
    println!(
        "Changes: {} staged, {} unstaged, {} untracked, {} conflicted, {} ignored",
        status.data.changes.staged,
        status.data.changes.unstaged,
        status.data.changes.untracked,
        status.data.changes.conflicted,
        status.data.changes.ignored
    );
    println!(
        "Upstream: {}",
        status.data.upstream.name.as_deref().unwrap_or("none")
    );
    if let (Some(ahead), Some(behind)) = (status.data.upstream.ahead, status.data.upstream.behind) {
        println!("Ahead/behind: {ahead}/{behind}");
    }
    println!("Merge: {}", status.data.merge.classification);
    if let Some(unique) = status.data.merge.unique_commits {
        println!("Unique commits: {unique}");
    }
    if let Some(commit) = &status.data.last_commit {
        println!(
            "Last commit: {} {}",
            &commit.id[..commit.id.len().min(12)],
            commit.subject.as_deref().unwrap_or("")
        );
        if let Some(committed_at) = &commit.committed_at {
            println!("Committed at: {committed_at}");
        }
    }
    println!(
        "Disk: {} worktree-local + {} common Git",
        format_bytes(status.data.disk_usage.worktree_bytes),
        format_bytes(status.data.disk_usage.git_common_bytes)
    );
    if let Some(modified) = status.data.time.filesystem_modified_unix_seconds {
        println!("Filesystem modified (approx.): {modified}");
    }
    if !status.data.changes.entries.is_empty() {
        println!();
        println!("Changed files:");
        for entry in &status.data.changes.entries {
            let source = entry
                .source_path
                .as_deref()
                .map(|source| format!(" <- {source}"))
                .unwrap_or_default();
            println!(
                "  {}{} [{}{}]",
                entry.kind,
                if source.is_empty() { "" } else { " " },
                entry.index_status,
                entry.worktree_status
            );
            println!("    {}{}", entry.path, source);
        }
    }
    if let Some(error) = &status.observation_error {
        println!();
        println!("Observation error: {error}");
    }
}

fn display_worktree_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .ok()
        .filter(|relative| !relative.as_os_str().is_empty())
        .map(|relative| relative.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

fn truncate(value: &str, width: usize) -> String {
    let mut characters = value.chars();
    let truncated: String = characters.by_ref().take(width).collect();
    if characters.next().is_some() {
        let mut output: String = value.chars().take(width.saturating_sub(1)).collect();
        output.push('…');
        output
    } else {
        truncated
    }
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", bytes, UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn daemon_client_if_available(
    repository: &GitRepository,
    db_path: &Path,
    minimum_age_seconds: Option<i64>,
    requested_socket_path: Option<&Path>,
) -> Result<DaemonClient, CliError> {
    daemon_client(
        repository,
        db_path,
        minimum_age_seconds,
        requested_socket_path,
    )
}

fn daemon_request_result(
    repository: &GitRepository,
    db_path: &Path,
    minimum_age_seconds: Option<i64>,
    requested_socket_path: Option<&Path>,
    request_id: &str,
    method: &str,
    params_json: &str,
) -> Result<String, CliError> {
    let client = daemon_client_if_available(
        repository,
        db_path,
        minimum_age_seconds,
        requested_socket_path,
    )?;
    match client.request_result(request_id, method, params_json) {
        Ok(result) => Ok(result),
        Err(error) if daemon_error_is_retryable(&error) => {
            let restarted_client = daemon_client(
                repository,
                db_path,
                minimum_age_seconds,
                requested_socket_path,
            )?;
            restarted_client
                .request_result(request_id, method, params_json)
                .map_err(CliError::Daemon)
        }
        Err(error) => Err(CliError::Daemon(error)),
    }
}

fn daemon_error_is_retryable(error: &DaemonError) -> bool {
    match error {
        DaemonError::Io { .. } | DaemonError::Protocol(_) => true,
        DaemonError::Remote { code, .. } => code == "inventory_replaced",
        _ => false,
    }
}

fn daemon_client(
    repository: &GitRepository,
    db_path: &Path,
    minimum_age_seconds: Option<i64>,
    requested_socket_path: Option<&Path>,
) -> Result<DaemonClient, CliError> {
    let socket_path = requested_socket_path
        .map(PathBuf::from)
        .unwrap_or_else(|| default_socket_path(repository));
    let client = DaemonClient::new(&socket_path).with_retries(0);
    if daemon_is_ready(&client, repository, db_path, minimum_age_seconds) {
        return Ok(client);
    }

    let executable = std::env::current_exe().map_err(|source| {
        CliError::Io(format!(
            "resolve wtm executable for daemon startup: {source}"
        ))
    })?;
    let mut child = Command::new(executable)
        .arg("daemon")
        .arg("--repo")
        .arg(&repository.root)
        .arg("--db")
        .arg(db_path)
        .arg("--socket")
        .arg(&socket_path)
        .arg("--minimum-age-seconds")
        .arg(
            minimum_age_seconds
                .unwrap_or(DEFAULT_CLEANUP_MINIMUM_AGE_SECONDS)
                .to_string(),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|source| CliError::Io(format!("start local wtm daemon: {source}")))?;

    for _ in 0..100 {
        if daemon_is_ready(&client, repository, db_path, minimum_age_seconds) {
            return Ok(client);
        }
        if let Some(status) = child
            .try_wait()
            .map_err(|source| CliError::Io(format!("check local wtm daemon startup: {source}")))?
        {
            for _ in 0..10 {
                if daemon_is_ready(&client, repository, db_path, minimum_age_seconds) {
                    return Ok(client);
                }
                thread::sleep(Duration::from_millis(50));
            }
            return Err(CliError::Io(format!(
                "local wtm daemon exited before opening {} ({status})",
                socket_path.display()
            )));
        }
        thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
    Err(CliError::Io(format!(
        "timed out waiting for local wtm daemon at {}",
        socket_path.display()
    )))
}

fn default_socket_path(repository: &GitRepository) -> PathBuf {
    let conventional = repository.common_git_dir.join("worktree-manager.sock");
    if !socket_path_too_long(&conventional) {
        return conventional;
    }

    let hash = socket_path_hash(&repository.common_git_dir);
    let filename = format!("wtm-{hash:016x}.sock");
    let temporary = std::env::temp_dir().join(&filename);
    if !socket_path_too_long(&temporary) {
        return temporary;
    }
    PathBuf::from("/tmp").join(filename)
}

#[cfg(unix)]
fn socket_path_too_long(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;

    path.as_os_str().as_bytes().len() >= 100
}

#[cfg(not(unix))]
fn socket_path_too_long(_path: &Path) -> bool {
    false
}

fn socket_path_hash(path: &Path) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in path.to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn daemon_is_ready(
    client: &DaemonClient,
    repository: &GitRepository,
    db_path: &Path,
    _minimum_age_seconds: Option<i64>,
) -> bool {
    let Ok(response) = client.request(&next_request_id("ping"), "ping", "{}") else {
        return false;
    };
    let Ok(response) = json::parse(&response) else {
        return false;
    };
    let Some(result) = response.object_field("result") else {
        return false;
    };
    let Ok(root) = result.required_string("repository_root") else {
        return false;
    };
    let Ok(inventory_path) = result.required_string("inventory_path") else {
        return false;
    };
    let Ok(actual_identity) = result.required_string("inventory_identity") else {
        return false;
    };
    let Ok(expected_identity) = inventory_identity(db_path) else {
        return false;
    };
    let expected_db = fs::canonicalize(db_path).unwrap_or_else(|_| db_path.to_path_buf());
    let actual_db =
        fs::canonicalize(inventory_path).unwrap_or_else(|_| PathBuf::from(inventory_path));
    root == repository.root.to_string_lossy()
        && actual_db == expected_db
        && actual_identity == expected_identity
}

fn next_request_id(operation: &str) -> String {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("cli-{operation}-{}-{timestamp}", std::process::id())
}

fn daemon_result(result_json: &str) -> Result<json::Value, CliError> {
    json::parse(result_json).map_err(|error| CliError::Io(error.to_string()))
}

fn print_create_result(result_json: &str, json_output: bool) -> Result<(), CliError> {
    if json_output {
        println!("{result_json}");
        return Ok(());
    }
    let result = daemon_result(result_json)?;
    let path = result.required_string("path").map_err(CliError::Io)?;
    let branch = result
        .optional_string("branch")
        .map_err(CliError::Io)?
        .unwrap_or("(detached)");
    let already_existed = result
        .optional_bool("already_existed")
        .map_err(CliError::Io)?
        .unwrap_or(false);
    println!(
        "{} worktree {} at {}",
        if already_existed { "Reused" } else { "Created" },
        branch,
        path
    );
    Ok(())
}

fn print_lock_result(result_json: &str, lock: bool, json_output: bool) -> Result<(), CliError> {
    if json_output {
        println!("{result_json}");
        return Ok(());
    }
    let result = daemon_result(result_json)?;
    let path = result.required_string("path").map_err(CliError::Io)?;
    let already = result
        .optional_bool("already_in_requested_state")
        .map_err(CliError::Io)?
        .unwrap_or(false);
    if already {
        println!(
            "{} already {}: {}",
            path,
            if lock { "locked" } else { "unlocked" },
            result
                .optional_string("lock_reason")
                .map_err(CliError::Io)?
                .unwrap_or("")
        );
    } else {
        println!("{} {}", if lock { "Locked" } else { "Unlocked" }, path);
    }
    Ok(())
}

fn print_remove_result(result_json: &str, json_output: bool) -> Result<(), CliError> {
    if json_output {
        println!("{result_json}");
        return Ok(());
    }
    let result = daemon_result(result_json)?;
    let path = result.required_string("path").map_err(CliError::Io)?;
    println!("Removed worktree {path}");
    if result
        .optional_bool("branch_deleted")
        .map_err(CliError::Io)?
        .unwrap_or(false)
        && let Some(branch) = result.optional_string("branch").map_err(CliError::Io)?
    {
        println!("Deleted branch {branch}");
    }
    Ok(())
}

fn print_cleanup_result(result_json: &str, json_output: bool) -> Result<(), CliError> {
    if json_output {
        println!("{result_json}");
        return Ok(());
    }
    let result = daemon_result(result_json)?;
    let candidates = match result.object_field("candidates") {
        Some(json::Value::Array(candidates)) => candidates,
        _ => {
            return Err(CliError::Io(
                "daemon cleanup response has no candidates array".to_owned(),
            ));
        }
    };
    println!("Cleanup candidates:");
    for candidate in candidates {
        let path = candidate.required_string("path").map_err(CliError::Io)?;
        let branch = candidate
            .optional_string("branch")
            .map_err(CliError::Io)?
            .unwrap_or("(detached)");
        let classification = candidate
            .required_string("classification")
            .map_err(CliError::Io)?;
        let blockers = match candidate.object_field("blockers") {
            Some(json::Value::Array(blockers)) => blockers
                .iter()
                .map(|blocker| match blocker {
                    json::Value::String(blocker) => Ok(blocker.as_str()),
                    _ => Err("daemon cleanup blocker is not a string"),
                })
                .collect::<Result<Vec<_>, _>>()
                .map_err(|message| CliError::Io(message.to_owned()))?,
            _ => {
                return Err(CliError::Io(
                    "daemon cleanup candidate has no blockers array".to_owned(),
                ));
            }
        };
        let blockers = if blockers.is_empty() {
            String::new()
        } else {
            format!(" — {}", blockers.join(", "))
        };
        println!(
            "  {:<8} {:<24} {:<20}{}",
            classification,
            truncate(branch, 24),
            path,
            blockers
        );
    }
    Ok(())
}

fn usage() -> &'static str {
    "Usage:\n  wtm list [--repo PATH] [--base REF] [--json]\n  wtm status PATH [--base REF] [--json]\n  wtm create BRANCH [--repo PATH] [--base REF] [--path PATH] [--idempotency-key KEY] [--db PATH] [--socket PATH] [--json]\n  wtm lock PATH [--reason TEXT] [--db PATH] [--socket PATH] [--json]\n  wtm unlock PATH [--db PATH] [--socket PATH] [--json]\n  wtm cleanup scan [--repo PATH] [--db PATH] [--socket PATH] [--minimum-age-seconds N] [--json]\n  wtm remove PATH [--repo PATH] [--delete-branch] [--db PATH] [--socket PATH] [--minimum-age-seconds N] [--json]\n  wtm daemon [--repo PATH] [--db PATH] [--socket PATH] [--minimum-age-seconds N]\n  wtm --help\n  wtm --version"
}

fn print_help() {
    println!(
        "Worktree Manager\n\nGit worktree discovery, status inspection, and guarded lifecycle operations.\n\n{}",
        usage()
    );
}

#[cfg(test)]
mod tests {
    use super::format_bytes;

    #[test]
    fn formats_bytes_for_human_output() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(2048), "2.0 KB");
    }
}
