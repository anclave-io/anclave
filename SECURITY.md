# Security model

Anclave has two security models that share no controls, and a set of things
that are frequently mistaken for security and are not. This page says which is
which, because a control you believe you have is worse than one you know you
lack.

For what is implemented versus deferred, see
[`COMPATIBILITY.md`](COMPATIBILITY.md).

## The two models

| | UI plugins | Coding agents |
|---|---|---|
| Protects | Anclave from a pane | your host from an agent |
| What runs | Lua, drawing a tree | your real tools, on your files |
| Withheld by | absence: `io`, `os`, `debug`, `package` are not in the VM | the sandbox, the constructed environment, the network policy |
| Bounded by | an instruction budget per render | the container, or **nothing** in `host` mode |
| Granted by | `anclave-cli plugin trust PATH` | the profile the session was created with |
| Worst case | a pane draws nonsense or stops drawing | an agent does anything you can do |

They are separate on purpose, and the separation is enforced rather than
promised: the plugin host may not depend on the daemon, the database, the
security crate, or an async runtime, and a test fails if that changes.

## Five things that are not what they look like

### 1. `host` mode is not sandboxing

The default profile runs the agent **on your machine with your authority**. It
can read and write anything you can and reach any network you can. It is a
compatibility mode, and every client reports it as ambient trust rather than
as a sandbox:

```
sandbox=host (ambient trust: no containment) network=full credentials=ambient
```

If you want containment, say so with a profile that asks for it. The daemon
will refuse a profile it cannot honor rather than run a weaker one.

### 2. Worktrees are not isolation

A Git worktree per session prevents *merge conflicts*. It does not contain
process authority. An agent in a worktree can leave it: `cd ..` is not a
security boundary, and neither is a path.

Workspaces reduce the chance of two agents fighting over one checkout. Nothing
more should be read into them.

### 3. Plugin security does not protect agents

Trusting a UI plugin grants a pane the ability to ask the client to do
something the client already does, such as focusing a session. It does not
widen what any agent may do. Revoking trust does not narrow it either.

A plugin cannot spawn a process, open a file, reach the network, or talk to
the daemon, trusted or not. An agent's authority comes only from its profile.

### 4. Credentials and network policies depend on the backend

A policy is a request the backend must be able to honor. Where it cannot, it
is refused rather than quietly downgraded. There are two refusals, at two
different times, and the difference matters when you are diagnosing one:

- **At daemon startup**, a profile that is *internally* unenforceable is
  rejected and the daemon does not start: asking for a restricted network
  while running on the host, or for a container with no image. This is about
  the profile alone.
- **At session creation**, a policy this host's runtime cannot express is
  rejected and the session is not created: `network = "none"` under Apple
  `container`, or any allowlist anywhere. This depends on what is installed,
  so the same profile can start a daemon on one machine and refuse a session
  on another.

Neither point runs a weaker policy than the one you asked for.

| Policy | Honored by |
|---|---|
| `network = "none"` | podman, docker. Apple `container` refuses it: it exposes no network isolation |
| `network = "allowlist"` | nothing yet. No runtime here can express it |
| `network = "proxy-only"` | nothing yet. No proxy is implemented |
| `credentials = "none"` | every backend, including `host` |
| `credentials = "files"` | **nothing yet**. It currently behaves as `none`: it withholds credentials, it does not supply them |

The environment is **constructed, not filtered**. A contained agent starts
from nothing and receives what the policy names, rather than starting from
your environment and having secrets removed. A filter has to know every name
worth removing; construction does not.

### 5. Remote trust is SSH's, and the audit log is evidence, not prevention

**Remote.** A remote session is a session on a remote daemon, reached by
forwarding its socket over SSH. Anclave adds no second credential and no key
material of its own. If you can reach that socket over SSH you can drive that
daemon, and the socket's own `0600` permissions decide who that is on the far
side. Anclave does not verify the host beyond what SSH verifies, and does not
re-authenticate events arriving over the tunnel: they come from the daemon you
connected to.

**Audit.** The log is hash-chained: editing or removing an entry breaks the
chain and `anclave-cli audit verify` reports where. That is tamper-*evidence*,
not tamper-*proofing*:

- rewriting the whole chain from a chosen point produces a log that verifies
- truncating the tail produces a log that verifies
- an attacker with write access to the file has both of those

Detecting either needs the chain head published somewhere the daemon does not
control. That is not implemented, so treat the log as a record that shows
casual alteration, not one that survives a determined one.

## Approval

Under `approval = "anclave"`, actions the daemon performs **on the agent's
behalf** can require a decision. Today that is workspace destruction; the
other action types exist but nothing issues them yet.

The important limit: this gates what the *daemon* does, not what the agent
does. It cannot intercept an agent running `git push --force` in its own
process, and reading a command line to guess intent is not a boundary. Making
an agent action approvable means not giving the sandbox the capability in the
first place.

## Reporting a vulnerability

Open a security advisory on the repository. Anclave is pre-1.0 and its
protocol and configuration formats change without ceremony; the security
properties above are the ones worth reporting against.
