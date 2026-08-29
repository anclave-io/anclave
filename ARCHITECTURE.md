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

## Security phase status

Phase 7 has begun. What exists, and what each piece actually enforces:

| Piece | State | Enforces |
|---|---|---|
| `SecurityProfile` + `SecurityConfig` | done | nothing by itself; it is the declaration every other piece reads |
| Environment construction | done | credential variables really are withheld, host mode included |
| Applied at launch | done | a session runs under a stored profile; restart re-resolves it |
| `CredentialProvider` | done | scope, expiry, and a cap on requested lifetime; issues no secret into a grant |
| `Sandbox` trait | done | that no launch bypasses the boundary; `HostSandbox` *refuses* what it cannot apply |
| Runtime detection | done | nothing directly; it reports what this host *could* enforce |
| Apple `container` backend | done | **verified live**: separate kernel, workspace mounted, credentials absent; **refuses** a network policy |
| podman backend | done | **verified live**: only `lo`, network unreachable, credentials absent |
| Runtime selection | done | a profile naming a runtime gets that one or an error — never a substitution |
| Network policy | done (podman) | `network = "none"` is real under podman; Apple's runtime refuses it |
| Approval broker | not built | — |
| Audit log | not built | — |

### Choosing a containment runtime

Anclave hard-codes no containment technology. `security::runtime` holds a
catalogue per platform, probes the machine, and reports what it found —
including what it looked for and did not find, so an operator learns what to
install rather than being told "no".

| Platform | Ranked candidates |
|---|---|
| macOS | Apple `container`, podman, docker, `sandbox-exec` |
| Linux | Firecracker, podman, docker, bubblewrap |
| Windows | Hyper-V container, Windows Sandbox, WSL2 |

The catalogue is ordered by **isolation strength, not convenience**, and a
test asserts that ordering holds for every platform. The distinction the
report exists to carry is `Machine` (separate kernel in a VM) versus `Kernel`
(namespaces or a restricted token, host kernel shared) — a process-isolated
Windows container and a Hyper-V-isolated one are both "a container" and are
not the same boundary. Ranking by ease of setup would put the weaker one
first.

Windows is the least settled of the three. Hyper-V isolation is the only
candidate there that is both a real boundary and drivable per session;
WSL2 is listed because it is what people have, carrying the caveat that one
shared VM isolates sessions from Windows but not from each other.

### Proven, not just tested

The Apple backend was verified against a real running agent, not only in unit
tests. With `ANCLAVE_TEST_SECRET`, `AWS_SECRET_ACCESS_KEY` and `SSH_AUTH_SOCK`
planted in the daemon's environment, a session under a contained profile
reported:

```text
host:      Darwin 25.5.0
container: Linux 6.18.35        <- a separate kernel
PWD=/workspace                  <- relocated; the host path is not visible
planted credentials present: 0
```

That run also found a bug no unit test had: the agent came up with the
*host's* `PATH` (full of `/opt/homebrew`), `HOME=/Users/…` and
`SHELL=/bin/zsh`, none of which exist in a Linux image — so an agent would
have been unable to find its own binaries. Host facts (`PATH`, `HOME`, `USER`,
`LOGNAME`, `SHELL`, `TMPDIR`) are now forwarded only when the agent actually
runs on the host; terminal and locale variables travel everywhere because they
are equally true inside. Running the thing is how that was found.

### Network isolation, proven with a control

`network = "none"` under podman was verified against a running agent, with a
control so that "the request failed" could not be mistaken for something
unrelated:

```text
profile "online"     interfaces=eth0 lo   NETWORK=REACHABLE   leaks=0
profile "airgapped"  interfaces=lo        NETWORK=blocked     leaks=0
```

Same image, same agent, same workspace — one flag apart.

Two constraints found by running it, neither obvious from the docs:

**podman on macOS cannot mount `/tmp`.** Its VM shares `$HOME`, so a workspace
root under `/tmp` fails with `statfs: no such file or directory`. On macOS the
workspace root must sit somewhere the podman VM shares.

**A registry credential helper can break a local-image run.** A broken
`credsStore` in `~/.docker/config.json` makes podman fail before it looks at
the local image; `REGISTRY_AUTH_FILE` pointing at an empty JSON object is the
way past it.

### Two backends, and why that matters

`security::oci` writes the container command line once; `apple` and `podman`
are thin wrappers that declare *what they can enforce*. Two hand-written
copies would drift, and the second copy is where a hardening flag gets
forgotten.

| | Apple `container` | podman |
|---|---|---|
| Isolation | separate kernel per session | shares the host kernel |
| `network = "none"` | **cannot** — refused | **yes** |
| Hardening | `--cap-drop=ALL` | `--cap-drop=ALL`, `no-new-privileges` |
| Verified live | yes | argv only |

Neither is strictly better, which is the argument for pluggability: the
strongest isolation and the only working network control are currently in
different runtimes.

**A profile that names a runtime gets that runtime or an error.** Substituting
a weaker one because the named one is missing would be the most dangerous kind
of helpfulness — a profile silently dropping from a per-session VM to a shared
kernel while still reporting `contained: true`. A profile that names none
takes the strongest the host actually has, probed once at daemon startup
rather than per launch.

An allowlist and proxy-only are refused by *both* backends: neither runtime
can express "these hosts and no others", and the proxy does not exist yet.

### The first real backend, and what it cannot do

`security::apple` drives Apple's `container`: a separate kernel per session on
Apple silicon. It is the first thing in the codebase that actually confines an
agent.

It also cannot isolate the network. `container` 1.3.0 exposes no
`--network none` — `--network` takes a network *name*, and `--no-dns` only
withholds resolver configuration, which is not a boundary. So the backend
**refuses** a profile asking for a restricted network rather than accepting it
and quietly ignoring it. Enforcing network policy on macOS needs a different
mechanism, and until one exists the honest answer is that this runtime does
not provide it.

Two structural choices worth keeping:

A sandbox returns **argv**, it does not spawn. The session backend still owns
the pty and the process lifecycle; the sandbox decides what that pty is
attached to. A sandbox that spawned its own process would give every session
two lifecycles, and returning argv is what makes containment assertable in a
unit test on a machine with no container runtime installed.

The wrapped argv never contains `--ssh`. That single flag forwards the SSH
agent socket into the container and would undo the entire credential policy,
so a test asserts its absence.

A constructed environment is delivered by launching the agent through
`env -i`, not through tmux's own `-e`. `-e` *adds* variables to whatever the
tmux server already holds, so the inherited credentials would survive
alongside the constructed ones — the policy would appear applied and change
nothing. `env -i` starts from empty, which is the only form matching what
`build_environment` promises.

Two rules keep the declaration honest:

`SecurityProfile::validate` **refuses to load** a profile promising enforcement
its sandbox cannot deliver — a restricted network or filesystem under `host`.
A control that displays as enforced and is enforced by nothing is worse than an
absent one.

Credentials are the exception, and the distinction is worth stating precisely.
The daemon *builds* the child environment, so withholding `SSH_AUTH_SOCK` and
the cloud variables is real enforcement even on the host. What the host cannot
do is stop the agent reading a credential *file* off disk — that needs a
filesystem policy. So `host` + `credentials = none` is allowed, and
`SecurityProfile::caveats` states the gap rather than letting the profile
overclaim.

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
