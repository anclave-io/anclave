# Anclave Rewrite Implementation Plan

This plan describes a complete rewrite of Anclave as a daemon-backed, secure, persistent session-management platform. The rewrite should be developed on a new branch, in small independently validated commits, while the existing implementation remains available until the replacement reaches the required compatibility milestone.

## Goals

Anclave should manage multiple coding-agent sessions in persistent, isolated workspaces and expose the same core through a CLI and TUI. It should support local and remote execution, Git worktrees, multi-repository sessions, terminal persistence, optional sandboxing, scoped credentials, policy controls, and an append-only security audit trail.

The central architectural change is:

```text
anclaved
├── owns live sessions and side effects
├── owns terminal readers and writers
├── owns backends, sandboxes, policies, and credentials
├── persists durable state
└── exposes a typed IPC protocol

anclave
└── TUI client

anclave-cli
└── headless client
```

The TUI and CLI must not independently own session lifecycle logic or access SQLite directly. They communicate with the daemon through a versioned protocol.

## Security boundary

Anclave has two separate security models:

### UI/plugin security

This protects Anclave from Lua UI plugins:

- restricted Lua standard library
- no `os`, `io`, `debug`, `package`, or dynamic loaders
- explicit plugin capabilities
- trust tied to absolute path and content digest
- bounded plugin execution
- versioned plugin API

### Agent execution security

This protects the host and user resources from coding agents. It must be modeled separately. Worktrees and symlink workspaces are workspace isolation only; they reduce merge conflicts but do not contain process authority.

The rewrite must make these controls explicit:

- sandbox type
- filesystem visibility
- network access
- credential inheritance and grants
- approval policy
- persistence policy
- audit behavior

The default compatibility mode may run on the host, but it must be visibly labeled as ambient-trust execution rather than sandboxing.

## Core design principles

1. **Daemon owns live state.** Clients render and issue requests; the daemon owns processes, terminal readers, retries, and lifecycle transitions.
2. **Protocol before clients.** Define and test the typed IPC protocol before implementing the TUI or full CLI.
3. **Explicit boundaries.** Keep terminal handling, backends, workspaces, sandboxes, credentials, policies, and audit logging behind narrow interfaces.
4. **Small core.** Build reliable local session management before adding extensions, remote mirroring, tasks, or automations.
5. **Workspace isolation is not security isolation.** Never describe Git worktrees or symlinks as agent containment.
6. **No ambient secrets by default.** Environment variables, SSH agent sockets, credential files, and cloud configuration must be filtered explicitly.
7. **Enforce policy below the agent.** Prompts and shell-string parsing are not security boundaries.
8. **Recovery is a feature.** A broken Lua plugin, crashed daemon, dropped backend, or failed sandbox must not make the system unusable.
9. **Every commit is green.** Each commit must compile and pass its relevant tests.
10. **Document behavior changes.** Update architecture, compatibility, and security documentation in the same commit as implementation changes.

## Branch and repository strategy

Create a new branch before implementation:

```text
rewrite/anclaved-core
```

Initially keep the rewrite isolated from the existing application. Do not rewrite the existing implementation in place until the daemon-backed replacement has passed the migration and compatibility milestones.

Recommended new boundaries:

```text
rewrite/
├── README.md
├── ARCHITECTURE.md
└── crates/
    ├── protocol/
    ├── daemon/
    ├── cli/
    ├── tui/
    ├── agent/
    ├── backend/
    ├── terminal/
    ├── workspace/
    ├── security/
    └── audit/
```

The exact Cargo workspace layout may follow repository conventions, but dependency direction must remain explicit and tested.

---

# Phase 0: Baseline and branch setup

## Commit 1 — Record the current baseline

### Work

Add:

```text
docs/rewrite/baseline.md
docs/rewrite/compatibility-matrix.md
```

Record:

