# Worktree Manager implementation plan

## 1. Product direction

Build a local-first worktree control plane for developers running multiple coding agents against one repository. Start with a terminal UI (TUI), because it is fast to ship, works naturally beside agents, and avoids committing to a desktop framework before the workflows are validated. Keep all behavior behind a reusable core and local daemon so a native or web-based desktop GUI can be added later.

The product should never treat a worktree as disposable solely because it is old. Cleanup decisions must account for uncommitted changes, unpushed commits, merge status, active processes, and an explicit ownership lease.

## 2. Scope

### MVP

- Register one or more root repositories.
- Discover all linked worktrees with `git worktree list --porcelain -z`.
- Create a worktree and branch without branch collisions.
- Assign a worktree to an agent/session and show that assignment.
- Show dirty files, ahead/behind state, upstream, HEAD, age, disk usage, and merge status.
- Open or reconnect to the terminal/agent associated with a worktree through provider adapters.
- Lock, unlock, archive, and safely remove a worktree.
- Preview every cleanup operation and explain why a worktree is or is not safe to remove.
- Record auditable lifecycle events locally.

### Later

- Desktop GUI.
- Remote machines and SSH-hosted worktrees.
- Team-shared ownership and leases.
- Automated cleanup policies and scheduled maintenance.
- Pull-request and CI status integrations.
- Container and dev-environment lifecycle management.

### Non-goals for the first release

- Replacing Git.
- Automatically resolving merge conflicts.
- Killing unknown processes without confirmation.
- Claiming reliable agent/window ownership for sessions that were not launched or registered through the tool.

## 3. Feasibility of agent/window attribution

It is possible to identify the agent or window using a worktree reliably when the session is launched by, or explicitly registered with, Worktree Manager. The launcher records a stable session ID, provider, PID/process group, worktree path, terminal metadata, and any provider-specific thread/window identifier. A heartbeat maintains a renewable lease.

For already-running sessions, attribution is best effort. The tool can inspect process working directories and descendants and may use terminal or agent-specific APIs, but operating-system permissions and app behavior make this incomplete. The UI must label the source and confidence, for example:

- `registered`: explicit session registration; high confidence.
- `launched`: created by Worktree Manager; high confidence.
- `detected`: process working directory matches; medium confidence.
- `stale`: the recorded process or heartbeat is gone.
- `unknown`: no attribution available.

Do not present a PID alone as identity. PIDs are reused; validate process start time and, where available, a generated session token.

## 4. Recommended architecture

Use a small local daemon as the single writer and coordination point. Both the TUI and future GUI call the same local API.

```text
TUI / future desktop GUI
          |
     local IPC API
          |
  worktree-manager daemon
    |       |        |
   Git   SQLite   agent adapters
    |                |
repositories      terminals/agents
```

Recommended implementation stack:

- Rust for the daemon, CLI, and TUI.
- `ratatui` plus `crossterm` for the TUI.
- SQLite for inventory, leases, and event history.
- JSON-RPC over a Unix domain socket initially; named-pipe abstraction for Windows later.
- Invoke the installed `git` executable with structured/porcelain output instead of using a Git reimplementation. This preserves compatibility with the user's Git configuration, hooks, credentials, and worktree behavior.

Keep the core split into packages/modules with no UI dependencies:

- `git`: repository discovery, worktree operations, status, refs, merge-base calculations.
- `inventory`: repositories, worktrees, sessions, leases, lifecycle timestamps, and events.
- `process`: active-process detection and validation.
- `agents`: provider-neutral interface plus individual adapters.
- `policy`: safety classification and cleanup recommendations.
- `service`: commands, concurrency control, filesystem watching, and IPC.
- `tui`: views, keyboard actions, confirmations, and streaming updates.

## 5. Source of truth and data model

Git remains authoritative for repository and worktree state. SQLite only stores facts Git does not provide and cached observations that can be rebuilt.

Core records:

| Record | Important fields |
| --- | --- |
| Repository | ID, canonical common Git directory, display name, default branch, added time |
| Worktree | ID, canonical path, Git administrative path, branch/ref, HEAD, discovered/created/last-seen times, lifecycle state |
| Session | ID, provider, provider session/thread ID, PID, process start time, terminal/window metadata, worktree ID |
| Lease | worktree ID, session ID, acquired/renewed/expires times, state |
| Observation | worktree ID, timestamp, dirty counts, ahead/behind, merge classification, disk usage, active process count |
| Event | timestamp, actor, action, target, result, structured details |

Timestamp rules:

- `created_at` is exact only for worktrees created through this tool.
- Existing worktrees get `first_seen_at`; any inferred creation time is labeled approximate.
- `last_used_at` is updated by registered session heartbeat, tool actions, or a detected active process. Filesystem modification time is supporting evidence, not the sole source.
- Display the provenance of inferred data so users are not given false precision.

## 6. Git state and merge classification

For each worktree, collect:

