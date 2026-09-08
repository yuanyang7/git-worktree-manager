# Worktree Manager

Worktree Manager is a local tool for safely creating, inspecting, assigning, and cleaning up Git worktrees used by parallel coding agents.

The current checkout provides a command-line client, reusable Rust service, SQLite inventory, and local daemon. A terminal UI and agent/provider integrations are planned on top of this core.

The tool discovers worktrees through Git’s porcelain interface, records repository/worktree facts in a local SQLite inventory, and provides collision-safe create, lock, unlock, cleanup-scan, and non-force remove operations. See the [implementation plan](docs/implementation-plan.md) for the remaining TUI and agent work.

## Requirements and installation

You need Git and Rust/Cargo. The inventory links against the system SQLite library; macOS provides it, while Linux installations may need their distribution’s SQLite development package.

Install `wtm` from a checkout:

```sh
cd /path/to/git-worktree-manager
cargo install --path .
wtm --help
```

Cargo installs the executable in its binary directory, usually `~/.cargo/bin`; make sure that directory is on your `PATH`. Update an existing installation with `cargo install --path . --force`, or remove it with `cargo uninstall worktree-manager`.

For a development build instead of a global installation:

```sh
cargo build --release
./target/release/wtm --help
```

During development, use `cargo run --` in place of `./target/release/wtm`.

## Usage

Run read-only commands from a repository or pass `--repo PATH` explicitly:

```sh
./target/release/wtm list
./target/release/wtm list --repo /path/to/repository --json
./target/release/wtm status /path/to/worktree --base main
```

Create and protect a worktree:

```sh
./target/release/wtm create feature/auth \
  --repo /path/to/repository \
  --path /path/to/feature-auth \
  --base main \
  --idempotency-key feature-auth-001

./target/release/wtm lock /path/to/feature-auth --reason "agent is using this"
./target/release/wtm unlock /path/to/feature-auth
```

Inspect cleanup candidates and remove a worktree:

```sh
./target/release/wtm cleanup scan --repo /path/to/repository --json
./target/release/wtm cleanup scan --repo /path/to/repository --base dev --json
./target/release/wtm remove /path/to/feature-auth --repo /path/to/repository
./target/release/wtm remove /path/to/feature-auth \
  --repo /path/to/repository \
  --delete-branch \
  --minimum-age-seconds 0
```

`--json` output starts with `schema_version: 1`, so scripts can depend on an explicit format version. Lifecycle commands use `.git/worktree-manager.sqlite3` by default. Pass `--db PATH` to place the inventory elsewhere.

The human-readable list includes `MODIFIED`, an approximate UTC date derived from the worktree directory’s filesystem metadata. JSON output retains the raw Unix timestamp and its provenance under `time`.

Creation accepts `--idempotency-key KEY`; repeated requests with the same key reuse the recorded worktree rather than creating another one. Cleanup accepts `--base REF` to evaluate merge safety against a different integration ref, such as `dev`. Cleanup and removal require a 24-hour minimum age by default; use `--minimum-age-seconds N` when a repository-specific policy calls for a different threshold. Removal is conservative: dirty, conflicted, locked, leased, in-use, unmerged, unavailable, or ambiguous worktrees are blocked, and branch deletion is a separate explicit `--delete-branch` action.

## Daemon and crash recovery

Mutating CLI commands automatically start or reuse the local daemon and route their database/Git writes through it. To run one explicitly:

```sh
./target/release/wtm daemon \
  --repo /path/to/repository \
  --db /path/to/repository/.git/worktree-manager.sqlite3 \
  --socket /tmp/wtm.sock
```

When using a custom socket, pass it to mutating commands too:

```sh
./target/release/wtm create feature/auth \
  --repo /path/to/repository \
  --socket /tmp/wtm.sock
```

To remove only a stale socket, use:

```sh
./target/release/wtm daemon clean --repo /path/to/repository
```

The command refuses to remove a socket that accepts connections and never deletes or resets the SQLite inventory. Pass `--socket PATH` when the daemon uses a custom socket. Automatic startup waits up to 120 seconds for the readiness handshake, reports daemon startup and connection errors, and removes a socket left by a failed child; running `wtm daemon` directly is useful for diagnosis.

The daemon serves versioned JSON-lines requests over a Unix socket. Its request journal replays completed responses and retries interrupted requests by request ID. Startup reconciliation refreshes Git before accepting work, and sessions can register, heartbeat, acquire renewable exclusive leases, and release them. The session/lease backend is implemented for future adapters; user-facing `wtm agent` commands are not available yet. Use a short socket path on systems with strict Unix socket path limits.

If a daemon crashes after Git has removed a worktree, retry the same command so the request journal can reconcile and replay it. If the worktree path no longer lets Git discover the repository, add `--repo PATH` to `wtm remove`. Multiple daemon processes sharing one inventory serialize their request journal through database-scoped locks. Mutations also verify the inventory identity and recover from replacement of the database file.

## Implemented and remaining

Implemented:

- Worktree discovery, status inspection, JSON output, and merge/dirty-state reporting.
- Collision-safe create, Git lock/unlock, cleanup assessment, and conservative removal.
- SQLite inventory, lifecycle events, reservations, sessions, and leases.
- Daemon IPC, centralized mutation writes, idempotent request journaling, retries, and crash recovery.

Next planned slice:

- TUI and service read model with filtering, details, cleanup preview, confirmations, and live refresh.
- Provider-neutral session adapters for launching, discovering, attaching to, messaging, and stopping sessions.
- Initial generic-terminal, `tmux`, and Codex adapters.
- User-facing `wtm tui`, `wtm agent ...`, and `wtm doctor` commands.

Later work includes archive/export-before-delete, filesystem watching, policy configuration, scheduled cleanup, desktop GUI support, remote worktrees, team-shared leases, and PR/CI integrations. See the [implementation plan](docs/implementation-plan.md) for the full roadmap.

## Intended outcomes

- Avoid branch/worktree collisions between concurrent agents.
- Show worktree Git state, merge state, ownership, and activity.
- Reconnect to the agent or terminal associated with a worktree.
- Reclaim disk space without deleting uncommitted or unmerged work.
