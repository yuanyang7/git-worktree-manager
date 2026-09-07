use crate::json::{self, Object};
use crate::model::{
    ChangeEntry, ChangeKind, ChangeSummary, CommitSummary, DiskUsage, MergeStatus, TimeObservation,
    UpstreamStatus, WorktreeStatusData,
};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug)]
pub enum GitError {
    Io {
        operation: String,
        source: io::Error,
    },
    Command {
        args: String,
        code: Option<i32>,
        stderr: String,
    },
    InvalidOutput(String),
    NotRepository(PathBuf),
}

impl fmt::Display for GitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { operation, source } => write!(formatter, "{operation}: {source}"),
            Self::Command { args, code, stderr } => {
                let status = code
                    .map(|code| format!("exit code {code}"))
                    .unwrap_or_else(|| "terminated without an exit code".to_owned());
                if stderr.is_empty() {
                    write!(formatter, "git {args} ({status})")
                } else {
                    write!(formatter, "git {args} ({status}): {stderr}")
                }
            }
            Self::InvalidOutput(message) => write!(formatter, "invalid Git output: {message}"),
            Self::NotRepository(path) => {
                write!(formatter, "not a Git worktree: {}", path.display())
            }
        }
    }
}

impl std::error::Error for GitError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitWorktree {
    pub path: PathBuf,
    pub head: Option<String>,
    pub branch: Option<String>,
    pub detached: bool,
    pub bare: bool,
    pub locked: bool,
    pub lock_reason: Option<String>,
    pub prunable: bool,
    pub prune_reason: Option<String>,
}

impl GitWorktree {
    pub fn state(&self) -> &'static str {
        if self.prunable {
            "prunable"
        } else if self.bare {
            "bare"
        } else if self.locked {
            "locked"
        } else if self.detached {
            "detached"
        } else {
            "available"
        }
    }

    fn to_json(&self) -> String {
        Object::new()
            .string("path", &self.path.to_string_lossy())
            .optional_string("branch", self.branch.as_deref())
            .optional_string("head", self.head.as_deref())
            .bool("detached", self.detached)
            .bool("bare", self.bare)
            .bool("locked", self.locked)
            .optional_string("lock_reason", self.lock_reason.as_deref())
            .bool("prunable", self.prunable)
            .optional_string("prune_reason", self.prune_reason.as_deref())
            .string("git_state", self.state())
            .finish()
    }
}

#[derive(Debug, Clone)]
pub struct GitWorktreeStatus {
    pub worktree: GitWorktree,
    pub data: WorktreeStatusData,
    pub observation_error: Option<String>,
}

