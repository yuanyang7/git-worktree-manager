use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};
use worktree_manager::git::GitRepository;

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new() -> Self {
        let base = std::env::temp_dir();
        for attempt in 0..100 {
            let timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock should be after Unix epoch")
                .as_nanos();
            let path = base.join(format!(
                "worktree-manager-test-{}-{timestamp}-{attempt}",
                std::process::id()
            ));
            if fs::create_dir(&path).is_ok() {
                return Self { path };
            }
        }
        panic!("could not allocate a temporary directory");
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn git(cwd: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("git should be installed for fixture tests");
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
}

fn make_repository() -> (TempDir, PathBuf, PathBuf) {
    let fixture = TempDir::new();
    let root = fixture.path().join("repository");
    let feature = fixture.path().join("feature");
    fs::create_dir(&root).expect("repository directory should be created");
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["config", "user.name", "Fixture"]);
    git(&root, &["config", "user.email", "fixture@example.test"]);
    fs::write(root.join("README.md"), "fixture\n").expect("fixture file should be written");
    git(&root, &["add", "README.md"]);
    git(&root, &["commit", "-q", "-m", "initial"]);
    git(
        &root,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feature",
            feature.to_str().expect("fixture path should be UTF-8"),
        ],
    );
    (fixture, root, feature)
}

#[test]
fn discovers_linked_worktrees_and_reports_dirty_files() {
    let (_fixture, root, feature_path) = make_repository();
    let feature = fs::canonicalize(feature_path).expect("feature path should be canonicalizable");
    git(&root, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
    fs::write(feature.join("notes.txt"), "untracked content\n")
        .expect("dirty file should be written");

    let repository = GitRepository::discover(&root).expect("repository should be discovered");
    assert_eq!(repository.integration_branch.as_deref(), Some("main"));
    assert_eq!(
        repository.remote_integration_branch.as_deref(),
        Some("origin/main")
    );
    let statuses = repository
        .inspect_all(None)
        .expect("worktrees should be listed");
    assert_eq!(statuses.len(), 2);

    let feature_status = statuses
        .iter()
        .find(|status| status.worktree.path == feature)
        .expect("feature worktree should be present");
    assert_eq!(feature_status.data.changes.untracked, 1);
    assert!(feature_status.data.changes.dirty());
    assert_eq!(feature_status.data.merge.unique_commits, Some(0));
    assert_eq!(feature_status.data.merge.merged_locally, Some(true));
    assert_eq!(feature_status.data.merge.merged_remotely, Some(true));
    assert!(feature_status.data.disk_usage.worktree_bytes > 0);
    assert!(feature_status.observation_error.is_none());

    let main_path = fs::canonicalize(root).expect("main path should be canonicalizable");
    let main_status = statuses
        .iter()
        .find(|status| status.worktree.path == main_path)
        .expect("main worktree should be present");
    assert_eq!(main_status.state(), "clean");
    assert_eq!(main_status.data.merge.classification, "merged-locally");
}

#[test]
fn reports_branch_commits_that_are_not_integration_commits() {
    let (_fixture, root, feature_path) = make_repository();
    let feature = fs::canonicalize(feature_path).expect("feature path should be canonicalizable");
    fs::write(feature.join("feature.txt"), "feature\n").expect("feature file should be written");
    git(&feature, &["add", "feature.txt"]);
    git(&feature, &["commit", "-q", "-m", "feature work"]);

    let repository = GitRepository::discover(&root).expect("repository should be discovered");
    let statuses = repository
        .inspect_all(None)
        .expect("worktrees should be listed");
    let feature_status = statuses
        .iter()
        .find(|status| status.worktree.path == feature)
        .expect("feature worktree should be present");
    assert_eq!(feature_status.data.merge.unique_commits, Some(1));
    assert_eq!(feature_status.data.merge.merged_locally, Some(false));
    assert_eq!(feature_status.data.merge.classification, "unmerged-commits");
}
