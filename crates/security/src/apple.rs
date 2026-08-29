//! Containment via Apple's `container` runtime.
//!
//! Each container runs in its own lightweight VM on Apple silicon, so this is
//! `Isolation::Machine` — a separate kernel, not shared namespaces.
//!
//! **What this runtime cannot do today.** `container` 1.3.0 exposes no
//! `--network none`: `--network` takes a network *name*, and `--no-dns` only
//! withholds resolver configuration, which is not a network boundary. So a
//! profile asking for a restricted network is **refused** here rather than
//! accepted and quietly ignored. That refusal is the whole discipline — a
//! policy that appears applied and enforces nothing is worse than one that
//! fails loudly at startup.

use std::path::PathBuf;

use crate::sandbox::{
    CommandSpec, ProcessHandle, Sandbox, SandboxError, SandboxHandle, SandboxRequest,
    CONTAINED_WORKSPACE,
};
use crate::{FilesystemPolicy, NetworkPolicy, SandboxKind};
use anclave_protocol::Size;

/// Drives the `container` CLI.
#[derive(Debug, Clone)]
pub struct AppleContainerSandbox {
    /// The executable, so tests and unusual installs can point elsewhere.
    program: String,
}

impl Default for AppleContainerSandbox {
    fn default() -> Self {
        Self {
            program: "container".to_owned(),
        }
    }
}

impl AppleContainerSandbox {
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
        }
    }

    fn container_name(session: &anclave_protocol::SessionId) -> String {
        // Deterministic, so a restart addresses the same container and a
        // leaked one is identifiable rather than anonymous.
        format!("anclave-{session}")
    }
}

impl Sandbox for AppleContainerSandbox {
    fn prepare(&self, request: &SandboxRequest) -> Result<SandboxHandle, SandboxError> {
        if !request.profile.sandbox.contains() {
            return Err(SandboxError::Unsupported(
                "a host profile — use HostSandbox",
            ));
        }
        // The refusal that matters: this runtime has no way to remove the
        // network, so it must not accept a profile that asks for it.
        if request.profile.network != NetworkPolicy::Full {
            return Err(SandboxError::Unsupported(
                "a restricted network (this runtime exposes no network isolation)",
            ));
        }
        if request.profile.image.is_none() {
            return Err(SandboxError::StartupFailed(
                "the profile names no image".to_owned(),
            ));
        }
        if !request.workspace.exists() {
            return Err(SandboxError::StartupFailed(format!(
                "workspace does not exist: {}",
                request.workspace.display()
            )));
        }
        Ok(SandboxHandle {
            id: Self::container_name(&request.session),
            kind: SandboxKind::Container,
            // Relocated: where the workspace sits on the host is not a fact
            // the agent gets to learn.
            workspace: PathBuf::from(CONTAINED_WORKSPACE),
        })
    }

    fn wrap(
        &self,
        sandbox: &SandboxHandle,
        command: &CommandSpec,
    ) -> Result<Vec<String>, SandboxError> {
        if command.program.is_empty() {
            return Err(SandboxError::StartupFailed("no program".to_owned()));
        }
        let image = command
            .environment
            .get("ANCLAVE_IMAGE")
            .cloned()
            .ok_or_else(|| SandboxError::StartupFailed("no image resolved".to_owned()))?;

        let mut argv = vec![
            self.program.clone(),
            "run".to_owned(),
            // Removed when it stops: a session's container must not outlive
            // the session and accumulate.
            "--rm".to_owned(),
            "-i".to_owned(),
            "-t".to_owned(),
            "--name".to_owned(),
            sandbox.id.clone(),
            "-w".to_owned(),
            sandbox.workspace.to_string_lossy().into_owned(),
        ];

        // The environment is passed explicitly, one flag per variable. Note
        // what is *absent*: `--ssh`, which would forward the SSH agent socket
        // into the container and undo the credential policy in one flag.
        for (name, value) in &command.environment {
            if name == "ANCLAVE_IMAGE" {
                continue;
            }
            argv.push("-e".to_owned());
            argv.push(format!("{name}={value}"));
        }

        argv.push(image);
        argv.push(command.program.clone());
        argv.extend(command.args.iter().cloned());
        Ok(argv)
    }

