//! Containment, exercised against a real container runtime.
//!
//! The unit tests assert the argv Anclave *builds*. This asserts what the
//! runtime then *does* with it — the two are different claims, and only the
//! second one is the product's promise. It builds the command through the
//! real `PodmanSandbox`, so a change that weakens the argv fails here rather
//! than passing a hand-written command nobody maintains.
//!
//! Runs against **every** container runtime present, not a chosen one. The
//! backends differ in ways only a real runtime can check — docker and podman
//! spell `no-new-privileges` differently, and a hardening flag a runtime
//! silently ignores reads as applied while doing nothing. Skipped entirely
//! when no runtime can run, in the same shape as the tmux tests.

use std::path::PathBuf;
use std::process::Command;

use anclave_protocol::{SessionId, Size};
use anclave_security::sandbox::{CommandSpec, Sandbox, SandboxRequest};
use anclave_security::{
    ApprovalPolicy, CredentialPolicy, FilesystemPolicy, NetworkPolicy, PersistencePolicy,
    SandboxKind, SecurityProfile,
};

const IMAGE: &str = "docker.io/library/alpine:latest";

/// A container runtime this test knows how to drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rt {
    Podman,
    Docker,
}

impl Rt {
    fn program(self) -> &'static str {
        match self {
            Self::Podman => "podman",
            Self::Docker => "docker",
        }
    }

    fn sandbox(self) -> Box<dyn Sandbox> {
        match self {
            Self::Podman => Box::new(anclave_security::podman::PodmanSandbox::default()),
            Self::Docker => Box::new(anclave_security::docker::DockerSandbox::default()),
        }
    }

    fn profile_name(self) -> &'static str {
        match self {
            Self::Podman => "podman",
            Self::Docker => "docker",
        }
    }
}

/// Every runtime that can actually start a container here.
fn available() -> Vec<Rt> {
    [Rt::Podman, Rt::Docker]
        .into_iter()
        .filter(|runtime| ready(*runtime) && image_present(*runtime))
        .collect()
}

/// A runtime command that ignores the host's registry credentials.
///
/// A broken `credsStore` in `~/.docker/config.json` — a stale `gcloud` helper,
/// say — makes podman fail on *any* image reference, including one already
/// pulled. An empty auth file keeps the test measuring Anclave rather than
/// the machine's registry logins. Public images need no credentials.
fn runtime_command(runtime: Rt) -> Command {
    let auth = std::env::temp_dir().join("anclave-test-registry-auth.json");
    if !auth.exists() {
        let _ = std::fs::write(&auth, "{}");
    }
    let mut command = Command::new(runtime.program());
    command.env("REGISTRY_AUTH_FILE", &auth);
    command
}

