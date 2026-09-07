# Worktree Manager

Worktree Manager is a local tool for safely creating, inspecting, assigning, and cleaning up Git worktrees used by parallel coding agents.

The first release is intended to be a terminal UI backed by a reusable core service. A desktop GUI can be added later without duplicating Git or process-management logic.

The first read-only implementation slice is now available as the `wtm` command. It discovers worktrees through Git’s porcelain interface and reports changes, upstream divergence, merge ancestry, commit metadata, approximate filesystem time, and separate worktree/common-Git disk usage. See the [implementation plan](docs/implementation-plan.md) for the remaining lifecycle, agent, daemon, and TUI work.

## Quick start

From a Git repository:

```sh
cargo run -- list
cargo run -- list --json
cargo run -- status /path/to/worktree
cargo run -- status /path/to/worktree --base main --json
```

`--json` output starts with `schema_version: 1` so scripts can depend on an explicit format version. The current phase is read-only: it does not create, lock, archive, remove, or mutate worktrees.

## Intended outcomes

- Avoid branch/worktree collisions between concurrent agents.
- Show worktree Git state, merge state, ownership, and activity.
- Reconnect to the agent or terminal associated with a worktree.
- Reclaim disk space without deleting uncommitted or unmerged work.
