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

## Using it

The daemon owns the sessions; `anclave-cli` and the `anclave` TUI are clients
of it. Start it with a workspace root, which is where per-session Git
worktrees are built:

```sh
export ANCLAVE_WORKSPACE_ROOT=~/.anclave/workspaces
anclaved --socket /tmp/anclaved.sock &
```

Without that variable the daemon runs, but any session asking for a workspace
is refused: it is the one piece of setup a `--repo` session needs and cannot
infer.

Create a session on its own worktree, type into it, read the screen back:

```sh
anclave-cli session create work --repo ~/code/myproject --branch feature-x
anclave-cli session list
anclave-cli session send session-0 'ls -la
'
anclave-cli session capture session-0
```

`--repo` wants a path to a Git repository that exists, and `--branch` names a
branch to create. The agent starts inside the resulting worktree. Naming an
agent is optional and goes before the flags (`session create work claude
--repo ...`); agents beyond the built-in shell are declared in a TOML file
named by `ANCLAVE_AGENTS_FILE`:

```toml
[[agents]]
name = "claude"
command = "claude"
args = []
```

### A daemon on another machine

Set `ANCLAVE_HOST` to an ssh destination and the client forwards that host's
daemon socket instead of using the local one:

```sh
ANCLAVE_HOST=me@devbox anclave-cli session list
ANCLAVE_HOST=me@devbox anclave              # the TUI, against the remote
```

Everything downstream is unchanged, because the remote daemon owns remote
sessions exactly as the local one owns local sessions:

```text
local client → SSH tunnel → remote anclaved → remote backend
```

Nothing in the client knows a session is remote. Paths in `--repo` are
interpreted on the host that runs the daemon, which is the machine the
worktree ends up on. Authentication is ssh's: Anclave adds no second
credential, and the remote socket's own permissions decide who may drive that
daemon.

`anclave` is the terminal client against the same daemon: `j`/`k` move between
sessions, `enter` focuses one, `Ctrl+]` leaves terminal mode, `q` quits.
Quitting leaves every session running, because the daemon owns them and not
the client.

Sessions outlive the daemon too. Restart `anclaved` and it re-adopts what is
still running.

## Status

**0.2.2: the daemon, the CLI, and a terminal client you can work in.** The
session core and the security layer are complete and verified against real
container runtimes in CI. Expect the protocol and the configuration format to
change without ceremony.

### What works

| | |
|---|---|
| Daemon, typed IPC, SQLite persistence | sessions survive a daemon restart and are re-adopted |
| Session lifecycle | create, list, get, restart, delete, attach, detach |
| Session state | a session whose agent dies is reported exited, without restarting the daemon |
| Terminals | the real screen grid with color, streamed live, cursor and alternate screen included |
| Terminal client | create sessions, move between them, type into one, diagnose the connection |
| Workspaces | Git worktrees, one workspace spanning several repositories, and the agent starts in it |
| Security profiles | declared per session, applied at launch, reported to every client |
| Environment construction | credential variables really are withheld, host mode included |
| Containment | three backends: Apple `container`, podman, docker |
| Network isolation | `network = "none"` under podman and docker |
| Approval broker | daemon-performed actions can require a decision |
| Audit log | hash-chained; an edited or removed entry is detectable |
| Remote hosts | a daemon on another machine, over an SSH-forwarded socket |
| Migration | inspect and import a previous installation's agents and preferences |

Containment is checked against **real container runtimes on every push**, not
only in unit tests: CI starts containers under both podman and docker and
asserts that a no-network profile leaves the agent with no route out, that
credentials planted in the daemon's environment do not reach it, and that each
backend's hardening flags are accepted by the runtime receiving them. The
terminal client is driven through a real pty and its output parsed by a real
VT parser, on Linux x86_64, Linux arm64 and macOS arm64.

### What it does not do yet

**The terminal client is young.** It creates, lists, focuses and drives
sessions, and `d` shows why it is unhappy when it is. It does not yet offer a
security profile at creation, delete a session, or manage plugins beyond
reloading them.

