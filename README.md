# Anclave

Anclave manages multiple coding-agent sessions in persistent, isolated
workspaces and exposes one core through a CLI and a TUI.

A daemon owns all live state:

```text
anclaved
├── owns live sessions and side effects
├── owns terminal readers and writers
├── owns backends, sandboxes, policies, and credentials
├── persists durable state
└── exposes a typed IPC protocol

anclave       — TUI client
anclave-cli   — headless client
```

Clients render and issue requests. They do not own session lifecycle logic and
never touch SQLite directly.

## Status

**Pre-implementation.** This repository currently holds the plan and the
architecture it is built against; no crates have landed yet.

| Document | What it is |
|---|---|
| [`IMPLEMENTATION_PLAN.md`](IMPLEMENTATION_PLAN.md) | The 43-commit plan |
| [`ARCHITECTURE.md`](ARCHITECTURE.md) | Crate boundaries and dependency rules |

## Two security models, deliberately separate

Anclave distinguishes between protecting *itself from UI plugins* and
protecting *the host from coding agents*. These are not the same problem and
do not share controls:

```text
workspace isolation   ≠ agent security
UI plugin isolation   ≠ agent security
remote execution      ≠ sandboxing
SQLite session state  ≠ tamper-proof audit history
```

Git worktrees and symlink workspaces reduce merge conflicts. They do not
contain process authority. Nothing here should describe them as though they do.

The default `host` execution mode runs agents on the host with the user's own
authority. It is a compatibility mode, and it is labeled ambient-trust rather
than sandboxed everywhere it appears.
