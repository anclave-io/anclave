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

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/anclave-io/anclave/main/scripts/install.sh | sh
```

Installs `anclaved`, `anclave` and `anclave-cli` into `~/.local/bin`. Set
`VERSION` to pin a release or `INSTALL_DIR` to put them elsewhere. Linux and
macOS, x86_64 and arm64; the daemon needs a Unix socket, so there is no
Windows build yet.

```sh
anclaved --socket /tmp/anclaved.sock &
anclave-cli daemon status
anclave-cli daemon sandbox   # what containment this host can provide
```

## Status

**Pre-implementation.** This repository currently holds the plan and the
architecture it is built against; no crates have landed yet.

| Document | What it is |
|---|---|
| [`IMPLEMENTATION_PLAN.md`](IMPLEMENTATION_PLAN.md) | The 43-commit plan |
| [`ARCHITECTURE.md`](ARCHITECTURE.md) | Crate boundaries and dependency rules |

## Building and checking

```bash
cargo test --workspace                                    # 163 tests
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
```

CI runs these on every push to `main` and every pull request, on Linux.

Two external tools change what the suite actually covers, so CI installs both
rather than letting coverage quietly shrink:

**tmux** — `crates/daemon/tests/tmux_backend.rs` skips without it, but the
end-to-end suite in `crates/cli/tests/` creates real sessions and fails rather
than skips.

**podman** — `crates/daemon/tests/containment.rs` starts real containers and
asserts that a no-network profile leaves an agent with no route out, that
planted credentials do not reach it, and that the workspace is mounted where
the policy says. On Linux podman needs no VM. These tests *skip* when podman
cannot run, so CI additionally asserts that they ran — a skipped security test
that reports success is worse than a missing one.

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