- current build, formatting, lint, and test commands
- current CLI commands and flags
- current configuration files
- current database schema and migration state
- current agent launch and resume behavior
- supported operating systems and required external tools
- local, SSH, WSL, and psmux limitations
- current terminal and UI behavior
- known security boundaries and missing agent containment

### Validation

Run and record:

```bash
cargo fmt --all -- --check
cargo check --all
cargo nextest run --all
cargo clippy --all-targets --all-features -- -D warnings
```

### Acceptance criteria

- Existing behavior is documented.
- No production behavior changes.
- Baseline commands are reproducible.

## Commit 2 — Create the rewrite workspace

### Work

Create the rewrite source boundary and placeholder crates/modules. Add rewrite architecture documentation describing allowed dependencies and the intended daemon/client split.

### Validation

```bash
cargo check --workspace
cargo test --workspace
```

### Acceptance criteria

- New rewrite code compiles independently.
- Existing binaries still build.
- New modules do not casually depend on the old application implementation.

---

# Phase 1: Protocol-first foundation

## Commit 3 — Add common identifiers and protocol types

### Work

Define stable types:

```text
SessionId
AgentId
BackendId
WorkspaceId
SandboxId
RequestId
```

Define shared values:

```text
Size
TerminalInput
ScreenSnapshot
SessionState
ErrorCode
```

Use explicit serialization and a protocol version.

### Validation

Test:

- identifier serialization and parsing
- invalid identifiers
- enum round trips
- unknown-value behavior
- protocol version handling
- deterministic serialization

### Acceptance criteria

All protocol types serialize deterministically and malformed values fail safely.

## Commit 4 — Define requests, responses, and events

### Work

Define initial requests:

```text
Ping
GetVersion
ListSessions
GetSession
CreateSession
DeleteSession
RestartSession
AttachSession
DetachSession
SendInput
ResizeSession
CaptureScreen
```

Define initial events:

```text
SessionCreated
SessionStateChanged
OutputChanged
ScreenChanged
SessionExited
BackendError
```

Every request should carry a request/correlation ID where appropriate. Responses must preserve correlation IDs and return structured errors.

### Validation

Add fixture tests for:

- every request and event serialization
- malformed messages
- oversized messages
- unsupported protocol versions
- correlated errors

### Acceptance criteria

The protocol can be tested without tmux, a daemon, or a terminal.

## Commit 5 — Implement framed local IPC

### Work

Implement Unix-domain-socket IPC with length-delimited frames rather than newline-delimited JSON. Support request/response messages and event subscriptions. Enforce frame size limits and clean client disconnects.

### Validation

Use temporary sockets and fake servers to test:

- multiple sequential requests
- concurrent requests
- concurrent clients
- event subscriptions
- client disconnects
- truncated frames
- oversized frames
- backpressure
- malformed payloads

### Acceptance criteria

A client can communicate with a fake daemon through the real framing and transport layer.

---

# Phase 2: Daemon skeleton

## Commit 6 — Add the daemon process

### Work

Implement:

```bash
anclaved --foreground
anclaved --socket PATH
```

The daemon should:

- bind the IPC socket
- answer `Ping`
- answer `GetVersion`
- handle SIGTERM/SIGINT
- shut down cleanly
- remove stale sockets safely
- set restrictive local socket permissions

### Validation

Test:

- daemon startup
- ping
- clean shutdown
- stale socket behavior
- socket permissions
- repeated startup and shutdown

### Acceptance criteria

A real daemon can start and answer a real protocol client.

## Commit 7 — Add daemon runtime and supervision

### Work

Create a central runtime containing:

```text
request router
client registry
subscription broadcaster
session registry
background task supervisor
shutdown coordinator
```

Add internal events and task cancellation. There are still no live agent sessions.

### Validation

Test:

- request routing
- event delivery
- task cancellation
- client cleanup
- shutdown with active tasks
- event ordering

### Acceptance criteria

All daemon live state is owned by one runtime rather than by clients.