impl GitWorktreeStatus {
    pub fn state(&self) -> &'static str {
        if self.worktree.prunable || !self.worktree.path.exists() {
            "unavailable"
        } else if self.observation_error.is_some() {
            "unknown"
        } else if self.data.changes.conflicted > 0 {
            "conflicted"
        } else if self.data.changes.dirty() {
            "dirty"
        } else if self.data.merge.merged_locally == Some(true) {
            "merged-locally"
        } else if self.data.merge.merged_locally == Some(false) {
            "unmerged"
        } else {
            "clean"
        }
    }

    pub fn to_json(&self) -> String {
        let changes = Object::new()
            .number("staged", self.data.changes.staged as u64)
            .number("unstaged", self.data.changes.unstaged as u64)
            .number("untracked", self.data.changes.untracked as u64)
            .number("conflicted", self.data.changes.conflicted as u64)
            .number("ignored", self.data.changes.ignored as u64)
            .number("dirty_files", self.data.changes.dirty_files() as u64)
            .bool("dirty", self.data.changes.dirty())
            .string("state", self.data.changes.state())
            .raw(
                "entries",
                json::array(self.data.changes.entries.iter().map(change_entry_to_json)),
            )
            .finish();

        let upstream = Object::new()
            .optional_string("name", self.data.upstream.name.as_deref())
            .optional_number("ahead", self.data.upstream.ahead.map(u64::from))
            .optional_number("behind", self.data.upstream.behind.map(u64::from))
            .finish();

        let merge = Object::new()
            .optional_string(
                "integration_branch",
                self.data.merge.integration_branch.as_deref(),
            )
            .optional_string(
                "remote_integration_branch",
                self.data.merge.remote_integration_branch.as_deref(),
            )
            .optional_number(
                "unique_commits",
                self.data.merge.unique_commits.map(u64::from),
            )
            .optional_bool("merged_locally", self.data.merge.merged_locally)
            .optional_bool("merged_remotely", self.data.merge.merged_remotely)
            .string("classification", &self.data.merge.classification)
            .finish();

        let last_commit = self
            .data
            .last_commit
            .as_ref()
            .map(commit_to_json)
            .unwrap_or_else(|| "null".to_owned());

        let disk_usage = Object::new()
            .number("worktree_bytes", self.data.disk_usage.worktree_bytes)
            .number("git_common_bytes", self.data.disk_usage.git_common_bytes)
            .number(
                "total_bytes",
                self.data
                    .disk_usage
                    .worktree_bytes
                    .saturating_add(self.data.disk_usage.git_common_bytes),
            )
            .finish();

        let time = Object::new()
            .optional_signed_number(
                "filesystem_modified_unix_seconds",
                self.data.time.filesystem_modified_unix_seconds,
            )
            .string(
                "filesystem_modified_provenance",
                self.data.time.filesystem_modified_provenance,
            )
            .finish();

        Object::new()
            .raw("worktree", self.worktree.to_json())
            .string("state", self.state())
            .raw("changes", changes)
            .raw("upstream", upstream)
            .raw("merge", merge)
            .raw("last_commit", last_commit)
            .raw("disk_usage", disk_usage)
            .raw("time", time)
            .optional_string("observation_error", self.observation_error.as_deref())
            .finish()
    }
}

#[derive(Debug, Clone)]
pub struct GitRepository {
    pub root: PathBuf,
    pub common_git_dir: PathBuf,
    pub display_name: String,
    pub integration_branch: Option<String>,
    pub remote_integration_branch: Option<String>,
}

impl GitRepository {
    pub fn discover(path: impl AsRef<Path>) -> Result<Self, GitError> {
        let input = path.as_ref();
        let canonical_input = canonicalize(input).map_err(|error| GitError::Io {
            operation: format!("resolve repository path {}", input.display()),
            source: error,
        })?;

        let root_output = require_stdout(
            &canonical_input,
            vec![arg("rev-parse"), arg("--show-toplevel")],
        )?;
        let root = root_output.trim();
        if root.is_empty() {
            return Err(GitError::NotRepository(canonical_input));
        }
        let root = canonicalize(Path::new(root)).map_err(|error| GitError::Io {
            operation: format!("resolve Git worktree root {root}"),
            source: error,
        })?;

        let common_git_dir_output =
            require_stdout(&root, vec![arg("rev-parse"), arg("--git-common-dir")])?;
        let common_git_dir_path = Path::new(common_git_dir_output.trim());
        let common_git_dir_path = if common_git_dir_path.is_absolute() {
            common_git_dir_path.to_path_buf()
        } else {
            root.join(common_git_dir_path)
        };
        let common_git_dir = canonicalize(&common_git_dir_path).map_err(|error| GitError::Io {
            operation: format!(
                "resolve common Git directory {}",
                common_git_dir_path.display()
            ),
            source: error,
        })?;

        let remote_head = find_remote_integration_branch(&root)?;
        let integration_branch = find_integration_branch(&root, remote_head.as_deref())?;
        let remote_integration_branch = match remote_head {
            Some(remote) => Some(remote),
            None => find_remote_branch_for_integration(&root, integration_branch.as_deref())?,
        };
        let display_name = repository_display_name(&root, &common_git_dir);

        Ok(Self {
            root,
            common_git_dir,
            display_name,
            integration_branch,
            remote_integration_branch,
        })
    }