- Path, branch, HEAD, detached/bare/locked/prunable state.
- Staged, unstaged, untracked, conflicted, and ignored-file summary.
- Upstream and ahead/behind counts when an upstream exists.
- Commits unique to the worktree branch relative to the configured integration branch.
- Whether the branch tip is an ancestor of the local integration branch.
- Whether it is merged into the local integration branch and, separately, the configured remote-tracking branch.
- Last commit time and author.
- Disk usage, with `.git` common objects reported separately from worktree-local files.

Use explicit terms in the UI:

- `clean`: no index or working-tree changes.
- `dirty`: staged, unstaged, untracked, or conflicted changes exist.
- `merged locally`: branch tip is reachable from the chosen local integration branch.
- `merged remotely`: branch tip is reachable from its configured remote-tracking integration branch after the last fetch.
- `unmerged commits`: branch has commits not reachable from the integration branch.
- `unknown`: missing base/upstream or Git operation failed.

Never equate a merged pull request with local ancestry without a provider integration. Squash and rebase merges require a separate provider-derived status and should not be used alone to authorize deletion.

## 7. Concurrency and collision prevention

All mutations go through the daemon and acquire a repository-scoped mutex. Creation should:

1. Refresh `git worktree list` and prune only stale metadata that Git itself identifies as prunable.
2. Validate and normalize the requested path and branch.
3. Reject a branch already checked out elsewhere unless the user explicitly chooses a detached or new-branch workflow.
4. Reserve the intended path/branch in SQLite inside a transaction.
5. Run `git worktree add` with arguments passed directly, never through a shell string.
6. Record the resulting Git identity and emit an event.
7. Roll back the reservation on failure and rescan Git state.

Use `git worktree lock` for long-lived worktrees that must not be pruned or moved, and a separate renewable ownership lease for agent assignment. Git locks and agent leases solve different problems and should both be visible.

Changes made outside the tool are expected. Watch the common Git directory and registered worktree roots, debounce events, and periodically reconcile the database with Git.

## 8. Agent integration contract

Define a provider-neutral adapter with these capabilities:

- `launch(worktree, prompt?, options) -> session`
- `discover(worktree) -> session candidates`
- `health(session) -> active | idle | stale | unknown`
- `focus(session)` or `attach(session)`
- `send(session, message)` when the provider offers a safe API
- `stop(session)` only with explicit confirmation

Initial adapters:

1. Generic terminal process: launch a configured command in the worktree; reconnect by terminal session ID where supported.
2. `tmux`: strong attach/focus semantics and stable session naming.
3. Codex adapter: record task/thread identity when launched or registered; use supported app/CLI interfaces to focus or send follow-up work.

Provider capabilities vary, so the UI should show available actions rather than assuming every agent supports messaging or attachment. Credentials and provider tokens must stay in OS credential storage or the provider's own configuration, never in the project database.

## 9. TUI design

Primary screen:

```text
Repository: example                         Filter: active, dirty

WORKTREE             BRANCH          STATE       OWNER        LAST USED   SIZE
main                 main            clean       terminal-1   now         1.2 GB
feature/auth         feat/auth       dirty (3)   codex-42     8m          780 MB
fix/cache            fix/cache       merged      stale        12d         2.4 GB

[Enter] Details  [n] New  [a] Agent  [o] Open  [l] Lock  [x] Cleanup
```

The detail view contains:

- Identity: canonical path, branch, HEAD, integration branch, upstream.
- Changes: staged/unstaged/untracked/conflicted file counts with drill-down.
- Commits: ahead/behind and unique commit summaries.
- Usage: assigned agent, lease/heartbeat, detected processes, created/first-seen/last-used times.
- Storage: total worktree-local size and largest directories.
- Safety: a human-readable deletion assessment and blockers.
- Actions: open shell, focus/attach agent, send investigation prompt, diff, lock, archive, remove.

The cleanup screen should rank candidates, allow filters, and show an exact preview. Destructive confirmation names the path and explains blockers; `--force` is never the default.

## 10. Cleanup safety policy

Classify each worktree:

- `in use`: active lease or validated process; removal blocked.
- `unsafe`: dirty, conflicted, or unmerged commits; removal blocked by default.
- `review`: attribution is stale/unknown, remote status is stale, or merge state is ambiguous.
- `eligible`: clean, no active process, no unique commits, merge state verified, minimum age reached.

Default removal flow:

1. Refresh Git and process state.
2. Re-evaluate policy immediately before mutation.
3. Show the worktree path, branch, unique commits, dirty file counts, age, and estimated reclaimed space.
4. Remove through `git worktree remove` without force.
5. Delete the branch only as a distinct, separately confirmed action.
6. Retain the event record and tombstoned metadata.

Support an `archive` action before deletion: export a patch for tracked changes, list untracked files, and optionally create a recovery branch/commit only when explicitly requested. Do not silently commit user changes.

## 11. CLI and local API surface

Even with a TUI, expose scriptable commands:

```text
wtm repo add <path>
wtm list [--repo <id>] [--json]
wtm create <branch> [--base <ref>] [--path <path>]
wtm status <worktree> [--json]
wtm agent launch <worktree> [--provider <name>]
wtm agent attach <worktree>
wtm agent send <worktree> <message>
wtm lock|unlock <worktree>
wtm cleanup scan [--json]
wtm remove <worktree> [--delete-branch]
wtm doctor
wtm tui
```

