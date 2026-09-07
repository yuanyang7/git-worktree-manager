use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    Modified,
    Added,
    Deleted,
    Renamed,
    Copied,
    Untracked,
    TypeChanged,
    Unmerged,
    Other,
}

impl ChangeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Modified => "modified",
            Self::Added => "added",
            Self::Deleted => "deleted",
            Self::Renamed => "renamed",
            Self::Copied => "copied",
            Self::Untracked => "untracked",
            Self::TypeChanged => "type-changed",
            Self::Unmerged => "unmerged",
            Self::Other => "other",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeEntry {
    pub path: String,
    pub source_path: Option<String>,
    pub index_status: char,
    pub worktree_status: char,
    pub kind: ChangeKind,
    pub conflicted: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChangeSummary {
    pub staged: u32,
    pub unstaged: u32,
    pub untracked: u32,
    pub conflicted: u32,
    pub ignored: u32,
    pub entries: Vec<ChangeEntry>,
}

impl ChangeSummary {
    pub fn dirty(&self) -> bool {
        self.staged > 0 || self.unstaged > 0 || self.untracked > 0 || self.conflicted > 0
    }

    pub fn dirty_files(&self) -> u32 {
        self.entries.len() as u32
    }

    pub fn state(&self) -> &'static str {
        if self.conflicted > 0 {
            "conflicted"
        } else if self.dirty() {
            "dirty"
        } else {
            "clean"
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UpstreamStatus {
    pub name: Option<String>,
    pub ahead: Option<u32>,
    pub behind: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MergeStatus {
    pub integration_branch: Option<String>,
    pub remote_integration_branch: Option<String>,
    pub unique_commits: Option<u32>,
    pub merged_locally: Option<bool>,
    pub merged_remotely: Option<bool>,
    pub classification: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommitSummary {
    pub id: String,
    pub author: Option<String>,
    pub committed_at: Option<String>,
    pub subject: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiskUsage {
    pub worktree_bytes: u64,
    pub git_common_bytes: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TimeObservation {
    pub filesystem_modified_unix_seconds: Option<i64>,
    pub filesystem_modified_provenance: &'static str,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorktreeStatusData {
    pub changes: ChangeSummary,
    pub upstream: UpstreamStatus,
    pub merge: MergeStatus,
    pub last_commit: Option<CommitSummary>,
    pub disk_usage: DiskUsage,
    pub time: TimeObservation,
}

impl fmt::Display for ChangeKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}
