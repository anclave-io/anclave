//! Containment via podman.
//!
//! Weaker than a per-session VM: podman containers share one kernel, so a
//! kernel bug is a full escape: and stronger in the one dimension that
//! matters most in practice today: podman can actually remove the network.
//! Apple's `container` cannot, so a profile asking for `network = "none"` is
//! honored here and refused there.
//!
//! Rootless by default, which is why the hardening flags below are worth
//! having anyway: dropping every capability and refusing privilege escalation
//! costs an agent nothing it legitimately needs.

use crate::oci::OciRuntime;
use crate::sandbox::{
    CommandSpec, ProcessHandle, Sandbox, SandboxError, SandboxHandle, SandboxRequest,
};
use anclave_protocol::Size;

/// Flags applied to every container.
///
/// `no-new-privileges` stops a setuid binary inside the image from raising
/// privilege, and dropping all capabilities removes things an agent has no
/// business with (raw sockets, mounting, changing system time). Neither
/// restricts anything a coding agent legitimately does.
const HARDENING: &[&str] = &["--cap-drop=ALL", "--security-opt=no-new-privileges"];

#[derive(Debug, Clone)]
pub struct PodmanSandbox {
    inner: OciRuntime,
}

impl Default for PodmanSandbox {
    fn default() -> Self {
        Self::new("podman")
    }
}

impl PodmanSandbox {
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            inner: OciRuntime {
                program: program.into(),
                network_isolation: true,
                hardening: HARDENING,
                description: "podman: shares the host kernel; can remove the network",
            },
        }
    }
}

impl Sandbox for PodmanSandbox {
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
        "podman: shares the host kernel; can remove the network"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::CONTAINED_WORKSPACE;
    use crate::{
        ApprovalPolicy, CredentialPolicy, FilesystemPolicy, NetworkPolicy, PersistencePolicy,
        SandboxKind, SecurityProfile,
    };
    use anclave_protocol::SessionId;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn workspace() -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "anclave-podman-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn profile(network: NetworkPolicy) -> SecurityProfile {
        SecurityProfile {
            sandbox: SandboxKind::Container,
            runtime: None,
            image: Some("anclave/agent:latest".to_owned()),
            filesystem: FilesystemPolicy::Workspace,
            network,
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
            environment: BTreeMap::from([("TERM".to_owned(), "xterm".to_owned())]),
            working_directory: PathBuf::from(CONTAINED_WORKSPACE),
        }
    }

    /// The capability Apple's runtime does not have, and the reason this
    /// backend exists.
    #[test]
    fn a_disabled_network_is_honored_rather_than_refused() {
        let path = workspace();
        let sandbox = PodmanSandbox::default();
        let handle = sandbox
            .prepare(&request(profile(NetworkPolicy::None), path.clone()))
            .unwrap();
        let argv = sandbox.wrap(&handle, &command()).unwrap();

        let network_at = argv
            .iter()
            .position(|a| a == "--network")
            .expect("--network");
        assert_eq!(argv[network_at + 1], "none");
        let _ = std::fs::remove_dir_all(path);
    }

    #[test]
    fn full_network_emits_no_network_flag() {
        let path = workspace();
        let sandbox = PodmanSandbox::default();
        let handle = sandbox
            .prepare(&request(profile(NetworkPolicy::Full), path.clone()))
            .unwrap();
        let argv = sandbox.wrap(&handle, &command()).unwrap();
        assert!(!argv.contains(&"--network".to_owned()));
        let _ = std::fs::remove_dir_all(path);
    }

    /// Podman cannot express "these hosts and no others", so it must not
    /// accept a profile that asks for one.
    #[test]
    fn an_allowlist_is_refused_because_nothing_here_can_express_one() {
        let path = workspace();
        for network in [
            NetworkPolicy::Allowlist(vec!["crates.io".to_owned()]),
            NetworkPolicy::ProxyOnly,
        ] {
            assert!(matches!(
                PodmanSandbox::default().prepare(&request(profile(network), path.clone())),
                Err(SandboxError::Unsupported(_))
            ));
        }
        let _ = std::fs::remove_dir_all(path);
    }

    #[test]
    fn every_container_is_hardened() {
        let path = workspace();
        let sandbox = PodmanSandbox::default();
        let handle = sandbox
            .prepare(&request(profile(NetworkPolicy::Full), path.clone()))
            .unwrap();
        let argv = sandbox.wrap(&handle, &command()).unwrap();
        assert!(argv.contains(&"--cap-drop=ALL".to_owned()));
        assert!(argv.contains(&"--security-opt=no-new-privileges".to_owned()));
        assert!(!argv.contains(&"--ssh".to_owned()));
        let _ = std::fs::remove_dir_all(path);
    }

    #[test]
    fn the_workspace_is_mounted_and_the_agent_runs_last() {
        let path = workspace();
        let sandbox = PodmanSandbox::default();
        let handle = sandbox
            .prepare(&request(profile(NetworkPolicy::Full), path.clone()))
            .unwrap();
        let argv = sandbox.wrap(&handle, &command()).unwrap();

        assert_eq!(argv[0], "podman");
        assert!(argv.contains(&format!("{}:{CONTAINED_WORKSPACE}", path.display())));
        let image_at = argv
            .iter()
            .position(|a| a == "anclave/agent:latest")
            .unwrap();
        assert_eq!(argv[image_at + 1], "claude");
        assert_eq!(argv[image_at + 2], "--resume");
        let _ = std::fs::remove_dir_all(path);
    }

    #[test]
    fn a_read_only_policy_locks_the_root_filesystem_too() {
        let path = workspace();
        let profile = SecurityProfile {
            filesystem: FilesystemPolicy::WorkspaceReadOnly,
            ..profile(NetworkPolicy::None)
        };
        let sandbox = PodmanSandbox::default();
        let handle = sandbox.prepare(&request(profile, path.clone())).unwrap();
        let argv = sandbox.wrap(&handle, &command()).unwrap();
        assert!(argv.contains(&"--read-only".to_owned()));
        assert!(argv.contains(&format!("{}:{CONTAINED_WORKSPACE}:ro", path.display())));
        let _ = std::fs::remove_dir_all(path);
    }
}