    pub fn list_worktrees(&self) -> Result<Vec<GitWorktree>, GitError> {
        let output = require_output(
            &self.root,
            vec![arg("worktree"), arg("list"), arg("--porcelain"), arg("-z")],
        )?;
        parse_worktree_list_porcelain(&output.stdout).map(|worktrees| {
            worktrees
                .into_iter()
                .map(|mut worktree| {
                    if let Ok(canonical_path) = fs::canonicalize(&worktree.path) {
                        worktree.path = canonical_path;
                    }
                    worktree
                })
                .collect()
        })
    }

    pub fn inspect_all(
        &self,
        base_override: Option<&str>,
    ) -> Result<Vec<GitWorktreeStatus>, GitError> {
        self.list_worktrees().map(|worktrees| {
            worktrees
                .iter()
                .map(|worktree| self.inspect_worktree(worktree, base_override))
                .collect()
        })
    }

    pub fn inspect_worktree(
        &self,
        worktree: &GitWorktree,
        base_override: Option<&str>,
    ) -> GitWorktreeStatus {
        if worktree.bare || worktree.prunable || !worktree.path.is_dir() {
            return GitWorktreeStatus {
                worktree: worktree.clone(),
                data: WorktreeStatusData {
                    merge: merge_status_unavailable(
                        base_override.or(self.integration_branch.as_deref()),
                    ),
                    time: filesystem_time(&worktree.path),
                    ..WorktreeStatusData::default()
                },
                observation_error: Some(if worktree.prunable {
                    "Git reports this worktree as prunable".to_owned()
                } else if worktree.bare {
                    "bare repository; working-tree status is not applicable".to_owned()
                } else {
                    "worktree path is missing or not a directory".to_owned()
                }),
            };
        }

        let mut errors = Vec::new();
        let changes = match read_change_summary(&worktree.path) {
            Ok(value) => value,
            Err(error) => {
                errors.push(format!("changes: {error}"));
                ChangeSummary::default()
            }
        };
        let upstream = match read_upstream_status(&worktree.path) {
            Ok(value) => value,
            Err(error) => {
                errors.push(format!("upstream: {error}"));
                UpstreamStatus::default()
            }
        };
        let last_commit = match read_last_commit(&worktree.path) {
            Ok(value) => value,
            Err(error) => {
                errors.push(format!("last commit: {error}"));
                None
            }
        };
        let remote_integration_branch = match base_override {
            Some(base) => match find_remote_branch_for_base(&worktree.path, base) {
                Ok(remote) => remote,
                Err(error) => {
                    errors.push(format!("remote integration branch: {error}"));
                    None
                }
            },
            None => self.remote_integration_branch.clone(),
        };
        let merge = match read_merge_status(
            &worktree.path,
            base_override.or(self.integration_branch.as_deref()),
            remote_integration_branch.as_deref(),
            worktree.head.as_deref(),
        ) {
            Ok(value) => value,
            Err(error) => {
                errors.push(format!("merge: {error}"));
                merge_status_unavailable(base_override.or(self.integration_branch.as_deref()))
            }
        };
        let disk_usage = match read_disk_usage(&worktree.path, &self.common_git_dir) {
            Ok(value) => value,
            Err(error) => {
                errors.push(format!("disk usage: {error}"));
                DiskUsage::default()
            }
        };

        GitWorktreeStatus {
            worktree: worktree.clone(),
            data: WorktreeStatusData {
                changes,
                upstream,
                merge,
                last_commit,
                disk_usage,
                time: filesystem_time(&worktree.path),
            },
            observation_error: (!errors.is_empty()).then(|| errors.join("; ")),
        }
    }