## Commit 8 — Add durable SQLite storage

### Work

Create a new versioned rewrite schema. Keep the central session table small:

```text
sessions
agents
backends
workspaces
session_status
schema_meta
```

Keep optional concerns separate:

```text
session_tasks
session_messages
security_audit_events
```

Use migrations from the beginning.

### Validation

Test:

- fresh database creation
- migration ordering
- interrupted migration handling
- transaction rollback
- duplicate IDs
- concurrent readers
- schema version mismatch

### Acceptance criteria

The daemon can restart without losing durable session metadata.

---

# Phase 3: Local session lifecycle

## Commit 9 — Add explicit agent definitions

### Work

Implement versioned agent configuration with explicit launch behavior:

```toml
[agents.claude]
command = "claude"
args = []
resume_strategy = "session_id"
supports_fork = true
```

Represent behavior with explicit strategies:

```rust
enum ResumeStrategy {
    ExactSessionId(Vec<String>),
    Latest(Vec<String>),
    SessionFile { create: Vec<String>, resume: Vec<String> },
    FreshOnly,
}
```

### Validation

Test:

- configuration parsing
- unknown fields
- invalid strategies
- argument substitution
- shell-safe argument handling
- home expansion
- new-session command generation
- resume command generation
- fork command generation

### Acceptance criteria

Agent command generation is deterministic, isolated, and independently testable.

## Commit 10 — Add backend abstraction and local tmux backend

### Work

Define:

```rust
trait SessionBackend {
    async fn create(&self, request: CreateRequest) -> Result<SessionHandle>;
    async fn attach(&self, id: &SessionId) -> Result<AttachedSession>;
    async fn send_input(&self, id: &SessionId, input: &[u8]) -> Result<()>;
    async fn resize(&self, id: &SessionId, size: Size) -> Result<()>;
    async fn kill(&self, id: &SessionId) -> Result<()>;
}
```

Implement `LocalTmuxBackend`. Keep tmux command construction isolated behind a transport that can be faked in unit tests.

### Validation

Unit-test command construction with fake transports. Where tmux is available, integration-test:

- session creation
- window creation
- input delivery
- resize
- process termination
- reconnect

### Acceptance criteria

The daemon can create and manage a basic tmux-backed process.

## Commit 11 — Implement local session creation

### Work

Implement the local creation path:

```text
CreateSession
→ validate request
→ insert session record
→ prepare workspace
→ create tmux window
→ launch agent
→ publish state events
```

Use a state machine so failures during any step can be recovered or rolled back.

### Validation

Test:

- successful creation
- invalid agent
- invalid repository
- tmux failure
- agent launch failure
- database failure
- rollback after partial failure
- duplicate names
- daemon restart during creation

### Acceptance criteria

A session can be created through the protocol and appears consistently in storage and tmux.

## Commit 12 — Implement list, get, delete, and restart

### Work

Add:

```text
ListSessions
GetSession
DeleteSession
RestartSession
```

Make deletion idempotent and conservative. Stop the process, remove runtime resources, remove the workspace only according to policy, and persist cleanup results.

### Validation

Test:

- lifecycle transitions
- idempotent deletion
- restart after process exit
- restart after daemon restart
- failed cleanup reporting
- partial cleanup
- concurrent operations

### Acceptance criteria

The daemon supports a complete usable local session lifecycle.

---

# Phase 4: Terminal subsystem

## Commit 13 — Isolate terminal surface handling

### Work

Create:

```rust
trait TerminalSurface {
    fn resize(&mut self, size: Size);
    fn write_output(&mut self, bytes: &[u8]);
    fn screen(&self) -> ScreenSnapshot;
    fn input(&self, input: TerminalInput);
}
```

Wrap vt100 parsing behind this interface. Keep terminal state independent of UI layout and session lifecycle.

### Validation

Test:

