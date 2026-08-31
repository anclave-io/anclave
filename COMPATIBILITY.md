# Compatibility and status

What Anclave does today, what it does partly, and what it does not do yet.
The point of this page is that you can tell which is which **before** you move
work onto it, rather than by discovering a gap mid-task.

Statuses mean:

| | |
|---|---|
| **supported** | works, and is covered by tests that fail when it breaks |
| **partial** | works for the cases named, and the rest is stated here |
| **replaced** | the capability exists, in a different shape than you may expect |
| **deferred** | planned, not started; nothing depends on it |
| **removed** | deliberately not carried forward |

Anything not listed here is not implemented. A feature absent from this page
is absent from the program.

## Sessions and terminals

| Feature | Status | Notes |
|---|---|---|
| Create, list, get, delete, restart | supported | |
| Attach, detach, send input, resize | supported | |
| Screen capture with color | supported | the real grid, cursor and alternate screen included |
| Live streaming without polling | supported | events over the same socket |
| Sessions survive a daemon restart | supported | re-adopted from the multiplexer |
| A dead session is reported exited | supported | reconciled while the daemon runs, not only at startup |
| Multiple concurrent sessions | supported | |
| Session ownership | **replaced** | the daemon owns lifecycle; clients ask. A client cannot create a session by writing to a database, because it cannot open one |

## Clients

| Feature | Status | Notes |
|---|---|---|
| Headless CLI | supported | |
| Terminal client, two modes | supported | NAVIGATE and TERMINAL, `Ctrl+]` to leave |
| Terminal client: create a session | supported | `n` opens a form: name, agent, repository, branch |
| Terminal client: choose a profile at creation | **deferred** | the daemon's default applies; use `anclave-cli --security` to pick one |
| Terminal client: delete a session | **deferred** | use `anclave-cli session delete` |
| Reconnect and diagnostics | supported | `r` retries, `d` shows socket, versions and the last error |
| Protocol version check | supported | a mismatch is named as one, not retried |

## Workspaces

| Feature | Status | Notes |
|---|---|---|
| Git worktree per session | supported | needs `ANCLAVE_WORKSPACE_ROOT` |
| One workspace over several repositories | supported | |
| The agent starts inside its workspace | supported | |
| Worktree removed when the session is deleted | supported | the **branch** is kept: it can hold committed work |

## Security

See [`SECURITY.md`](SECURITY.md) for what each control does and does not
protect. Status only, here:

| Feature | Status | Notes |
|---|---|---|
| Security profiles, per session | supported | applied at launch, reported to every client |
| Environment construction | supported | credentials withheld rather than filtered out afterwards |
| Containment: podman, docker | supported | asserted against real runtimes in CI |
| Containment: Apple `container` | partial | separate kernel, but **no network isolation**: it refuses a restricted network rather than pretending |
| `network = "none"` | partial | podman and docker only |
| `network = "allowlist"` / `"proxy-only"` | **deferred** | declared in the format and **refused** at startup by every backend. Nothing silently ignores them |
| `credentials = "none"` / `"ambient"` | supported | |
| `credentials = "files"` | **deferred** | currently behaves as `none`: it withholds, it does not supply. Do not rely on it to deliver a credential |
| Approval: workspace destruction | supported | refused without a decision under `approval = "anclave"` |
| Approval: credentials, network widening, push, branch deletion | **deferred** | the types exist; nothing issues them yet |
| Audit log | partial | hash-chained and tamper-**evident**; see SECURITY.md for what that does not mean |

## Remote

| Feature | Status | Notes |
|---|---|---|
| A daemon on another host | supported | `ANCLAVE_HOST=user@box`, over an SSH-forwarded socket |
| Remote lifecycle | **replaced** | there is no remote backend. The remote daemon owns remote sessions exactly as the local one owns local ones, so remote and local share one implementation |
| WSL | **removed** | Windows is out of scope |
| Native Windows | **removed** | the daemon needs a Unix socket and a POSIX process model |

## UI plugins

| Feature | Status | Notes |
|---|---|---|
| Lua plugin panes | supported | opt-in via `ANCLAVE_PLUGIN_DIR` |
| Versioned plugin API | supported | a plugin declaring another version is refused, not adapted |
| Sandbox: withheld globals, instruction budget | supported | |
| Isolation between plugins | supported | each gets its own environment |
| Trust and capabilities | supported | keyed by path **and** content digest |
| Capabilities available | partial | `commands` only: focus and restart. That is the whole list |

## Orchestration

| Feature | Status | Notes |
|---|---|---|
| Tasks | **deferred** | |
| Inter-session messages | **deferred** | |
| Automations | **deferred** | |
| Extension packages | **deferred** | |

## Migration

| Feature | Status | Notes |
|---|---|---|
| `migrate inspect` | supported | read-only |
| `migrate import` | supported | dry run unless `--apply`; writes a rollback record first |
| Agent definitions, ordinary preferences | supported | |
| Session records | partial | reported, never created: creating one runs an agent, which a file deletion cannot undo. Create them yourself, choosing a profile as you go |
| Security settings | **removed** | never imported, by design. See SECURITY.md |

## Platforms

Linux and macOS, x86_64 and arm64. Intel macOS is built by cross-compiling on
Apple silicon and is **not exercised in CI**; the other three are.
