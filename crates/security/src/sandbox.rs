//! The boundary every agent launch passes through.
//!
//! The interface exists before any real containment does, and that ordering
//! is deliberate: if launching can happen *around* the sandbox, then adding a
//! real one later only covers the paths someone remembers to route through
//! it. Making `HostSandbox` an implementation rather than a special case means
//! the uncontained path and the contained one are the same path.
//!
//! Every sandbox reports what it provides. [`SandboxHandle::contains`] is the
//! single answer to "is this agent confined", and `HostSandbox` answers `false`
//! rather than staying quiet about it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anclave_protocol::{SessionId, Size};

use crate::{FilesystemPolicy, NetworkPolicy, SandboxKind, SecurityProfile};

/// What the daemon asks a sandbox to prepare.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxRequest {
    pub session: SessionId,
    pub profile: SecurityProfile,
    /// The directory the agent runs in. A contained sandbox mounts it; the
    /// host one simply uses it as a working directory.
    pub workspace: PathBuf,
    pub size: Size,
}

/// A prepared, not-yet-running sandbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxHandle {
    pub id: String,
    pub kind: SandboxKind,
    /// Where the workspace appears *inside* the sandbox. Equal to the host
    /// path for `HostSandbox`, and typically not for a container.
    pub workspace: PathBuf,
    /// What gets mounted, decided when the sandbox was prepared.
    ///
    /// `prepare` makes the decisions and `wrap` only renders them. Deciding
    /// inside `wrap` would mean the argv could disagree with the handle the
    /// daemon is holding.
    pub mounts: Vec<Mount>,
    /// The image, for runtimes that need one.
    pub image: Option<String>,
}

impl SandboxHandle {
    /// Whether this handle confines the process it spawns.
    pub fn contains(&self) -> bool {
        self.kind.contains()
    }
}

/// A command, fully specified, with nothing left to a shell.
///
/// `program` and `args` are passed as they are — never joined into a string
/// for a shell to re-split. Quoting a command back together is how an
/// argument containing a space becomes two arguments, and how a policy check
/// on "the command" stops matching what actually runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
    pub environment: BTreeMap<String, String>,
    pub working_directory: PathBuf,
}