**Nothing enforces a network allowlist or proxy-only mode.** Both are declared
in the profile format, and a session asking for one is *refused* when it is
created rather than run with a weaker policy. Apple's `container` cannot
remove the network at all, so it refuses `network = "none"` too: use podman or
docker for that. [`SECURITY.md`](SECURITY.md) has the full table of what each
backend honors.

**The approval broker gates what the daemon does, not what the agent does.**
It cannot intercept an agent running `git push --force` inside its own
process, and reading a command line to guess intent is not a boundary. What it
gates is credential issuance, workspace destruction, and network widening:
things the daemon performs on the agent's behalf. Making an agent action
approvable means not giving the sandbox the capability in the first place.

**The audit log detects tampering; it does not prevent it.** Editing or
removing an entry breaks the chain and `anclave-cli audit verify` reports
where. Rewriting the whole chain from a given point, or truncating the tail,
leaves something that still verifies. Preventing that needs the chain head
published somewhere this daemon does not control.

Absent and planned rather than forgotten: tasks, inter-session messages,
and automations.

### Platforms

Linux and macOS, x86_64 and arm64. The daemon needs a Unix socket and a POSIX
process model, so there is no Windows build. Intel macOS is built and shipped
by cross-compiling on Apple silicon, but is not tested in CI: those runners
are being retired and starve.

| Document | What it is |
|---|---|
| [`COMPATIBILITY.md`](COMPATIBILITY.md) | Every feature as supported, partial, replaced, deferred or removed |
| [`SECURITY.md`](SECURITY.md) | What each control protects, and what it does not |
| [`IMPLEMENTATION_PLAN.md`](IMPLEMENTATION_PLAN.md) | The 43-commit plan this is built against |
| [`ARCHITECTURE.md`](ARCHITECTURE.md) | Crate boundaries, dependency rules, and what each security layer actually enforces |

## Building and checking

```bash
cargo test --workspace                                    # 244 tests
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
```

CI runs these on every push to `main` and every pull request, on Linux
x86_64, Linux arm64 and macOS arm64.

Three parts of the suite test against the real thing rather than a stand-in,
so what they cover depends on what is installed. CI installs all of it rather
than letting coverage quietly shrink:

**tmux**: `crates/daemon/tests/tmux_backend.rs` skips without it, but the
end-to-end suite in `crates/cli/tests/` creates real sessions and fails rather
than skips.

**a pty**: `crates/tui/tests/pty_e2e.rs` runs the terminal client on a real
pty and parses what it writes with a real VT parser, so the assertions are
about bytes rather than about one emulator. It checks that quitting restores
the terminal it took, that a session list renders, that attaching shows the
agent's output, and that a 1x1 window does not crash it.

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

### UI plugin controls are not agent controls

The terminal client can load Lua plugin panes. They have their own security
model, and it is worth being blunt about its scope, because a control that
sounds like it constrains an agent and does not is worse than no control:

| | UI plugins | Coding agents |
|---|---|---|
| What it is | a pane that draws | a process that runs your tools |
| Where it runs | inside the client | under a security profile |
| Withheld by | absence: `io`, `os`, `debug`, `package` are not in the VM | the sandbox, the constructed environment, the network policy |
| Bounded by | an instruction budget per render | the container, or nothing in `host` mode |
| Granted by | `anclave-cli plugin trust PATH` | the profile the session was created with |

**Trusting a plugin does not widen what any agent may do.** It grants a pane
the ability to ask the client to do something the client already does, such as
focusing a session. A plugin cannot spawn a process, open a file, reach the
network, or talk to the daemon, whether or not it is trusted.

Trust is keyed by **path and content digest together**. A path alone would
carry a grant across an edit that replaced the file; a digest alone would let
identical bytes inherit a grant from anywhere on disk. Editing a trusted
plugin reports it as `modified` rather than silently keeping or silently
dropping the grant, because "this changed since you approved it" and "you
never approved this" are different things to tell someone.

```sh
anclave-cli plugin list                  # trust state and declared capabilities
anclave-cli plugin trust path/to/pane.lua
anclave-cli plugin revoke path/to/pane.lua
```