- cursor movement
- colors
- alternate screen
- carriage returns
- wrapping
- malformed escape sequences
- minimum dimensions
- double-width characters
- output after process exit

### Acceptance criteria

Terminal parsing is testable without tmux or the UI.

## Commit 14 — Add terminal output streaming

### Work

The daemon should:

- read tmux output
- update terminal surfaces
- retain the latest screen
- publish coalesced screen events
- avoid sending every byte to every client
- provide the current screen to a newly attached client

### Validation

Test:

- output bursts
- event coalescing
- slow clients
- reconnect with latest screen
- terminal reader failures
- output after exit
- event ordering

### Acceptance criteria

Clients can attach and receive a correct current terminal screen.

## Commit 15 — Add input and resize routing

### Work

Implement:

```text
SendInput
ResizeSession
```

Validate session existence, attachment/access policy, input bounds, and terminal dimensions.

### Validation

Test:

- input reaches the process
- resize reaches tmux and the parser
- invalid dimensions
- disconnected sessions
- oversized paste
- concurrent input

### Acceptance criteria

A protocol client can interact with an agent terminal.

## Commit 16 — Add terminal recovery and restoration

### Work

Implement:

- reconnect after daemon restart
- reattach to existing tmux windows
- dropped-reader handling
- cursor restoration
- alternate-screen restoration
- terminal cleanup on signals

### Validation

Test:

- daemon restart with a running agent
- agent exit while attached
- simulated backend failure
- SIGTERM cleanup
- terminal restoration
- reconnect after temporary disconnect

### Acceptance criteria

The rewrite’s primary persistence promise works reliably.

---

# Phase 5: CLI client

## Commit 17 — Add the basic daemon-backed CLI

### Work

Implement:

```bash
anclave-cli daemon status
anclave-cli session list
anclave-cli session get ID
anclave-cli session create ...
anclave-cli session delete ID
anclave-cli session restart ID
anclave-cli session capture ID
anclave-cli session send ID TEXT
```

The CLI must use IPC only and must not read SQLite directly.

### Validation

Test:

- command parsing
- text output
- JSON output
- piped output
- missing daemon
- timeout handling
- structured errors
- exit codes

### Acceptance criteria

The new CLI can fully operate local sessions.

## Commit 18 — Add compatibility mode

### Work

Map compatible legacy commands to the new daemon or retain the old implementation temporarily. Document unsupported flags and behavior differences.

### Validation

Test:

- old command mappings
- deprecated flags
- migration warnings
- behavior differences
- failure messages

### Acceptance criteria

Users can evaluate the rewrite without losing access to the current CLI.

---

# Phase 6: Git and workspace management

## Commit 19 — Add repository discovery

### Work

Implement a focused Git layer:

```text
is_repository
current_branch
default_branch
status
remote_url
```

Use a single process-construction path and scrub inherited `GIT_*` location variables.

### Validation

Test:

- normal repository
- bare repository
- detached HEAD
- missing Git
- hostile paths
- inherited Git environment
- remote repository failure

### Acceptance criteria

Repository metadata is deterministic and safe.

## Commit 20 — Add worktree creation and removal

### Work

Implement worktree lifecycle with explicit cleanup records:

```text
create worktree
remove worktree
recover stale worktree
```

### Validation

Test:

- branch creation
- existing branch
- dirty source repository
- worktree cleanup
- stale lock
- interrupted creation
- concurrent creation
- cleanup after daemon crash

### Acceptance criteria

Session workspaces are isolated by branch and safely recoverable.

## Commit 21 — Add multi-repository workspaces

### Work

Implement per-session derived workspaces containing repository entries. Define explicit policies for symlinks, read-only members, duplicate names, missing repositories, path traversal, and cleanup.

Make clear in documentation that the workspace is not a security sandbox.

### Validation

Test:

- multiple repositories
- duplicate basenames
- missing members
- symlink escape attempts
- workspace recreation
- cleanup
- remote path compatibility

