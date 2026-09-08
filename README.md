# Worktree Manager

Worktree Manager is a local tool for safely creating, inspecting, assigning, and cleaning up Git worktrees used by parallel coding agents.

The first release is intended to be a terminal UI backed by a reusable core service. A desktop GUI can be added later without duplicating Git or process-management logic. The guarded service can run as a local daemon so clients share one serialized SQLite writer.

The read-only inspection core, guarded lifecycle slice, and local daemon are available as the `wtm` command. It discovers worktrees through Git’s porcelain interface, records repository/worktree facts in a local SQLite inventory, and provides collision-safe create, lock, unlock, cleanup-scan, and non-force remove operations. See the [implementation plan](docs/implementation-plan.md) for the remaining agent and TUI work.

## Quick start

From a Git repository:

```sh
cargo run -- list
cargo run -- list --json
cargo run -- status /path/to/worktree
cargo run -- status /path/to/worktree --base main --json
cargo run -- create feature/auth --repo /path/to/repository --path /path/to/feature-auth
cargo run -- cleanup scan --repo /path/to/repository --json
cargo run -- remove /path/to/feature-auth --delete-branch --minimum-age-seconds 0
cargo run -- daemon --repo /path/to/repository --socket /path/to/wtm.sock
```

`--json` output starts with `schema_version: 1` so scripts can depend on an explicit format version. Lifecycle commands use `.git/worktree-manager.sqlite3` by default. Pass `--db PATH` to place the inventory elsewhere.

Creation accepts `--idempotency-key KEY`; repeated requests with the same key reuse the recorded worktree rather than creating another one. Cleanup and removal require a 24-hour minimum age by default; use `--minimum-age-seconds N` when a repository-specific policy calls for a different threshold. Removal is conservative: dirty, conflicted, locked, leased, in-use, unmerged, unavailable, or ambiguous worktrees are blocked, and branch deletion is a separate explicit `--delete-branch` action.

Mutating CLI commands start or reuse the local daemon and route their database/Git writes through it. Use `--socket PATH` on a mutating command when the daemon was started with a custom socket, or run the daemon explicitly first. `wtm daemon` serves versioned JSON-lines requests over a Unix socket. Its request journal replays completed responses and retries interrupted requests by request ID, while startup reconciliation refreshes Git before accepting work. The daemon also exposes session registration, heartbeats, exclusive renewable leases, and session/lease release operations for future agent adapters. Use a short socket path on systems with strict Unix socket path limits.

If a daemon crashes after Git has removed a worktree, retry the same command so the request journal can reconcile and replay it; if the worktree path no longer lets Git discover the repository, add `--repo PATH` to `wtm remove`. Multiple daemon processes sharing one inventory serialize their request journal through a database-scoped lock.

The inventory links against the system SQLite library; macOS provides it, while Linux installations may need their distribution’s SQLite development package.

## Intended outcomes

- Avoid branch/worktree collisions between concurrent agents.
- Show worktree Git state, merge state, ownership, and activity.
- Reconnect to the agent or terminal associated with a worktree.
- Reclaim disk space without deleting uncommitted or unmerged work.