    pub fn to_json(&self, worktrees: &[GitWorktreeStatus]) -> String {
        Object::new()
            .number("schema_version", 1)
            .raw(
                "repository",
                Object::new()
                    .string("name", &self.display_name)
                    .string("root", &self.root.to_string_lossy())
                    .string("common_git_dir", &self.common_git_dir.to_string_lossy())
                    .optional_string("integration_branch", self.integration_branch.as_deref())
                    .optional_string(
                        "remote_integration_branch",
                        self.remote_integration_branch.as_deref(),
                    )
                    .finish(),
            )
            .raw(
                "worktrees",
                json::array(worktrees.iter().map(GitWorktreeStatus::to_json)),
            )
            .finish()
    }
}

pub fn parse_worktree_list_porcelain(input: &[u8]) -> Result<Vec<GitWorktree>, GitError> {
    let mut records: Vec<Vec<Vec<u8>>> = Vec::new();
    let mut current = Vec::new();

    for field in input.split(|byte| *byte == 0) {
        if field.is_empty() {
            if !current.is_empty() {
                records.push(std::mem::take(&mut current));
            }
        } else {
            current.push(field.to_vec());
        }
    }
    if !current.is_empty() {
        records.push(current);
    }

    records.into_iter().map(parse_worktree_record).collect()
}

fn parse_worktree_record(fields: Vec<Vec<u8>>) -> Result<GitWorktree, GitError> {
    let mut path = None;
    let mut head = None;
    let mut branch = None;
    let mut detached = false;
    let mut bare = false;
    let mut locked = false;
    let mut lock_reason = None;
    let mut prunable = false;
    let mut prune_reason = None;

    for field in fields {
        let (key, value) = match field.iter().position(|byte| *byte == b' ') {
            Some(index) => (&field[..index], Some(&field[index + 1..])),
            None => (field.as_slice(), None),
        };
        match key {
            b"worktree" => {
                path = Some(path_string(value.ok_or_else(|| {
                    GitError::InvalidOutput("worktree record has no path".to_owned())
                })?)?)
            }
            b"HEAD" => head = Some(value_string(value)?),
            b"branch" => {
                let branch_ref = value_string(value)?;
                branch = Some(
                    branch_ref
                        .strip_prefix("refs/heads/")
                        .unwrap_or(&branch_ref)
                        .to_owned(),
                );
            }
            b"detached" => detached = true,
            b"bare" => bare = true,
            b"locked" => {
                locked = true;
                lock_reason = value.map(|value| String::from_utf8_lossy(value).into_owned());
            }
            b"prunable" => {
                prunable = true;
                prune_reason = value.map(|value| String::from_utf8_lossy(value).into_owned());
            }
            other => {
                return Err(GitError::InvalidOutput(format!(
                    "unknown worktree record field {:?}",
                    String::from_utf8_lossy(other)
                )));
            }
        }
    }

    let path =
        path.ok_or_else(|| GitError::InvalidOutput("worktree record has no path".to_owned()))?;
    Ok(GitWorktree {
        path: PathBuf::from(path),
        head,
        branch,
        detached,
        bare,
        locked,
        lock_reason,
        prunable,
        prune_reason,
    })
}

fn read_change_summary(path: &Path) -> Result<ChangeSummary, GitError> {
    let output = require_output(
        path,
        vec![
            arg("status"),
            arg("--porcelain=v1"),
            arg("-z"),
            arg("--untracked-files=all"),
            arg("--ignored"),
        ],
    )?;
    parse_status_porcelain(&output.stdout)
}