    fn spawn(
        &self,
        sandbox: &SandboxHandle,
        command: &CommandSpec,
    ) -> Result<ProcessHandle, SandboxError> {
        // Like the host sandbox, this does not start the process: the session
        // backend owns the pty. `wrap` is what makes the containment real.
        self.wrap(sandbox, command)?;
        Ok(ProcessHandle {
            sandbox: sandbox.id.clone(),
            reference: sandbox.id.clone(),
        })
    }

    fn resize(&self, _sandbox: &SandboxHandle, size: Size) -> Result<(), SandboxError> {
        // The pty belongs to the session backend, which resizes it; the
        // container inherits that size through the tty it was given.
        size.validate()
            .map(|_| ())
            .map_err(|_| SandboxError::Failed("invalid terminal size".to_owned()))
    }

    fn destroy(&self, sandbox: SandboxHandle) -> Result<(), SandboxError> {
        // Best-effort: `--rm` already removes it on exit, so this only cleans
        // up a container whose process died without stopping it. A failure
        // here must not mask whatever caused the teardown.
        let _ = std::process::Command::new(&self.program)
            .args(["delete", "--force", &sandbox.id])
            .output();
        Ok(())
    }

    fn describe(&self) -> &'static str {
        "apple container: separate kernel per session; no network isolation available"
    }
}

