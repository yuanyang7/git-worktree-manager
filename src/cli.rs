use crate::git::{GitError, GitRepository, GitWorktreeStatus};
use crate::json;
use crate::service::{
    CreateRequest, DEFAULT_CLEANUP_MINIMUM_AGE_SECONDS, RepositoryService, ServiceError,
};
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub enum CliError {
    Usage(String),
    Git(GitError),
    Service(ServiceError),
    Io(String),
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage(message) => write!(formatter, "{message}"),
            Self::Git(error) => error.fmt(formatter),
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
    request: CreateRequest,
    json: bool,
}

#[derive(Debug, Default)]
struct PathMutationOptions {
    path: Option<PathBuf>,
    db_path: Option<PathBuf>,
    minimum_age_seconds: Option<i64>,
    reason: Option<String>,
    delete_branch: bool,
    json: bool,
}

#[derive(Debug, Default)]
struct RepositoryMutationOptions {
    repo_path: PathBuf,
    db_path: Option<PathBuf>,
    minimum_age_seconds: Option<i64>,
    json: bool,
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
    let service = RepositoryService::open(repository, db_path)?;
    let result = service.create(options.request)?;
    if options.json {
        println!("{}", result.to_json());
    } else {
        println!(
            "{} worktree {} at {}",
            if result.already_existed {
                "Reused"
            } else {
                "Created"
            },
            result.worktree.branch.as_deref().unwrap_or("(detached)"),
            result.worktree.path.display()
        );
    }
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
    let service = RepositoryService::open(repository, db_path)?;
    let result = if lock {
        service.lock(&path, options.reason.as_deref(), "cli")?
    } else {
        service.unlock(&path, "cli")?
    };
    if options.json {
        println!("{}", result.to_json(if lock { "lock" } else { "unlock" }));
    } else if result.already_in_requested_state {
        println!(
            "{} already {}: {}",
            result.worktree.path.display(),
            if lock { "locked" } else { "unlocked" },
            result.worktree.lock_reason.as_deref().unwrap_or("")
        );
    } else {
        println!(
            "{} {}",
            if lock { "Locked" } else { "Unlocked" },
            result.worktree.path.display()
        );
    }
    Ok(())
}

fn run_remove(args: &[String]) -> Result<(), CliError> {
    let options = parse_path_mutation_options(args, "remove")?;
    let path = options.path.clone().ok_or_else(|| {
        CliError::Usage(format!("remove requires a worktree path\n\n{}", usage()))
    })?;
    let repository = GitRepository::discover(&path)?;
    let db_path = options
        .db_path
        .unwrap_or_else(|| default_inventory_path(&repository));
    let service = RepositoryService::open_with_minimum_age(
        repository,
        db_path,
        options
            .minimum_age_seconds
            .unwrap_or(DEFAULT_CLEANUP_MINIMUM_AGE_SECONDS),
    )?;
    let result = service.remove(&path, options.delete_branch, "cli")?;
    if options.json {
        println!("{}", result.to_json());
    } else {
        println!("Removed worktree {}", result.path.display());
        if result.branch_deleted
            && let Some(branch) = result.branch
        {
            println!("Deleted branch {branch}");
        }
    }
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
    let service = RepositoryService::open_with_minimum_age(
        repository,
        db_path,
        options
            .minimum_age_seconds
            .unwrap_or(DEFAULT_CLEANUP_MINIMUM_AGE_SECONDS),
    )?;
    let candidates = service.cleanup_scan()?;
    if options.json {
        println!(
            "{}",
            json::Object::new()
                .number("schema_version", 1)
                .string("operation", "cleanup_scan")
                .raw(
                    "candidates",
                    json::array(candidates.iter().map(|candidate| candidate.to_json())),
                )
                .finish()
        );
    } else {
        println!("Cleanup candidates:");
        for candidate in candidates {
            let branch = candidate.branch.as_deref().unwrap_or("(detached)");
            let blockers = if candidate.blockers.is_empty() {
                String::new()
            } else {
                format!(" — {}", candidate.blockers.join(", "))
            };
            println!(
                "  {:<8} {:<24} {:<20}{}",
                candidate.classification,
                truncate(branch, 24),
                candidate.path.display(),
                blockers
            );
        }
    }
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
            "--db" => {
                index += 1;
                options.db_path = Some(PathBuf::from(required_value(args, index, "--db")?));
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

fn usage() -> &'static str {
    "Usage:\n  wtm list [--repo PATH] [--base REF] [--json]\n  wtm status PATH [--base REF] [--json]\n  wtm create BRANCH [--repo PATH] [--base REF] [--path PATH] [--idempotency-key KEY] [--db PATH] [--json]\n  wtm lock PATH [--reason TEXT] [--db PATH] [--json]\n  wtm unlock PATH [--db PATH] [--json]\n  wtm cleanup scan [--repo PATH] [--db PATH] [--minimum-age-seconds N] [--json]\n  wtm remove PATH [--delete-branch] [--db PATH] [--minimum-age-seconds N] [--json]\n  wtm --help\n  wtm --version"
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
