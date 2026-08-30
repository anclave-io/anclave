//! Shared argv construction for OCI-style runtimes.
//!
//! Apple's `container` and `podman` take nearly the same command line, and
//! two hand-written copies of it would drift: the second copy is where a
//! hardening flag gets forgotten. What genuinely differs between runtimes is
//! *what they can enforce*, so that is what [`OciRuntime`] parameterises, and
//! the argv itself is written once.
//!
//! A runtime that cannot honor a policy **refuses** it here. Accepting a
//! restriction and emitting no flag for it is how a policy becomes decoration.

use std::path::PathBuf;

use anclave_protocol::Size;

use crate::sandbox::{
    mount_plan, CommandSpec, ProcessHandle, SandboxError, SandboxHandle, SandboxRequest,
    CONTAINED_WORKSPACE,
};
use crate::{NetworkPolicy, SandboxKind};

/// What one runtime's command line looks like and what it can enforce.
#[derive(Debug, Clone)]
pub struct OciRuntime {
    /// The executable.
    pub program: String,
    /// Whether `--network none` is available. When false, any network
    /// restriction is refused rather than silently dropped.
    pub network_isolation: bool,
    /// Flags applied to every container this runtime starts.
    pub hardening: &'static [&'static str],
    /// Shown to a person reading a session's posture.
    pub description: &'static str,
}

impl OciRuntime {
    fn container_name(session: &anclave_protocol::SessionId) -> String {
        // Deterministic, so a restart addresses the same container and a
        // leaked one is identifiable rather than anonymous.
        format!("anclave-{session}")
    }

    pub fn prepare(&self, request: &SandboxRequest) -> Result<SandboxHandle, SandboxError> {
        if !request.profile.sandbox.contains() {
            return Err(SandboxError::Unsupported("a host profile: use HostSandbox"));
        }
        match &request.profile.network {
            NetworkPolicy::Full => {}
            NetworkPolicy::None if self.network_isolation => {}
            NetworkPolicy::None => {
                return Err(SandboxError::Unsupported(
                    "a disabled network (this runtime exposes no network isolation)",
                ))
            }
            // Neither runtime can express "these hosts and no others", and a
            // proxy the daemon controls does not exist yet. Refusing is the
            // only honest answer until one of those changes.
            NetworkPolicy::Allowlist(_) => {
                return Err(SandboxError::Unsupported(
                    "a network allowlist (no runtime here can express one)",
                ))
            }
            NetworkPolicy::ProxyOnly => {
                return Err(SandboxError::Unsupported(
                    "proxy-only networking (no proxy is implemented yet)",
                ))
            }
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
            mounts: mount_plan(&request.profile, &request.workspace),
            image: request.profile.image.clone(),
            network: request.profile.network.clone(),
            read_only_root: request.profile.filesystem
                == crate::FilesystemPolicy::WorkspaceReadOnly,
        })
    }

    pub fn wrap(
        &self,
        sandbox: &SandboxHandle,
        command: &CommandSpec,
    ) -> Result<Vec<String>, SandboxError> {
        if command.program.is_empty() {
            return Err(SandboxError::StartupFailed("no program".to_owned()));
        }
        let image = sandbox
            .image
            .clone()
            .ok_or_else(|| SandboxError::StartupFailed("no image on the handle".to_owned()))?;

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

        argv.extend(self.hardening.iter().map(|flag| (*flag).to_owned()));

        if sandbox.network == NetworkPolicy::None {
            argv.push("--network".to_owned());
            argv.push("none".to_owned());
        }
        if sandbox.read_only_root {
            argv.push("--read-only".to_owned());
        }

        // Without this the container starts empty and the agent looks as
        // though it lost the repository.
        for mount in &sandbox.mounts {
            argv.push("-v".to_owned());
            let suffix = if mount.writable { "" } else { ":ro" };
            argv.push(format!(
                "{}:{}{suffix}",
                mount.source.display(),
                mount.destination.display()
            ));
        }

        // The environment is passed explicitly, one flag per variable. Note
        // what is *absent*: `--ssh`, which would forward the SSH agent socket
        // into the container and undo the credential policy in one flag.
        for (name, value) in &command.environment {
            argv.push("-e".to_owned());
            argv.push(format!("{name}={value}"));
        }

        argv.push(image);
        argv.push(command.program.clone());
        argv.extend(command.args.iter().cloned());
        Ok(argv)
    }

    pub fn spawn(
        &self,
        sandbox: &SandboxHandle,
        command: &CommandSpec,
    ) -> Result<ProcessHandle, SandboxError> {
        // Neither runtime starts the process here: the session backend owns
        // the pty. `wrap` is what makes the containment real.
        self.wrap(sandbox, command)?;
        Ok(ProcessHandle {
            sandbox: sandbox.id.clone(),
            reference: sandbox.id.clone(),
        })
    }

    pub fn resize(&self, size: Size) -> Result<(), SandboxError> {
        // The pty belongs to the session backend, which resizes it; the
        // container inherits that size through the tty it was given.
        size.validate()
            .map(|_| ())
            .map_err(|_| SandboxError::Failed("invalid terminal size".to_owned()))
    }

    pub fn destroy(&self, sandbox: SandboxHandle) -> Result<(), SandboxError> {
        // Best-effort: `--rm` already removes it on exit, so this only cleans
        // up a container whose process died without stopping it. A failure
        // here must not mask whatever caused the teardown.
        let _ = std::process::Command::new(&self.program)
            .args(["rm", "--force", &sandbox.id])
            .output();
        Ok(())
    }
}