pub fn parse_status_porcelain(input: &[u8]) -> Result<ChangeSummary, GitError> {
    let fields: Vec<&[u8]> = input
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty())
        .collect();
    let mut summary = ChangeSummary::default();
    let mut index = 0;

    while index < fields.len() {
        let field = fields[index];
        if field.len() < 3 || field[2] != b' ' {
            return Err(GitError::InvalidOutput(format!(
                "invalid status entry {:?}",
                String::from_utf8_lossy(field)
            )));
        }

        let index_status = field[0] as char;
        let worktree_status = field[1] as char;
        let path = value_string(Some(&field[3..]))?;
        let source_path =
            if matches!(index_status, 'R' | 'C') || matches!(worktree_status, 'R' | 'C') {
                index += 1;
                if index >= fields.len() {
                    return Err(GitError::InvalidOutput(format!(
                        "rename/copy entry for {path:?} has no source path"
                    )));
                }
                Some(value_string(Some(fields[index]))?)
            } else {
                None
            };

        if index_status == '!' && worktree_status == '!' {
            summary.ignored = summary.ignored.saturating_add(1);
            index += 1;
            continue;
        }

        let conflicted = is_conflicted(index_status, worktree_status);
        let kind = change_kind(index_status, worktree_status, conflicted);
        if index_status == '?' && worktree_status == '?' {
            summary.untracked = summary.untracked.saturating_add(1);
        } else {
            if status_counts_as_staged(index_status) {
                summary.staged = summary.staged.saturating_add(1);
            }
            if status_counts_as_unstaged(worktree_status) {
                summary.unstaged = summary.unstaged.saturating_add(1);
            }
        }
        if conflicted {
            summary.conflicted = summary.conflicted.saturating_add(1);
        }
        summary.entries.push(ChangeEntry {
            path,
            source_path,
            index_status,
            worktree_status,
            kind,
            conflicted,
        });
        index += 1;
    }

    Ok(summary)
}

fn is_conflicted(index_status: char, worktree_status: char) -> bool {
    matches!(index_status, 'U')
        || matches!(worktree_status, 'U')
        || (index_status == 'A' && worktree_status == 'A')
        || (index_status == 'D' && worktree_status == 'D')
}

fn change_kind(index_status: char, worktree_status: char, conflicted: bool) -> ChangeKind {
    if conflicted {
        return ChangeKind::Unmerged;
    }
    let status = if index_status != ' ' && index_status != '?' && index_status != '!' {
        index_status
    } else {
        worktree_status
    };
    match status {
        'M' => ChangeKind::Modified,
        'A' => ChangeKind::Added,
        'D' => ChangeKind::Deleted,
        'R' => ChangeKind::Renamed,
        'C' => ChangeKind::Copied,
        'T' => ChangeKind::TypeChanged,
        '?' => ChangeKind::Untracked,
        _ => ChangeKind::Other,
    }
}

fn status_counts_as_staged(status: char) -> bool {
    status != ' ' && status != '?' && status != '!'
}

fn status_counts_as_unstaged(status: char) -> bool {
    status != ' ' && status != '?' && status != '!'
}

fn read_upstream_status(path: &Path) -> Result<UpstreamStatus, GitError> {
    let upstream_output = git_output(
        path,
        vec![
            arg("rev-parse"),
            arg("--abbrev-ref"),
            arg("--symbolic-full-name"),
            arg("@{upstream}"),
        ],
    )?;
    if !upstream_output.status.success() {
        return Ok(UpstreamStatus::default());
    }
    let name = stdout_string(&upstream_output)?.trim().to_owned();
    if name.is_empty() {
        return Ok(UpstreamStatus::default());
    }

    let counts = require_stdout(
        path,
        vec![
            arg("rev-list"),
            arg("--left-right"),
            arg("--count"),
            arg("HEAD...@{upstream}"),
        ],
    )?;
    let mut values = counts.split_whitespace();
    let ahead = parse_count(values.next(), "ahead")?;
    let behind = parse_count(values.next(), "behind")?;
    Ok(UpstreamStatus {
        name: Some(name),
        ahead: Some(ahead),
        behind: Some(behind),
    })
}