### Acceptance criteria

An agent can work across multiple repositories without provider-specific flags.

---

# Phase 7: Agent execution security

This phase is intentionally separate from workspace management. Worktrees do not count as security containment.

## Commit 22 — Add explicit security profiles

### Work

Add:

```rust
struct SecurityProfile {
    sandbox: SandboxKind,
    filesystem: FilesystemPolicy,
    network: NetworkPolicy,
    credentials: CredentialPolicy,
    approval: ApprovalPolicy,
    persistence: PersistencePolicy,
}
```

Initial profile examples:

```toml
[security.profiles.default]
sandbox = "host"
network = "full"
credentials = "ambient"
approval = "agent"

[security.profiles.untrusted]
sandbox = "container"
network = "none"
credentials = "none"
approval = "anclave"
```

`host` must be explicitly documented as non-contained compatibility mode.

### Validation

Test:

- profile parsing
- invalid combinations
- default selection
- profile persistence
- profile display in CLI
- profile display in TUI

### Acceptance criteria

Every session has an explicit, inspectable security posture.

## Commit 23 — Add environment and credential filtering

### Work

Build the child-process environment explicitly. Control:

- `SSH_AUTH_SOCK`
- cloud credential variables
- agent configuration variables
- Git credential variables
- Anclave identity variables

Never put secrets in command-line arguments or ordinary logs.

### Validation

Test:

- inherited environment filtering
- allowed variables
- denied variables
- selected credential files
- secret redaction
- child-process environment inspection

### Acceptance criteria

An agent receives only the environment and credentials selected by policy.

## Commit 24 — Add a credential provider abstraction

### Work

Define:

```rust
trait CredentialProvider {
    async fn issue(&self, request: CredentialRequest) -> Result<CredentialGrant>;
    async fn revoke(&self, grant: CredentialGrant) -> Result<()>;
}
```

Initially support:

- no credentials
- explicitly selected read-only files
- test-only mock short-lived tokens

Add expiration, scope, and revocation metadata without persisting secret values.

### Validation

Test:

- expiration
- revocation
- scope enforcement
- provider failures
- secret redaction
- no secret persistence

### Acceptance criteria

Credentials are explicit resources rather than ambient inheritance.

## Commit 25 — Add the sandbox abstraction

### Work

Define:

```rust
trait Sandbox {
    async fn prepare(&self, request: &SandboxRequest) -> Result<SandboxHandle>;
    async fn spawn(
        &self,
        sandbox: &SandboxHandle,
        command: &CommandSpec,
    ) -> Result<ProcessHandle>;
    async fn resize(&self, handle: &SandboxHandle, size: Size) -> Result<()>;
    async fn destroy(&self, handle: SandboxHandle) -> Result<()>;
}
```

Implement `HostSandbox` first for compatibility, but ensure it reports that it provides no containment.

### Validation

Test:

- sandbox lifecycle
- process exit
- cleanup after crash
- workspace visibility
- environment isolation
- startup failure

### Acceptance criteria

All agent launches pass through a security abstraction.

## Commit 26 — Add one real constrained sandbox backend

### Work

Research supported runtimes for each target platform before choosing an implementation. Possible backends include:

```text
Linux container
Apple containerization
Firecracker microVM
Windows sandbox/container
```

Implement one backend with:

- workspace mounts
- read-only/read-write policies
- no credential defaults
- no-network mode
- process cleanup
- resource limits where available

Do not make the core depend on one platform-specific sandbox.

### Validation

Test:

- filesystem containment
- network disabled
- process cleanup
- workspace persistence
- startup failure
- process escape attempts appropriate to the runtime
- resource limits where supported

### Acceptance criteria

At least one backend provides real agent containment.

## Commit 27 — Add enforceable network policies

### Work

Implement:

```text
none
allowlist
proxy-only
full
```

Enforcement must occur in the sandbox/backend or an operating-system/network proxy, not through agent prompts.