Machine-readable output should be versioned from the beginning. Mutating API calls accept idempotency keys so retries cannot create duplicate worktrees or sessions.

## 12. Delivery phases

### Phase 0: validate workflows

- Capture representative worktree layouts and agent-launch patterns.
- Write safety invariants and fixtures for clean, dirty, detached, locked, missing, and corrupted worktrees.
- Confirm supported platforms; target macOS and Linux first unless Windows is a requirement.
- Decide the exact integration-branch selection rule per repository.

Exit criterion: fixtures and behavior specification cover every cleanup classification.

### Phase 1: read-only core and CLI

- Create the Rust workspace and core module boundaries.
- Implement repository discovery and porcelain parsing.
- Implement status, ancestry, timestamps/provenance, disk usage, and JSON output.
- Add fixture-backed integration tests using temporary Git repositories.

Exit criterion: `wtm list` and `wtm status` accurately describe all fixtures without modifying them.

### Phase 2: safe lifecycle operations

- Add daemon, SQLite schema/migrations, repository mutexes, reconciliation, and event log.
- Implement create, lock, unlock, move, and non-force remove.
- Implement safety policy and dry-run cleanup reports.
- Add crash/retry tests and external-mutation reconciliation tests.

Exit criterion: concurrent creation cannot duplicate a path or branch, and unsafe removal is blocked in tests.

### Phase 3: TUI

- Build list, details, create, agent, and cleanup views.
- Stream daemon updates and retain full keyboard accessibility.
- Add actionable errors and recovery guidance.

Exit criterion: the complete lifecycle can be performed from the TUI without losing diagnostic detail.

### Phase 4: agent sessions

- Implement launcher/registration, heartbeats, process validation, and leases.
- Add generic terminal and `tmux` adapters, then one supported coding-agent adapter.
- Implement focus/attach and guarded message sending.

Exit criterion: a launched agent is attributed reliably, survives UI restarts, can be reattached, and becomes stale when it exits.

### Phase 5: cleanup automation and hardening

- Add policy configuration, notifications, archives, and scheduled scans.
- Test symlinks, nested paths, submodules, large repositories, permissions, multiple users, and abrupt daemon termination.
- Add structured logs, diagnostics bundle, database backup, and schema recovery.

Exit criterion: cleanup remains conservative under missing data and interrupted operations.

### Phase 6: optional desktop GUI

- Validate TUI usage data and workflows.
- Select a thin desktop shell that consumes the existing local API.
- Add richer diff, history, and storage visualizations without moving Git logic into the UI.

## 13. Testing strategy

- Unit tests for porcelain parsers, path normalization, policy decisions, and provider capability handling.
- Property tests for unusual branch/path names and status records.
- Integration tests that create real temporary repositories and worktrees.
- Concurrency tests with simultaneous create/remove/status operations.
- Failure injection around Git subprocess exit, daemon restart, SQLite transaction failure, and partially removed directories.
- Golden tests for CLI JSON and TUI state models.
- Platform tests for process discovery and Unix socket permissions.
- End-to-end tests for launch, heartbeat, attach, stale detection, cleanup preview, and removal.

Critical invariants:

- No removal while an active validated lease or process exists.
- No default removal with dirty files or unique commits.
- No branch deletion bundled invisibly with worktree removal.
- No database cache can override current Git state.
- Every mutation is attributable and recorded.

## 14. Security and privacy

- Bind IPC to the current user and restrict socket/database permissions.
- Canonicalize paths and reject operations outside registered repository/worktree roots.
- Pass Git and agent arguments as arrays, not shell-expanded strings.
- Treat repository content, branch names, hooks, and provider output as untrusted input.
- Make process inspection opt-in where platform permissions require it.
- Store no prompts, diffs, environment variables, or credentials by default; store only identifiers needed for reconnection.
- Redact secrets from diagnostic exports.

## 15. Early product decisions to confirm

These choices should be settled during Phase 0, but do not block repository initialization:

1. macOS/Linux only for the first release, or Windows from day one.
2. Codex-first agent integration, or a provider-neutral terminal launcher first.
3. Per-repository worktree location convention, such as a sibling directory versus a centralized directory.
4. How the integration branch is selected: explicit configuration, remote default branch, or local default.
5. Whether cleanup may automatically remove only `eligible` worktrees or must always require confirmation.
6. Whether the daemon starts on login or lazily with the CLI/TUI.

Recommended defaults: macOS/Linux first, generic terminal plus Codex registration, explicit repository configuration with sensible detection, sibling worktree directories, confirmation-required cleanup, and lazy daemon startup.

## 16. Definition of MVP complete

The MVP is complete when a user can register a repository, see every worktree and its trustworthy safety state, create a collision-free worktree, launch/register and later reconnect to an agent, inspect changes and unique commits, and safely reclaim an eligible worktree's disk space—with every mutation recorded and no force deletion in the normal flow.
