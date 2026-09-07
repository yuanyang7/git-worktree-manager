# Worktree Manager

Worktree Manager is a planned local tool for safely creating, inspecting, assigning, and cleaning up Git worktrees used by parallel coding agents.

The first release is intended to be a terminal UI backed by a reusable core service. A desktop GUI can be added later without duplicating Git or process-management logic.

No product code has been implemented yet. See the [implementation plan](docs/implementation-plan.md).

## Intended outcomes

- Avoid branch/worktree collisions between concurrent agents.
- Show worktree Git state, merge state, ownership, and activity.
- Reconnect to the agent or terminal associated with a worktree.
- Reclaim disk space without deleting uncommitted or unmerged work.