### Validation

Test:

- blocked outbound requests
- allowed hosts
- DNS behavior
- policy changes
- network failure reporting
- remote backend behavior

### Acceptance criteria

The selected network profile has observable enforcement.

## Commit 28 — Add the approval broker

### Work

Define protocol events and requests:

```text
ApprovalRequested
ApproveAction
DenyAction
ApprovalExpired
```

Begin with actions that can be controlled below the agent:

- force push
- branch deletion
- credential grants
- network escalation
- leaving the sandbox
- destructive workspace operations

Do not present shell-string parsing as a security boundary.

### Validation

Test:

- request and response
- timeout
- denial
- client disconnect
- duplicate approval
- unauthorized approval
- audit event generation

### Acceptance criteria

Policy-sensitive operations can be approved independently of the agent’s own UI.

## Commit 29 — Add the append-only security audit log

### Work

Create an audit stream separate from session metadata. Record:

```text
principal
session
action
policy
decision
timestamp
backend
result
```

Use hash chaining or another tamper-evident mechanism. Redact secrets. Authenticate remote event sources.

### Validation

Test:

- append ordering
- hash validation
- tamper detection
- crash recovery
- secret redaction
- remote event authentication

### Acceptance criteria

Security events can be independently reviewed and tampering is detectable.

---

# Phase 8: TUI client

## Commit 30 — Build the minimal daemon-backed TUI

### Work

Create a simple TUI with:

- session list
- current terminal
- status
- connection state
- basic keybindings

It must use the daemon protocol exclusively.

### Validation

Test with fake protocol events:

- session rendering
- focus movement
- input forwarding
- resize
- reconnect state
- empty state
- daemon error state

### Acceptance criteria

The new TUI manages local sessions without accessing daemon internals.

## Commit 31 — Add fallback and recovery UI

### Work

Ensure the TUI remains usable when:

- Lua fails
- a plugin is missing
- the daemon disconnects
- a session is unreachable
- UI configuration is invalid

Provide controls to:

- disable a plugin
- reload the UI
- inspect diagnostics
- reconnect
- quit safely

### Validation

Test:

- malformed plugin
- missing layout
- daemon unavailable
- protocol mismatch
- broken terminal surface
- corrupted UI configuration

### Acceptance criteria

A broken extensible UI cannot brick the application.

## Commit 32 — Add a versioned Lua plugin API

### Work

After the stable TUI works, add Lua support initially for:

- snapshot reads
- render trees
- commands
- bindings
- events

Every plugin declares an API version:

```lua
return {
  api_version = 1,
  id = "sessions",
  render = function(ctx)
    -- ...
  end,
}
```

### Validation

Test:

- API version checks
- restricted globals
- plugin timeouts
- malformed trees
- plugin reload
- fallback behavior
- unknown events and commands

### Acceptance criteria

Plugins are optional TUI clients and are not confused with agent security.

## Commit 33 — Add plugin trust and capabilities

### Work

Retain the strong existing UI security concepts:

- path + content digest trust
- capabilities by absence
- bounded execution
- explicit grants

Document prominently that these controls apply only to UI plugins, not coding agents.

### Validation

Test:

- modified trusted plugin
- revoked trust
- missing capability
- capability escalation attempts
- digest changes
- plugin isolation

### Acceptance criteria

The plugin security model is independently verifiable and accurately scoped.

---

# Phase 9: Remote backends

## Commit 34 — Add SSH through the daemon protocol

### Work

Prefer this architecture:

```text
local anclave client
→ SSH tunnel
→ remote anclaved
→ remote sandbox/backend
```

Avoid duplicating remote lifecycle behavior in the local client.

### Validation

Test:

- successful connection
- unavailable host
- timeout
- reconnect
- remote event authentication
- remote path handling
- protocol version mismatch
- remote daemon shutdown

### Acceptance criteria

Remote sessions use the same lifecycle and protocol as local sessions.

