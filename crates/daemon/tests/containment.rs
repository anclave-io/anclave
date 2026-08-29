//! Containment, exercised against a real container runtime.
//!
//! The unit tests assert the argv Anclave *builds*. This asserts what the
//! runtime then *does* with it — the two are different claims, and only the
//! second one is the product's promise. It builds the command through the
//! real `PodmanSandbox`, so a change that weakens the argv fails here rather
//! than passing a hand-written command nobody maintains.
//!
//! Skipped when podman cannot run, in the same shape as the tmux tests: CI
//! installs podman on Linux, where it needs no VM.

use std::path::PathBuf;
use std::process::Command;

use anclave_protocol::{SessionId, Size};
use anclave_security::sandbox::{CommandSpec, Sandbox, SandboxRequest};
use anclave_security::{
    ApprovalPolicy, CredentialPolicy, FilesystemPolicy, NetworkPolicy, PersistencePolicy,
    SandboxKind, SecurityProfile,
};

const IMAGE: &str = "docker.io/library/alpine:latest";

/// A podman command that ignores the host's registry credentials.
///
/// A broken `credsStore` in `~/.docker/config.json` — a stale `gcloud` helper,
/// say — makes podman fail on *any* image reference, including one already
/// pulled. An empty auth file keeps the test measuring Anclave rather than
/// the machine's registry logins. Public images need no credentials.
fn podman() -> Command {
    let auth = std::env::temp_dir().join("anclave-test-registry-auth.json");
    if !auth.exists() {
        let _ = std::fs::write(&auth, "{}");
    }
    let mut command = Command::new("podman");
    command.env("REGISTRY_AUTH_FILE", &auth);
    command
}

/// Whether podman can actually run a container here.
///
/// `--version` is not enough: on macOS podman is installed long before a VM
/// exists, and a test that ran the argv against a dead socket would fail for
/// a reason that has nothing to do with Anclave.
fn podman_ready() -> bool {
    podman()
        .args(["info", "--format", "{{.Host.Arch}}"])
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn image_present() -> bool {
    podman()
        .args(["image", "exists", IMAGE])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
        || podman()
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

fn profile(network: NetworkPolicy) -> SecurityProfile {
    SecurityProfile {
        sandbox: SandboxKind::Container,
        runtime: Some("podman".to_owned()),
        image: Some(IMAGE.to_owned()),
        filesystem: FilesystemPolicy::Workspace,
        network,
        credentials: CredentialPolicy::None,
        approval: ApprovalPolicy::Anclave,
        persistence: PersistencePolicy::Workspace,
    }
}

/// Build the command through the real sandbox and run it.
fn run_contained(label: &str, network: NetworkPolicy, script: &str) -> String {
    let workspace = mountable_workspace(label);
    let sandbox = anclave_security::podman::PodmanSandbox::default();
    let request = SandboxRequest {
        session: SessionId::new(format!("test-{label}")).unwrap(),
        profile: profile(network),
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

    let mut runner = podman();
    let output = runner.args(&argv[1..]).output().expect("podman runs");
    let _ = std::fs::remove_dir_all(&workspace);

    // Surface stderr on failure. Swallowing it turned every runtime problem
    // into an empty string and an assertion that could not say why.
    if !output.status.success() {
        panic!(
            "podman exited {}: {}\nargv: {argv:?}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The product's central claim, against a real runtime: an agent under a
/// no-network profile cannot reach the network, and one without that policy
/// can. The control is what makes the result mean anything — without it,
/// "the request failed" could be DNS, the runner, or anything else.
#[test]
fn a_no_network_profile_actually_removes_the_network() {
    if !podman_ready() || !image_present() {
        return;
    }

    let script = "ip -o addr | awk '{print $2}' | sort -u | tr '\\n' ' '";

    let open = run_contained("open", NetworkPolicy::Full, script);
    let closed = run_contained("closed", NetworkPolicy::None, script);

    assert!(
        open.contains("eth"),
        "the control must have a network interface, got: {open:?}"
    );
    assert!(
        !closed.contains("eth"),
        "a no-network profile must leave no interface but loopback, got: {closed:?}"
    );
    assert!(
        closed.contains("lo"),
        "loopback should remain, got: {closed:?}"
    );
}

/// Credentials in the daemon's environment must not survive into the agent.
#[test]
fn planted_credentials_do_not_reach_a_contained_agent() {
    if !podman_ready() || !image_present() {
        return;
    }
    std::env::set_var("ANCLAVE_TEST_LEAK_TOKEN", "leaked-value");
    let seen = run_contained(
        "creds",
        NetworkPolicy::None,
        "env | grep -c leaked-value || true",
    );
    std::env::remove_var("ANCLAVE_TEST_LEAK_TOKEN");
    assert!(
        seen.trim().starts_with('0'),
        "a credential reached the agent: {seen:?}"
    );
}

/// The workspace is mounted, and at the fixed path rather than wherever it
/// happens to live on the host.
#[test]
fn the_workspace_is_mounted_at_a_fixed_path() {
    if !podman_ready() || !image_present() {
        return;
    }
    let seen = run_contained("mount", NetworkPolicy::None, "pwd; ls -d /workspace");
    assert!(seen.contains("/workspace"), "got: {seen:?}");
}