/// Whether this runtime can actually start a container.
///
/// `--version` is not enough: on macOS both runtimes are installed long
/// before a VM or daemon exists, and a test that ran the argv against a dead
/// socket would fail for a reason that has nothing to do with Anclave.
fn ready(runtime: Rt) -> bool {
    runtime_command(runtime)
        .args(["info", "--format", "{{.OSType}}"])
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn image_present(runtime: Rt) -> bool {
    // `image exists` is podman-only, so fall back to pulling, which both do.
    runtime_command(runtime)
        .args(["image", "inspect", IMAGE])
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
        || runtime_command(runtime)
            .args(["pull", "-q", IMAGE])
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
}

/// A workspace the container runtime can actually mount.
///
/// On macOS podman's VM shares `$HOME` and *not* `/tmp`, so a workspace under
/// the system temp dir fails with `statfs: no such file or directory`. Using
/// the home directory works on both platforms.
fn mountable_workspace(label: &str) -> PathBuf {
    let base = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_owned());
    let path = PathBuf::from(base).join(format!(
        ".anclave-test-{}-{}-{}",
        label,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&path).expect("workspace is creatable");
    path
}

fn profile(runtime: Rt, network: NetworkPolicy) -> SecurityProfile {
    SecurityProfile {
        sandbox: SandboxKind::Container,
        runtime: Some(runtime.profile_name().to_owned()),
        image: Some(IMAGE.to_owned()),
        filesystem: FilesystemPolicy::Workspace,
        network,
        credentials: CredentialPolicy::None,
        approval: ApprovalPolicy::Anclave,
        persistence: PersistencePolicy::Workspace,
    }
}

/// Build the command through the real sandbox and run it.
fn run_contained(runtime: Rt, label: &str, network: NetworkPolicy, script: &str) -> String {
    let workspace = mountable_workspace(label);
    let sandbox = runtime.sandbox();
    let request = SandboxRequest {
        session: SessionId::new(format!("test-{label}-{}", runtime.program())).unwrap(),
        profile: profile(runtime, network),
        workspace: workspace.clone(),
        size: Size {
            columns: 80,
            rows: 24,
        },
    };
    let handle = sandbox.prepare(&request).expect("sandbox prepares");
    let command = CommandSpec {
        program: "sh".to_owned(),
        args: vec!["-c".to_owned(), script.to_owned()],
        environment: std::collections::BTreeMap::from([(
            "ANCLAVE_SESSION".to_owned(),
            format!("test-{label}"),
        )]),
        working_directory: handle.workspace.clone(),
    };
    let argv = sandbox.wrap(&handle, &command).expect("argv builds");

    // `-i -t` needs a terminal the test harness does not have.
    let argv: Vec<String> = argv
        .into_iter()
        .filter(|arg| arg != "-i" && arg != "-t")
        .collect();

    let mut runner = runtime_command(runtime);
    let output = runner
        .args(&argv[1..])
        .output()
        .unwrap_or_else(|error| panic!("{} could not run: {error}", runtime.program()));
    let _ = std::fs::remove_dir_all(&workspace);

    // Surface stderr on failure. Swallowing it turned every runtime problem
    // into an empty string and an assertion that could not say why.
    if !output.status.success() {
        panic!(
            "{} exited {}: {}\nargv: {argv:?}",
            runtime.program(),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The product's central claim, against every real runtime present: an agent
/// under a no-network profile cannot reach the network, and one without that
/// policy can. The control is what makes the result mean anything — without
/// it, "no interface" could be a broken runtime rather than a working policy.
#[test]
fn a_no_network_profile_actually_removes_the_network() {
    let runtimes = available();
    if runtimes.is_empty() {
        return;
    }
    let script = "ip -o addr | awk '{print $2}' | sort -u | tr '\\n' ' '";

    for runtime in runtimes {
        let open = run_contained(runtime, "open", NetworkPolicy::Full, script);
        let closed = run_contained(runtime, "closed", NetworkPolicy::None, script);
        let name = runtime.program();

        assert!(
            open.contains("eth"),
            "{name}: the control must have a network interface, got {open:?}"
        );
        assert!(
            !closed.contains("eth"),
            "{name}: a no-network profile must leave nothing but loopback, got {closed:?}"
        );
        assert!(closed.contains("lo"), "{name}: loopback should remain");
    }
}

/// Credentials in the daemon's environment must not survive into the agent.
#[test]
fn planted_credentials_do_not_reach_a_contained_agent() {
    let runtimes = available();
    if runtimes.is_empty() {
        return;
    }
    std::env::set_var("ANCLAVE_TEST_LEAK_TOKEN", "leaked-value");
    for runtime in runtimes {
        let seen = run_contained(
            runtime,
            "creds",
            NetworkPolicy::None,
            "env | grep -c leaked-value || true",
        );
        assert!(
            seen.trim().starts_with('0'),
            "{}: a credential reached the agent: {seen:?}",
            runtime.program()
        );
    }
    std::env::remove_var("ANCLAVE_TEST_LEAK_TOKEN");
}

/// The workspace is mounted, at the fixed path rather than wherever it
/// happens to live on the host.
#[test]
fn the_workspace_is_mounted_at_a_fixed_path() {
    let runtimes = available();
    if runtimes.is_empty() {
        return;
    }
    for runtime in runtimes {
        let seen = run_contained(
            runtime,
            "mount",
            NetworkPolicy::None,
            "pwd; ls -d /workspace",
        );
        assert!(
            seen.contains("/workspace"),
            "{}: got {seen:?}",
            runtime.program()
        );
    }
}

/// Each backend's hardening flags must be spelled the way *its own* runtime
/// accepts. A rejected flag fails the container outright; a silently ignored
/// one is worse, because it reads as applied. Only a live runtime can tell
/// the two apart from a correct one.
#[test]
fn hardening_flags_are_accepted_by_the_runtime_that_gets_them() {
    let runtimes = available();
    if runtimes.is_empty() {
        return;
    }
    for runtime in runtimes {
        // Reaching this without a panic means the runtime accepted every
        // flag the backend emitted, hardening included.
        let seen = run_contained(runtime, "harden", NetworkPolicy::None, "echo started");
        assert!(
            seen.contains("started"),
            "{}: container did not start: {seen:?}",
            runtime.program()
        );
    }
}