/// A process the sandbox started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessHandle {
    pub sandbox: String,
    /// How the backend addresses this process afterwards.
    pub reference: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SandboxError {
    #[error("this sandbox cannot honour {0}")]
    Unsupported(&'static str),
    #[error("sandbox {0} is not prepared")]
    NotPrepared(String),
    #[error("sandbox failed to start: {0}")]
    StartupFailed(String),
    #[error("sandbox operation failed: {0}")]
    Failed(String),
}

/// Prepares an execution boundary, runs a command inside it, tears it down.
pub trait Sandbox: Send + Sync {
    fn prepare(&self, request: &SandboxRequest) -> Result<SandboxHandle, SandboxError>;
    fn spawn(
        &self,
        sandbox: &SandboxHandle,
        command: &CommandSpec,
    ) -> Result<ProcessHandle, SandboxError>;
    fn resize(&self, sandbox: &SandboxHandle, size: Size) -> Result<(), SandboxError>;
    fn destroy(&self, sandbox: SandboxHandle) -> Result<(), SandboxError>;

    /// The argv the session backend should actually execute.
    ///
    /// This is where containment becomes real: the backend still owns the pty
    /// and the process lifecycle, and the sandbox decides what command that
    /// pty is attached to. Returning argv rather than spawning keeps one
    /// process lifecycle per session — a sandbox that spawned its own would
    /// give each session two — and it makes containment assertable in a unit
    /// test, without a container runtime present.
    fn wrap(
        &self,
        sandbox: &SandboxHandle,
        command: &CommandSpec,
    ) -> Result<Vec<String>, SandboxError>;

    /// What this implementation actually provides, for a person reading a
    /// session's posture.
    fn describe(&self) -> &'static str;
}

/// Runs the agent on the host, contained by nothing.
///
/// This is the compatibility path and the default. It exists as a real
/// `Sandbox` so that no launch bypasses the interface — but it refuses any
/// profile whose filesystem or network policy it cannot honour, rather than
/// accepting the request and silently ignoring the restriction. Accepting
/// and ignoring is how a policy becomes decoration.
#[derive(Debug, Default, Clone, Copy)]
pub struct HostSandbox;

impl Sandbox for HostSandbox {
    fn prepare(&self, request: &SandboxRequest) -> Result<SandboxHandle, SandboxError> {
        if request.profile.filesystem != FilesystemPolicy::Host {
            return Err(SandboxError::Unsupported("a restricted filesystem"));
        }
        if request.profile.network != NetworkPolicy::Full {
            return Err(SandboxError::Unsupported("a restricted network"));
        }
        if !request.workspace.exists() {
            return Err(SandboxError::StartupFailed(format!(
                "workspace does not exist: {}",
                request.workspace.display()
            )));
        }
        Ok(SandboxHandle {
            id: format!("host-{}", request.session),
            kind: SandboxKind::Host,
            // On the host the two paths are the same, which is exactly what
            // makes this not a sandbox.
            workspace: request.workspace.clone(),
            // Nothing is mounted because nothing is separated.
            mounts: Vec::new(),
            image: None,
        })
    }

    fn spawn(
        &self,
        sandbox: &SandboxHandle,
        command: &CommandSpec,
    ) -> Result<ProcessHandle, SandboxError> {
        // The host sandbox does not start processes itself: the session
        // backend owns the pty, and duplicating that here would give one
        // session two lifecycles. It validates and hands back the reference
        // the backend uses.
        if command.program.is_empty() {
            return Err(SandboxError::StartupFailed("no program".to_owned()));
        }
        Ok(ProcessHandle {
            sandbox: sandbox.id.clone(),
            reference: command.program.clone(),
        })
    }

    fn resize(&self, _sandbox: &SandboxHandle, size: Size) -> Result<(), SandboxError> {
        size.validate()
            .map(|_| ())
            .map_err(|_| SandboxError::Failed("invalid terminal size".to_owned()))
    }

    fn destroy(&self, _sandbox: SandboxHandle) -> Result<(), SandboxError> {
        // There is nothing to tear down, and saying so is the honest answer
        // rather than pretending a cleanup happened.
        Ok(())
    }

    fn wrap(
        &self,
        _sandbox: &SandboxHandle,
        command: &CommandSpec,
    ) -> Result<Vec<String>, SandboxError> {
        // Unchanged: on the host the command runs as itself. The environment
        // is applied by the backend, which is the only restriction there is.
        let mut argv = vec![command.program.clone()];
        argv.extend(command.args.iter().cloned());
        Ok(argv)
    }

    fn describe(&self) -> &'static str {
        "host: no containment — the agent runs with your full authority"
    }
}

/// Where the workspace is mounted inside a contained sandbox.
///
/// A fixed path rather than the host's, so a workspace's location on disk is
/// not a fact the agent gets to learn.
pub const CONTAINED_WORKSPACE: &str = "/workspace";

