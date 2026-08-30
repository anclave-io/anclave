# Architecture

How Anclave is split into crates, which direction dependencies run, and which
of those rules are enforced rather than merely stated.

## The split

```text
Clients:
  anclave           TUI
  anclave-cli       headless
  optional UI plugins

        │ versioned IPC (length-delimited frames, Unix domain socket)

Daemon (anclaved):
  protocol server
  session state machine
  terminal manager
  backend manager
  workspace manager
  security policy
  credential provider
  approval broker
  audit logger
  SQLite persistence

        │ explicit interfaces

Execution:
  local tmux
  remote daemon over SSH
  WSL
  psmux
  container sandbox
  microVM sandbox
```

The daemon owns every live thing: processes, terminal readers, retries, and
lifecycle transitions. Clients render and issue requests. **No client opens the
database.** If a client needs a fact, the protocol carries it; if a client needs
a change, the protocol requests it and the daemon decides.

## Crates

| Crate | Owns | May depend on |
|---|---|---|
| `anclave-protocol` | Identifiers, request/response/event types, framing, protocol version | nothing internal |
| `anclave-terminal` | vt100 parsing behind a terminal-surface interface | `protocol` |
| `anclave-workspace` | Git discovery, worktrees, multi-repo workspaces | `protocol` |
| `anclave-security` | Profiles, environment construction, credentials, sandbox interface | `protocol` |
| `anclave-audit` | Append-only, tamper-evident event log | `protocol` |
| `anclaved` | Runtime, storage, backends, agent registry, the server | all of the above |
| `anclave-architecture` | Nothing — the dependency-rule test | nothing |
| `anclave-cli` | Headless client | `protocol` |
| `anclave` | TUI client | `protocol`, `anclave-cli` |

Dependencies point one way: **protocol ← everything**, and only the daemon
depends on more than the protocol. A leaf crate never reaches sideways —
`terminal` does not know what a workspace is, and `workspace` does not know
what a terminal is. They meet in the daemon.

Two rules that are easy to break and expensive to unbreak:

- **`protocol` stays free of implementation dependencies.** No SQLite, no
  tokio, no vt100. It is the one crate both sides of the socket compile, and a
  client must not link a database driver to speak it.
- **No client crate is a dependency of the daemon.** `anclaved` must never
  depend on `anclave` or `anclave-cli`. The arrow that direction is how
  lifecycle logic leaks back into clients.

## How the rules are enforced

`crates/architecture/tests/architecture_rules.rs` reads every member's manifest
and fails on a forbidden dependency. It checks the manifest rather than the
source, so a dependency that is declared but not yet used is still caught, and
it asserts that **every** crate in `crates/` is named by a rule — a new crate
fails the test until its place in the dependency order is declared, so the
allowlist cannot drift behind the workspace.

The crate ships no code. It exists because the workspace root carries no
package and workspace-wide tests need a member to live in.

## Deviations to decide when the code lands

Two design choices in the current prototype differ from the plan, and neither
has been ratified:

1. **The backend trait is synchronous.** The plan specifies `async fn` for
   create/attach/send_input/resize/kill. A synchronous trait means tmux
   invocations block whichever task calls them, so a slow or unreachable host
   blocks a request handler. Either the trait becomes async or every call site
   moves to a blocking pool — the current shape does neither.
2. **`agent` and `backend` live inside the daemon** rather than as their own
   crates. Defensible while both are small, but it means the agent-launch
   specification and the tmux command construction are not independently
   testable from outside the daemon, which is what the plan's separate crates
   were for.

## Screen fidelity

`ScreenSnapshot` currently carries its content as a plain `String`. That drops
colors, cursor position, and alternate-screen state, none of which a client can
reconstruct. Terminal recovery — restoring the cursor and the alternate screen
after a daemon restart — cannot be satisfied through that shape, so the
snapshot type is expected to grow cells and a cursor before the terminal phase
is called done.

## Security boundaries

Anclave has two security models. They are separate, they protect different
things from different threats, and neither substitutes for the other.

**UI plugin security** protects Anclave from its own extensions: a restricted
Lua standard library with `os`, `io`, `debug`, `package` and the dynamic
loaders *withheld* rather than stubbed, explicit per-plugin capabilities, trust
keyed to absolute path plus content digest, bounded execution, and a versioned
plugin API. Withholding beats stubbing because absence is checkable
statically — the lint configuration and the runtime environment can be made to
agree.

**Agent execution security** protects the host and the user's resources from
coding agents. It is modeled separately and enforced *below* the agent, in the
sandbox, backend, or operating system — never by prompting the agent and never
by parsing shell strings. It covers sandbox type, filesystem visibility,
network access, credential inheritance and grants, approval policy, persistence
policy, and audit behavior.

The distinction the codebase must make impossible to miss:

```text
workspace isolation   ≠ agent security
UI plugin isolation   ≠ agent security
remote execution      ≠ sandboxing
SQLite session state  ≠ tamper-proof audit history
```

Worktrees and symlink workspaces are workspace isolation. They reduce merge
conflicts between concurrent agents and contain no process authority whatever.
The default `host` profile runs agents with the user's full authority; it is a
compatibility mode and must report that it provides no containment, in the CLI,
in the TUI, and in its own `Sandbox` implementation.

## Build order

Local session core first, then layers, each independently validated:

```text
protocol → daemon skeleton → local session lifecycle → terminal
  → CLI → workspaces → agent security → TUI → remote → orchestration
```

Lua plugins, remote mirroring, automations, and microVM support are all
deliberately late. Each becomes substantially easier once the daemon, the
protocol, the session state machine, and the terminal subsystem are proven, and
each is much harder to get right if it arrives while those are still moving.
