//! Containment via docker.
//!
//! Mechanically the closest sibling of the podman backend — same OCI command
//! line, same `--network none` — with one difference that matters for a
//! security posture and is not visible in the flags: **the docker daemon
//! normally runs as root**. A container escape there lands as root on the
//! host, where rootless podman lands as the user. Both are `Kernel`
//! isolation, and they are not equally forgiving of a kernel bug, which is
//! why the catalogue ranks podman first and this type says so out loud.
//!
//! Offered anyway because it is what many machines actually have, and a
//! containment story nobody can run is not a containment story.

use crate::oci::OciRuntime;
use crate::sandbox::{
    CommandSpec, ProcessHandle, Sandbox, SandboxError, SandboxHandle, SandboxRequest,
};
use anclave_protocol::Size;

/// Flags applied to every container.
///
/// `no-new-privileges:true` is docker's documented spelling; podman accepts
/// the bare form. Keeping the two sets separate is the point of making
/// hardening a per-runtime field rather than something shared — a flag that
/// silently does nothing on one runtime is exactly the kind of decoration
/// this codebase refuses.
const HARDENING: &[&str] = &["--cap-drop=ALL", "--security-opt=no-new-privileges:true"];

#[derive(Debug, Clone)]
pub struct DockerSandbox {
    inner: OciRuntime,
}

impl Default for DockerSandbox {
    fn default() -> Self {
        Self::new("docker")
    }
}

impl DockerSandbox {
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            inner: OciRuntime {
                program: program.into(),
                network_isolation: true,
                hardening: HARDENING,
                description: "docker: shares the host kernel; daemon usually runs as root",
            },
        }
    }
}

impl Sandbox for DockerSandbox {
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
        "docker: shares the host kernel; the daemon usually runs as root"
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
            "anclave-docker-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn profile(network: NetworkPolicy) -> SecurityProfile {
        SecurityProfile {
            sandbox: SandboxKind::Container,
            runtime: Some("docker".to_owned()),
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

    #[test]
    fn a_disabled_network_is_honoured() {
        let path = workspace();
        let sandbox = DockerSandbox::default();
        let handle = sandbox
            .prepare(&request(profile(NetworkPolicy::None), path.clone()))
            .unwrap();
        let argv = sandbox.wrap(&handle, &command()).unwrap();
        let at = argv
            .iter()
            .position(|a| a == "--network")
            .expect("--network");
        assert_eq!(argv[at + 1], "none");
        let _ = std::fs::remove_dir_all(path);
    }

    /// Docker's own spelling, not podman's. A hardening flag the runtime
    /// silently ignores is worse than none, because it reads as applied.
    #[test]
    fn hardening_uses_the_spelling_docker_documents() {
        let path = workspace();
        let sandbox = DockerSandbox::default();
        let handle = sandbox
            .prepare(&request(profile(NetworkPolicy::Full), path.clone()))
            .unwrap();
        let argv = sandbox.wrap(&handle, &command()).unwrap();
        assert!(argv.contains(&"--cap-drop=ALL".to_owned()));
        assert!(argv.contains(&"--security-opt=no-new-privileges:true".to_owned()));
        let _ = std::fs::remove_dir_all(path);
    }

    #[test]
    fn the_workspace_is_mounted_and_the_agent_runs_last() {
        let path = workspace();
        let sandbox = DockerSandbox::default();
        let handle = sandbox
            .prepare(&request(profile(NetworkPolicy::Full), path.clone()))
            .unwrap();
        let argv = sandbox.wrap(&handle, &command()).unwrap();
        assert_eq!(argv[0], "docker");
        assert!(argv.contains(&format!("{}:{CONTAINED_WORKSPACE}", path.display())));
        let image_at = argv
            .iter()
            .position(|a| a == "anclave/agent:latest")
            .unwrap();
        assert_eq!(argv[image_at + 1], "claude");
        assert!(!argv.contains(&"--ssh".to_owned()));
        let _ = std::fs::remove_dir_all(path);
    }

    #[test]
    fn an_allowlist_is_refused_like_every_other_runtime() {
        let path = workspace();
        assert!(matches!(
            DockerSandbox::default().prepare(&request(
                profile(NetworkPolicy::Allowlist(vec!["crates.io".to_owned()])),
                path.clone()
            )),
            Err(SandboxError::Unsupported(_))
        ));
        let _ = std::fs::remove_dir_all(path);
    }

    #[test]
    fn a_read_only_policy_locks_the_root_filesystem() {
        let path = workspace();
        let profile = SecurityProfile {
            filesystem: FilesystemPolicy::WorkspaceReadOnly,
            ..profile(NetworkPolicy::None)
        };
        let sandbox = DockerSandbox::default();
        let handle = sandbox.prepare(&request(profile, path.clone())).unwrap();
        let argv = sandbox.wrap(&handle, &command()).unwrap();
        assert!(argv.contains(&"--read-only".to_owned()));
        assert!(argv.contains(&format!("{}:{CONTAINED_WORKSPACE}:ro", path.display())));
        let _ = std::fs::remove_dir_all(path);
    }
}