/// Build a mount plan for a contained sandbox from a profile.
///
/// Separated from any runtime so the decision can be tested without one, and
/// so the first real backend implements *transport*, not policy.
pub fn mount_plan(profile: &SecurityProfile, workspace: &Path) -> Vec<Mount> {
    let writable = matches!(profile.filesystem, FilesystemPolicy::Host)
        || matches!(profile.filesystem, FilesystemPolicy::Workspace);
    vec![Mount {
        source: workspace.to_path_buf(),
        destination: PathBuf::from(CONTAINED_WORKSPACE),
        writable,
    }]
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    pub source: PathBuf,
    pub destination: PathBuf,
    pub writable: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ApprovalPolicy, CredentialPolicy, PersistencePolicy};

    /// A directory of this test's own. Keyed on process id alone, every test
    /// in the binary shared one path and deleted it from under the others as
    /// they ran in parallel.
    fn workspace() -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "anclave-sb-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
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

    #[test]
    fn the_host_sandbox_reports_that_it_contains_nothing() {
        let path = workspace();
        let handle = HostSandbox
            .prepare(&request(SecurityProfile::host(), path.clone()))
            .unwrap();
        assert!(!handle.contains());
        assert!(HostSandbox.describe().contains("no containment"));
        let _ = std::fs::remove_dir_all(path);
    }

    /// Accepting a restriction it cannot apply is the failure mode this whole
    /// interface exists to prevent.
    #[test]
    fn the_host_sandbox_refuses_a_restriction_it_cannot_apply() {
        let path = workspace();
        for profile in [
            SecurityProfile {
                filesystem: FilesystemPolicy::Workspace,
                sandbox: SandboxKind::Host,
                ..SecurityProfile::host()
            },
            SecurityProfile {
                network: NetworkPolicy::None,
                sandbox: SandboxKind::Host,
                ..SecurityProfile::host()
            },
        ] {
            assert!(matches!(
                HostSandbox.prepare(&request(profile, path.clone())),
                Err(SandboxError::Unsupported(_))
            ));
        }
        let _ = std::fs::remove_dir_all(path);
    }

    #[test]
    fn a_missing_workspace_fails_at_prepare_rather_than_at_spawn() {
        let request = request(
            SecurityProfile::host(),
            PathBuf::from("/definitely/not/here"),
        );
        assert!(matches!(
            HostSandbox.prepare(&request),
            Err(SandboxError::StartupFailed(_))
        ));
    }

    #[test]
    fn destroying_the_host_sandbox_is_a_no_op_that_succeeds() {
        let path = workspace();
        let handle = HostSandbox
            .prepare(&request(SecurityProfile::host(), path.clone()))
            .unwrap();
        assert!(HostSandbox.destroy(handle).is_ok());
        let _ = std::fs::remove_dir_all(path);
    }

    /// On the host the workspace path is unchanged — which is the observable
    /// difference between this and a real sandbox.
    #[test]
    fn the_host_sandbox_does_not_relocate_the_workspace() {
        let path = workspace();
        let handle = HostSandbox
            .prepare(&request(SecurityProfile::host(), path.clone()))
            .unwrap();
        assert_eq!(handle.workspace, path);
        let _ = std::fs::remove_dir_all(path);
    }

    #[test]
    fn a_contained_workspace_is_mounted_at_a_fixed_path() {
        let plan = mount_plan(&SecurityProfile::untrusted(), Path::new("/home/me/ws"));
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].destination, PathBuf::from(CONTAINED_WORKSPACE));
        assert_eq!(plan[0].source, PathBuf::from("/home/me/ws"));
        assert!(plan[0].writable);
    }

    #[test]
    fn a_read_only_filesystem_policy_produces_a_read_only_mount() {
        let profile = SecurityProfile {
            sandbox: SandboxKind::Container,
            image: Some("test/agent:latest".to_owned()),
            filesystem: FilesystemPolicy::WorkspaceReadOnly,
            network: NetworkPolicy::None,
            credentials: CredentialPolicy::None,
            approval: ApprovalPolicy::Anclave,
            persistence: PersistencePolicy::Ephemeral,
        };
        let plan = mount_plan(&profile, Path::new("/home/me/ws"));
        assert!(!plan[0].writable);
    }

    #[test]
    fn spawning_without_a_program_is_refused() {
        let path = workspace();
        let handle = HostSandbox
            .prepare(&request(SecurityProfile::host(), path.clone()))
            .unwrap();
        let command = CommandSpec {
            program: String::new(),
            args: Vec::new(),
            environment: BTreeMap::new(),
            working_directory: path.clone(),
        };
        assert!(HostSandbox.spawn(&handle, &command).is_err());
        let _ = std::fs::remove_dir_all(path);
    }
}