## Commit 35 — Add WSL backend

### Work

Implement WSL as a backend/transport variation. Keep WSL-specific path and process rules inside the backend.

### Validation

Test:

- distro selection
- daemon discovery
- Linux path handling
- process lifecycle
- reconnect
- unavailable distro

### Acceptance criteria

WSL differences do not spread into the general session model.

## Commit 36 — Add psmux/native Windows backend

### Work

Implement psmux-specific behavior behind the backend interface.

### Validation

Test platform-specific behavior for:

- input encoding
- paste
- resize
- window creation
- control-mode differences
- PowerShell command handling
- process cleanup

### Acceptance criteria

The core session model contains no psmux-specific branches.

---

# Phase 10: Orchestration features

These should be added only after the session, daemon, protocol, terminal, workspace, and security foundations are stable.

## Commit 37 — Add tasks

### Work

Implement tasks as a separate subsystem:

```text
create
list
show
edit
remove
run
```

Reference sessions by stable IDs rather than names.

### Validation

Test:

- task lifecycle
- invalid references
- session deletion
- task execution
- concurrent updates
- task-to-session behavior

### Acceptance criteria

Tasks do not expand or destabilize the core session table.

## Commit 38 — Add inter-session messages

### Work

Implement a daemon-backed mailbox with:

```text
send
inbox
claim
reply
prune
```

Use atomic claiming so concurrent clients cannot process the same message twice.

### Validation

Test:

- concurrent claimers
- reply routing
- sender identity
- body limits
- retention
- disconnected recipients
- duplicate delivery prevention

### Acceptance criteria

Sessions can exchange structured messages safely.

## Commit 39 — Add automations

### Work

Implement:

```text
Send
Spawn
Exec
```

Scheduling and execution must honor each session’s security profile. `Exec` must not silently bypass sandbox, credential, network, or audit rules.

### Validation

Test:

- due scheduling
- duplicate claims
- retries
- timeout
- process limits
- policy enforcement
- audit records
- sandboxed execution

### Acceptance criteria

Automations are ordinary policy-controlled daemon operations.

## Commit 40 — Add extension packages

### Work

Start with:

- versioned manifests
- external files
- install/uninstall
- activation state
- path traversal protection

Add self-healing only after the basic package lifecycle is stable.

### Validation

Test:

- manifest validation
- managed-file protection
- uninstall safety
- version mismatch
- malicious path traversal
- extension trust boundaries
- activation/deactivation

### Acceptance criteria

Extensions cannot overwrite arbitrary files or bypass security policy.

---

# Phase 11: Migration and release

## Commit 41 — Add old-state migration tools

### Work

Implement:

```bash
anclave-cli migrate inspect
anclave-cli migrate import
```

Import where safe:

- agent definitions
- compatible configuration
- session metadata
- selected preferences

Do not silently import ambiguous or unsafe security settings. Migration should have a dry-run mode.

### Validation

Test:

- old configuration formats
- partial migration
- invalid data
- duplicate sessions
- dry-run output
- rollback
- unsafe security settings

### Acceptance criteria

Migration is explicit, reviewable, and reversible.

## Commit 42 — Complete compatibility and security documentation

### Work

Document every existing feature as:

```text
supported
partially supported
replaced
deferred
removed
```

Document clearly:

- host mode is not sandboxing
- worktrees are not security isolation
- plugin security does not protect agents
- credentials and network policies depend on backend support
- remote trust and audit behavior

### Acceptance criteria

Users can understand behavior and security differences before migrating.

## Commit 43 — Switch default binaries

### Work

Make the daemon-backed implementation the default only after all release gates pass. Retain a rollback path during the transition.

### Release gates

- local sessions work
- CLI works
- TUI works
- terminal reconnect works
- worktrees work
- migration works
- at least one real sandbox works
- security posture is documented
- integration tests pass
- supported-platform builds pass