fn read_last_commit(path: &Path) -> Result<Option<CommitSummary>, GitError> {
    let output = git_output(
        path,
        vec![
            arg("show"),
            arg("-s"),
            arg("--format=%H%x00%an%x00%cI%x00%s"),
            arg("HEAD"),
        ],
    )?;
    if !output.status.success() {
        return Ok(None);
    }
    let fields: Vec<&[u8]> = output.stdout.split(|byte| *byte == 0).collect();
    if fields.len() < 4 {
        return Err(GitError::InvalidOutput(
            "last commit output has fewer than four fields".to_owned(),
        ));
    }
    let id = value_string(Some(fields[0]))?.trim().to_owned();
    if id.is_empty() {
        return Ok(None);
    }
    Ok(Some(CommitSummary {
        id,
        author: Some(value_string(Some(fields[1]))?.trim().to_owned()),
        committed_at: Some(value_string(Some(fields[2]))?.trim().to_owned()),
        subject: Some(value_string(Some(fields[3]))?.trim_end().to_owned()),
    }))
}

fn read_merge_status(
    path: &Path,
    integration_branch: Option<&str>,
    remote_integration_branch: Option<&str>,
    head: Option<&str>,
) -> Result<MergeStatus, GitError> {
    let Some(integration_branch) = integration_branch else {
        return Ok(merge_status_unavailable(None));
    };
    if head.is_none() {
        return Ok(merge_status_unavailable(Some(integration_branch)));
    }

    let unique_commits = rev_list_count(path, &format!("{integration_branch}..HEAD"))?;
    let merged_locally = is_ancestor(path, "HEAD", integration_branch)?;
    let merged_remotely = match remote_integration_branch {
        Some(remote) => is_ancestor(path, "HEAD", remote)?,
        None => None,
    };
    let classification = match merged_locally {
        Some(true) => "merged-locally",
        Some(false) => "unmerged-commits",
        None => "unknown",
    };
    Ok(MergeStatus {
        integration_branch: Some(integration_branch.to_owned()),
        remote_integration_branch: remote_integration_branch.map(str::to_owned),
        unique_commits,
        merged_locally,
        merged_remotely,
        classification: classification.to_owned(),
    })
}

fn merge_status_unavailable(integration_branch: Option<&str>) -> MergeStatus {
    MergeStatus {
        integration_branch: integration_branch.map(str::to_owned),
        classification: "unknown".to_owned(),
        ..MergeStatus::default()
    }
}

fn rev_list_count(path: &Path, range: &str) -> Result<Option<u32>, GitError> {
    let output = git_output(
        path,
        vec![arg("rev-list"), arg("--count"), OsString::from(range)],
    )?;
    if !output.status.success() {
        return Ok(None);
    }
    Ok(Some(parse_count(
        stdout_string(&output)?.split_whitespace().next(),
        "unique commits",
    )?))
}

fn is_ancestor(path: &Path, ancestor: &str, descendant: &str) -> Result<Option<bool>, GitError> {
    let output = git_output(
        path,
        vec![
            arg("merge-base"),
            arg("--is-ancestor"),
            OsString::from(ancestor),
            OsString::from(descendant),
        ],
    )?;
    match output.status.code() {
        Some(0) => Ok(Some(true)),
        Some(1) => Ok(Some(false)),
        _ => Ok(None),
    }
}

fn read_disk_usage(path: &Path, common_git_dir: &Path) -> Result<DiskUsage, GitError> {
    let worktree_bytes = directory_size(path, Some(&path.join(".git")))?;
    let git_common_bytes = directory_size(common_git_dir, None)?;
    Ok(DiskUsage {
        worktree_bytes,
        git_common_bytes,
    })
}

fn directory_size(path: &Path, skip_path: Option<&Path>) -> Result<u64, GitError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| GitError::Io {
        operation: format!("inspect {}", path.display()),
        source: error,
    })?;
    if metadata.is_file() {
        return Ok(metadata.len());
    }
    if !metadata.is_dir() {
        return Ok(0);
    }

    let mut total = 0_u64;
    let entries = fs::read_dir(path).map_err(|error| GitError::Io {
        operation: format!("read {}", path.display()),
        source: error,
    })?;
    for entry in entries {
        let entry = entry.map_err(|error| GitError::Io {
            operation: format!("read directory entry in {}", path.display()),
            source: error,
        })?;
        let entry_path = entry.path();
        if skip_path.is_some_and(|skip| entry_path == skip) {
            continue;
        }
        total = total.saturating_add(directory_size(&entry_path, skip_path)?);
    }
    Ok(total)
}