/// Build the mount arguments for a workspace.
///
/// Separated so the read-only decision is testable on its own: it is the
/// difference between an agent that can change your code and one that cannot.
pub fn mount_arguments(filesystem: FilesystemPolicy, host_workspace: &str) -> Vec<String> {
    let suffix = match filesystem {
        FilesystemPolicy::WorkspaceReadOnly => ":ro",
        FilesystemPolicy::Workspace | FilesystemPolicy::Host => "",
    };
    vec![
        "-v".to_owned(),
        format!("{host_workspace}:{CONTAINED_WORKSPACE}{suffix}"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ApprovalPolicy, CredentialPolicy, PersistencePolicy, SecurityProfile};
    use anclave_protocol::SessionId;
    use std::collections::BTreeMap;

    fn workspace() -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "anclave-apple-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn contained() -> SecurityProfile {
        SecurityProfile {
            sandbox: SandboxKind::Container,
            image: Some("anclave/agent:latest".to_owned()),
            filesystem: FilesystemPolicy::Workspace,
            network: NetworkPolicy::Full,
            credentials: CredentialPolicy::None,
            approval: ApprovalPolicy::Anclave,
            persistence: PersistencePolicy::Workspace,
        }
    }

    fn request(profile: SecurityProfile, workspace: PathBuf) -> SandboxRequest {
        SandboxRequest {
            session: SessionId::new("session-1").unwrap(),
            profile,
            workspace,
            size: Size {
                columns: 80,
                rows: 24,
            },
        }
    }

    fn command() -> CommandSpec {
        CommandSpec {
            program: "claude".to_owned(),
            args: vec!["--resume".to_owned()],
            environment: BTreeMap::from([
                (
                    "ANCLAVE_IMAGE".to_owned(),
                    "anclave/agent:latest".to_owned(),
                ),
                ("PATH".to_owned(), "/usr/bin".to_owned()),
            ]),
            working_directory: PathBuf::from(CONTAINED_WORKSPACE),
        }
    }

    /// The refusal this backend exists to demonstrate: it cannot remove the
    /// network, so it must not accept a profile that asks it to.
    #[test]
    fn a_restricted_network_is_refused_rather_than_ignored() {
        let path = workspace();
        for network in [
            NetworkPolicy::None,
            NetworkPolicy::ProxyOnly,
            NetworkPolicy::Allowlist(vec!["example.test".to_owned()]),
        ] {
            let profile = SecurityProfile {
                network,
                ..contained()
            };
            assert!(
                matches!(
                    AppleContainerSandbox::default().prepare(&request(profile, path.clone())),
                    Err(SandboxError::Unsupported(_))
                ),
                "a network restriction must be refused"
            );
        }
        let _ = std::fs::remove_dir_all(path);
    }

    #[test]
    fn the_workspace_is_relocated_inside_the_container() {
        let path = workspace();
        let handle = AppleContainerSandbox::default()
            .prepare(&request(contained(), path.clone()))
            .unwrap();
        assert_eq!(handle.workspace, PathBuf::from(CONTAINED_WORKSPACE));
        assert_ne!(handle.workspace, path);
        assert!(handle.contains());
        let _ = std::fs::remove_dir_all(path);
    }

    #[test]
    fn the_wrapped_argv_runs_the_agent_inside_the_image() {
        let path = workspace();
        let sandbox = AppleContainerSandbox::default();
        let handle = sandbox
            .prepare(&request(contained(), path.clone()))
            .unwrap();
        let argv = sandbox.wrap(&handle, &command()).unwrap();

        assert_eq!(argv[0], "container");
        assert_eq!(argv[1], "run");
        assert!(argv.contains(&"--rm".to_owned()));
        assert!(argv.contains(&"anclave/agent:latest".to_owned()));

        // The agent and its arguments come last, after the image.
        let image_at = argv
            .iter()
            .position(|a| a == "anclave/agent:latest")
            .unwrap();
        assert_eq!(argv[image_at + 1], "claude");
        assert_eq!(argv[image_at + 2], "--resume");
        let _ = std::fs::remove_dir_all(path);
    }

    /// One flag would undo the entire credential policy.
    #[test]
    fn the_ssh_agent_is_never_forwarded() {
        let path = workspace();
        let sandbox = AppleContainerSandbox::default();
        let handle = sandbox
            .prepare(&request(contained(), path.clone()))
            .unwrap();
        let argv = sandbox.wrap(&handle, &command()).unwrap();
        assert!(
            !argv.contains(&"--ssh".to_owned()),
            "forwarding the SSH agent would undo the credential policy"
        );
        let _ = std::fs::remove_dir_all(path);
    }

    #[test]
    fn the_environment_is_passed_explicitly_and_the_image_marker_is_not() {
        let path = workspace();
        let sandbox = AppleContainerSandbox::default();
        let handle = sandbox
            .prepare(&request(contained(), path.clone()))
            .unwrap();
        let argv = sandbox.wrap(&handle, &command()).unwrap();
        assert!(argv.contains(&"PATH=/usr/bin".to_owned()));
        assert!(
            !argv.iter().any(|a| a.starts_with("ANCLAVE_IMAGE=")),
            "the image marker is plumbing, not part of the agent's environment"
        );
        let _ = std::fs::remove_dir_all(path);
    }

    #[test]
    fn a_read_only_policy_produces_a_read_only_mount() {
        assert_eq!(
            mount_arguments(FilesystemPolicy::WorkspaceReadOnly, "/host/ws"),
            vec![
                "-v".to_owned(),
                format!("/host/ws:{CONTAINED_WORKSPACE}:ro")
            ]
        );
        assert_eq!(
            mount_arguments(FilesystemPolicy::Workspace, "/host/ws"),
            vec!["-v".to_owned(), format!("/host/ws:{CONTAINED_WORKSPACE}")]
        );
    }

    #[test]
    fn a_host_profile_belongs_to_the_host_sandbox() {
        let path = workspace();
        assert!(matches!(
            AppleContainerSandbox::default()
                .prepare(&request(SecurityProfile::host(), path.clone())),
            Err(SandboxError::Unsupported(_))
        ));
        let _ = std::fs::remove_dir_all(path);
    }

    #[test]
    fn the_container_name_is_derived_from_the_session() {
        let path = workspace();
        let handle = AppleContainerSandbox::default()
            .prepare(&request(contained(), path.clone()))
            .unwrap();
        assert_eq!(handle.id, "anclave-session-1");
        let _ = std::fs::remove_dir_all(path);
    }
}