### Acceptance criteria

The rewrite becomes the default without making recovery from a regression impossible.

---

# Validation strategy

## Per commit

Run the smallest relevant checks:

```bash
cargo fmt --all -- --check
cargo check --workspace
cargo test -p affected-crate
```

For protocol, lifecycle, and storage changes:

```bash
cargo nextest run --workspace
```

For security changes:

```bash
cargo test --workspace security
cargo deny check
```

For UI changes, run the relevant TUI and plugin tests.

## Per phase

Run the complete suite:

```bash
cargo fmt --all -- --check
cargo check --all
cargo clippy --all-targets --all-features -- -D warnings
cargo nextest run --all
cargo deny check advisories
```

Run integration tests with:

- real tmux where available
- temporary repositories
- temporary sockets
- isolated HOME/config/data directories
- no production credentials
- network disabled for security tests
- fake transports for deterministic backend tests

## End-to-end daemon scenario

Maintain one canonical black-box scenario:

```text
start daemon
→ create session
→ observe creation event
→ send input
→ observe terminal output
→ resize
→ capture screen
→ disconnect client
→ reconnect
→ restart daemon
→ reconnect session
→ stop agent
→ delete session
→ verify workspace cleanup
→ verify audit events
```

Run this scenario locally and in CI where the required backend exists.

## Failure-injection testing

Inject failures at each boundary:

```text
database write fails
tmux command fails
agent exits
daemon crashes
client disconnects
network drops
sandbox startup fails
credential provider fails
approval times out
remote host disappears
workspace cleanup fails
```

For every failure, specify expected persisted state, retry behavior, user-visible error, cleanup behavior, and audit events.

## Security validation

Explicitly prove that:

- host mode is visibly non-sandboxed
- denied environment variables do not reach agents
- credentials are not logged
- no-network mode blocks network access
- workspace restrictions do not follow unauthorized symlinks
- audit tampering is detectable
- UI plugin capabilities do not affect agent permissions
- agent permissions do not affect plugin permissions
- approval decisions are authenticated and correlated
- remote security events cannot be fabricated by an unauthenticated client

---

# Commit discipline

Use small conventional commits such as:

```text
feat(protocol): add versioned session request types
feat(daemon): serve ping over unix socket
feat(session): create local tmux-backed sessions
feat(terminal): stream coalesced screen updates
feat(workspace): create isolated git worktrees
feat(security): add explicit execution profiles
feat(audit): add tamper-evident security events
feat(tui): add daemon-backed session view
test(remote): cover SSH reconnect behavior
docs(rewrite): document agent security boundaries
```

Each commit must:

- compile
- pass its relevant tests
- avoid unrelated refactoring
- update documentation when behavior changes
- have one clear purpose
- leave the rewrite in a runnable state

Do not combine protocol, storage, backend, UI, and security changes in one commit unless the change cannot be made independently.

---

# Initial milestone

The first useful milestone should be:

```text
anclaved
+ Unix IPC
+ SQLite
+ local tmux
+ one generic agent
+ session create/list/delete
+ terminal attach/input/resize
+ reconnect after daemon restart
+ minimal CLI
+ minimal fallback TUI
```

Do not begin with Lua plugins, remote mirroring, automations, or microVM support. Those features become much easier once the daemon, protocol, session state machine, and terminal subsystem are proven.

# Target architecture

```text
Clients:
  anclave
  anclave-cli
  optional UI plugins

        │ versioned IPC

Daemon:
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

## Final implementation rule

Build and validate the daemon-backed local session core first. Add terminal reliability, workspaces, security, the TUI, remote execution, plugins, and orchestration as independent layers. Preserve the current implementation until migration and compatibility are proven.

The rewrite should make the following distinction impossible to miss:

```text
workspace isolation ≠ agent security
UI plugin isolation ≠ agent security
remote execution ≠ sandboxing
SQLite session state ≠ tamper-proof audit history
```