fn filesystem_time(path: &Path) -> TimeObservation {
    let modified = fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(system_time_to_unix_seconds);
    TimeObservation {
        filesystem_modified_unix_seconds: modified,
        filesystem_modified_provenance: "filesystem metadata; approximate",
    }
}

fn system_time_to_unix_seconds(time: SystemTime) -> Option<i64> {
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => i64::try_from(duration.as_secs()).ok(),
        Err(error) => i64::try_from(error.duration().as_secs())
            .ok()
            .map(|seconds| -seconds),
    }
}

fn find_integration_branch(
    root: &Path,
    remote_integration_branch: Option<&str>,
) -> Result<Option<String>, GitError> {
    if let Some(remote) = remote_integration_branch {
        let candidate = remote.strip_prefix("origin/").unwrap_or(remote);
        if has_ref(root, &format!("refs/heads/{candidate}"))? {
            return Ok(Some(candidate.to_owned()));
        }
    }
    for candidate in ["main", "master"] {
        if has_ref(root, &format!("refs/heads/{candidate}"))? {
            return Ok(Some(candidate.to_owned()));
        }
    }
    Ok(None)
}

fn find_remote_integration_branch(root: &Path) -> Result<Option<String>, GitError> {
    let output = git_output(
        root,
        vec![
            arg("symbolic-ref"),
            arg("--quiet"),
            arg("--short"),
            arg("refs/remotes/origin/HEAD"),
        ],
    )?;
    if !output.status.success() {
        return Ok(None);
    }
    let value = stdout_string(&output)?.trim().to_owned();
    Ok((!value.is_empty()).then_some(value))
}

fn find_remote_branch_for_integration(
    root: &Path,
    integration_branch: Option<&str>,
) -> Result<Option<String>, GitError> {
    let Some(integration_branch) = integration_branch else {
        return Ok(None);
    };
    find_remote_branch_for_base(root, integration_branch)
}

fn find_remote_branch_for_base(root: &Path, base: &str) -> Result<Option<String>, GitError> {
    let candidates = if let Some(remote_ref) = base.strip_prefix("refs/remotes/") {
        vec![remote_ref.to_owned()]
    } else {
        let local_ref = base.strip_prefix("refs/heads/").unwrap_or(base);
        let origin_candidate = format!("origin/{local_ref}");
        if base.starts_with("origin/") || base.starts_with("upstream/") {
            vec![base.to_owned(), origin_candidate]
        } else {
            vec![origin_candidate, base.to_owned()]
        }
    };

    for candidate in candidates {
        if has_ref(root, &format!("refs/remotes/{candidate}"))? {
            return Ok(Some(candidate));
        }
    }
    Ok(None)
}

fn has_ref(root: &Path, reference: &str) -> Result<bool, GitError> {
    let output = git_output(
        root,
        vec![
            arg("show-ref"),
            arg("--verify"),
            arg("--quiet"),
            arg(reference),
        ],
    )?;
    Ok(output.status.success())
}

fn repository_display_name(root: &Path, common_git_dir: &Path) -> String {
    if common_git_dir.file_name() == Some(OsStr::new(".git")) {
        common_git_dir
            .parent()
            .and_then(Path::file_name)
            .or_else(|| root.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| root.to_string_lossy().into_owned())
    } else {
        common_git_dir
            .file_name()
            .or_else(|| root.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| root.to_string_lossy().into_owned())
    }
}

fn parse_count(value: Option<&str>, label: &str) -> Result<u32, GitError> {
    value
        .ok_or_else(|| GitError::InvalidOutput(format!("missing {label} count")))?
        .parse::<u32>()
        .map_err(|error| GitError::InvalidOutput(format!("invalid {label} count: {error}")))
}

