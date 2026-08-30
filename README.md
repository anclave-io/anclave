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

anclave      : TUI client
anclave-cli  : headless client
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

**0.1.0: an early preview of the daemon and CLI.** The session core and the
containment layer work and are verified in CI; the terminal client is a
demonstration, not yet a usable interface. Expect the protocol and the
configuration format to change without ceremony.

### What works

| | |
|---|---|
| Daemon, typed IPC, SQLite persistence | sessions survive a daemon restart and are re-adopted |
| Session lifecycle | create, list, get, restart, delete, attach, detach |
| tmux-backed terminals | input, resize, screen capture, output streaming |
| Workspaces | Git worktrees, and one workspace spanning several repositories |
| Security profiles | declared per session and reported to every client |
| Environment construction | credential variables really are withheld, host mode included |
| Containment | three backends: Apple `container`, podman, docker |
| Network isolation | `network = "none"` under podman and docker |

Containment is checked against **real container runtimes on every push**, not
only in unit tests: CI starts containers under both podman and docker and
asserts that a no-network profile leaves the agent with no route out, that
credentials planted in the daemon's environment do not reach it, and that each
backend's hardening flags are accepted by the runtime receiving them.

### What does not work yet

**The TUI is a preview.** It lists sessions and shows a captured screen on
`Enter`; it does not stream, and it renders no color, cursor, or alternate
screen: because `ScreenSnapshot` currently carries its content as plain text.
A full-screen coding agent will look wrong through it. Use `anclave-cli` for
anything real.

**Nothing enforces a network allowlist or proxy-only mode.** Both are declared
in the profile format and both are *refused* at startup by every backend
rather than silently ignored. Apple's `container` cannot remove the network at
all, so it refuses `network = "none"` too: use podman or docker for that.

Also absent, and planned rather than forgotten: the approval broker, the
tamper-evident audit log, remote hosts over SSH and WSL, tasks, inter-session
messages, automations, Lua plugins, and migration tooling.

**Not a security boundary yet, in one specific sense:** a contained session is
genuinely contained, but Anclave keeps no tamper-evident record of what it
did. Treat the audit story as absent until the audit log lands.

### Platforms

Linux and macOS, x86_64 and arm64. The daemon needs a Unix socket and a POSIX
process model, so there is no Windows build.

| Document | What it is |
|---|---|
| [`IMPLEMENTATION_PLAN.md`](IMPLEMENTATION_PLAN.md) | The 43-commit plan this is built against |
| [`ARCHITECTURE.md`](ARCHITECTURE.md) | Crate boundaries, dependency rules, and what each security layer actually enforces |

## Building and checking

```bash
cargo test --workspace                                    # 163 tests
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
```

CI runs these on every push to `main` and every pull request, on Linux.

Two external tools change what the suite actually covers, so CI installs both
rather than letting coverage quietly shrink:

**tmux**: `crates/daemon/tests/tmux_backend.rs` skips without it, but the
end-to-end suite in `crates/cli/tests/` creates real sessions and fails rather
than skips.

**podman**: `crates/daemon/tests/containment.rs` starts real containers and
asserts that a no-network profile leaves an agent with no route out, that
planted credentials do not reach it, and that the workspace is mounted where
the policy says. On Linux podman needs no VM. These tests *skip* when podman
cannot run, so CI additionally asserts that they ran: a skipped security test
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
