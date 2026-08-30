//! Containment via Apple's `container` runtime.
//!
//! Each container runs in its own lightweight VM on Apple silicon, so this is
//! `Isolation::Machine` — a separate kernel, not shared namespaces. Verified
//! against a real agent: the host reports `Darwin`, the contained agent
//! reports `Linux`.
//!
//! **What this runtime cannot do today.** `container` 1.3.0 exposes no
//! `--network none`: `--network` takes a network *name*, and `--no-dns` only
//! withholds resolver configuration, which is not a network boundary. So a
//! profile asking for a restricted network is **refused** here rather than
//! accepted and quietly ignored — see `podman` for a runtime that can honour
//! one. That refusal is the whole discipline: a policy that appears applied
//! and enforces nothing is worse than one that fails loudly at startup.

use crate::oci::OciRuntime;
use crate::sandbox::{
    CommandSpec, ProcessHandle, Sandbox, SandboxError, SandboxHandle, SandboxRequest,
};
use crate::FilesystemPolicy;
use anclave_protocol::Size;

/// Drives the `container` CLI.
#[derive(Debug, Clone)]
pub struct AppleContainerSandbox {
    inner: OciRuntime,
}

impl Default for AppleContainerSandbox {
    fn default() -> Self {
        Self::new("container")
    }
}

impl AppleContainerSandbox {
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            inner: OciRuntime {
                program: program.into(),
                // The one capability this runtime lacks.
                network_isolation: false,
                // `container` 1.3.0 has --cap-drop but not --security-opt, so
                // the hardening set is deliberately smaller than podman's.
                hardening: &["--cap-drop=ALL"],
                description: "apple container: separate kernel per session",
            },
        }
    }
}

impl Sandbox for AppleContainerSandbox {
    fn prepare(&self, request: &SandboxRequest) -> Result<SandboxHandle, SandboxError> {
        self.inner.prepare(request)
    }

    fn wrap(
        &self,
        sandbox: &SandboxHandle,
        command: &CommandSpec,
    ) -> Result<Vec<String>, SandboxError> {
        self.inner.wrap(sandbox, command)
    }

    fn spawn(
        &self,
        sandbox: &SandboxHandle,
        command: &CommandSpec,
    ) -> Result<ProcessHandle, SandboxError> {
        self.inner.spawn(sandbox, command)
    }

    fn resize(&self, _sandbox: &SandboxHandle, size: Size) -> Result<(), SandboxError> {
        self.inner.resize(size)
    }

    fn destroy(&self, sandbox: SandboxHandle) -> Result<(), SandboxError> {
        self.inner.destroy(sandbox)
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
        format!(
            "{host_workspace}:{}{suffix}",
            crate::sandbox::CONTAINED_WORKSPACE
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::CONTAINED_WORKSPACE;
    use crate::{ApprovalPolicy, CredentialPolicy, PersistencePolicy, SecurityProfile};
    use crate::{NetworkPolicy, SandboxKind};
    use anclave_protocol::SessionId;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

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
            runtime: None,
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
    fn the_environment_is_passed_explicitly() {
        let path = workspace();
        let sandbox = AppleContainerSandbox::default();
        let handle = sandbox
            .prepare(&request(contained(), path.clone()))
            .unwrap();
        let argv = sandbox.wrap(&handle, &command()).unwrap();
        assert!(argv.contains(&"PATH=/usr/bin".to_owned()));
        let _ = std::fs::remove_dir_all(path);
    }

    /// Without the mount the container starts empty and the agent looks as
    /// though it lost the repository.
    #[test]
    fn the_workspace_is_actually_mounted() {
        let path = workspace();
        let sandbox = AppleContainerSandbox::default();
        let handle = sandbox
            .prepare(&request(contained(), path.clone()))
            .unwrap();
        let argv = sandbox.wrap(&handle, &command()).unwrap();
        let expected = format!("{}:{CONTAINED_WORKSPACE}", path.display());
        assert!(argv.contains(&expected), "workspace not mounted: {argv:?}");
        let _ = std::fs::remove_dir_all(path);
    }

    #[test]
    fn a_read_only_workspace_is_mounted_read_only() {
        let path = workspace();
        let profile = SecurityProfile {
            filesystem: FilesystemPolicy::WorkspaceReadOnly,
            ..contained()
        };
        let sandbox = AppleContainerSandbox::default();
        let handle = sandbox.prepare(&request(profile, path.clone())).unwrap();
        let argv = sandbox.wrap(&handle, &command()).unwrap();
        let expected = format!("{}:{CONTAINED_WORKSPACE}:ro", path.display());
        assert!(argv.contains(&expected), "not read-only: {argv:?}");
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