fn change_entry_to_json(entry: &ChangeEntry) -> String {
    Object::new()
        .string("path", &entry.path)
        .optional_string("source_path", entry.source_path.as_deref())
        .string("index_status", &entry.index_status.to_string())
        .string("worktree_status", &entry.worktree_status.to_string())
        .string("kind", entry.kind.as_str())
        .bool("conflicted", entry.conflicted)
        .finish()
}

fn commit_to_json(commit: &CommitSummary) -> String {
    Object::new()
        .string("id", &commit.id)
        .optional_string("author", commit.author.as_deref())
        .optional_string("committed_at", commit.committed_at.as_deref())
        .optional_string("subject", commit.subject.as_deref())
        .finish()
}

fn path_string(value: &[u8]) -> Result<String, GitError> {
    String::from_utf8(value.to_vec())
        .map_err(|_| GitError::InvalidOutput("Git returned a non-UTF-8 worktree path".to_owned()))
}

fn value_string(value: Option<&[u8]>) -> Result<String, GitError> {
    let value = value.ok_or_else(|| GitError::InvalidOutput("missing field value".to_owned()))?;
    String::from_utf8(value.to_vec())
        .map_err(|_| GitError::InvalidOutput("Git returned non-UTF-8 output".to_owned()))
}

fn canonicalize(path: &Path) -> io::Result<PathBuf> {
    fs::canonicalize(path)
}

fn arg(value: &str) -> OsString {
    OsString::from(value)
}

fn require_stdout(cwd: &Path, args: Vec<OsString>) -> Result<String, GitError> {
    let output = require_output(cwd, args)?;
    stdout_string(&output)
}

fn require_output(cwd: &Path, args: Vec<OsString>) -> Result<Output, GitError> {
    let output = git_output(cwd, args.clone())?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(command_error(&args, &output))
    }
}

fn git_output(cwd: &Path, args: Vec<OsString>) -> Result<Output, GitError> {
    let operation = format!("run Git in {}", cwd.display());
    Command::new("git")
        .args(&args)
        .current_dir(cwd)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_PAGER", "cat")
        .env("LC_ALL", "C")
        .output()
        .map_err(|source| GitError::Io { operation, source })
}

fn stdout_string(output: &Output) -> Result<String, GitError> {
    String::from_utf8(output.stdout.clone())
        .map_err(|_| GitError::InvalidOutput("Git returned non-UTF-8 stdout".to_owned()))
}

fn command_error(args: &[OsString], output: &Output) -> GitError {
    GitError::Command {
        args: args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" "),
        code: output.status.code(),
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_status_porcelain, parse_worktree_list_porcelain};
    use crate::model::ChangeKind;

    #[test]
    fn parses_worktree_records_with_optional_fields() {
        let input = b"worktree /tmp/project one\0HEAD abc123\0branch refs/heads/main\0\0worktree /tmp/project two\0HEAD def456\0detached\0locked reason\0\0";
        let records = parse_worktree_list_porcelain(input).expect("records should parse");
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].path.to_string_lossy(), "/tmp/project one");
        assert_eq!(records[0].branch.as_deref(), Some("main"));
        assert!(records[1].detached);
        assert!(records[1].locked);
        assert_eq!(records[1].lock_reason.as_deref(), Some("reason"));
    }

    #[test]
    fn counts_status_entries_and_renames() {
        let input = b"M  staged.txt\0 M changed.txt\0?? new.txt\0UU conflict.txt\0R  new-name.txt\0old-name.txt\0!! ignored.log\0";
        let summary = parse_status_porcelain(input).expect("status should parse");
        assert_eq!(summary.staged, 3);
        assert_eq!(summary.unstaged, 2);
        assert_eq!(summary.untracked, 1);
        assert_eq!(summary.conflicted, 1);
        assert_eq!(summary.ignored, 1);
        assert_eq!(summary.entries.len(), 5);
        assert_eq!(summary.entries[4].kind, ChangeKind::Renamed);
        assert_eq!(
            summary.entries[4].source_path.as_deref(),
            Some("old-name.txt")
        );
    }
}
