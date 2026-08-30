use std::sync::{Arc, Mutex};

use anclave_protocol::{
    CreateSession, Envelope, ErrorCode, Event, Request, Response, SessionState, SessionSummary,
};

use crate::backend::{BackendError, CreateRequest, SharedBackend};

/// How long an unanswered approval stands before it expires.
///
/// Long enough for a person to notice, short enough that a forgotten prompt
/// does not pin a session open indefinitely.
const APPROVAL_TTL: std::time::Duration = std::time::Duration::from_secs(300);
use crate::events::EventBus;
use crate::storage::Storage;
use crate::terminal::{TerminalError, TerminalStore, DEFAULT_SIZE};
use std::path::PathBuf;

#[derive(Clone)]
pub struct Runtime {
    storage: Arc<Mutex<Storage>>,
    backend: SharedBackend,
    terminals: TerminalStore,
    events: EventBus,
    agents: Arc<crate::agent::AgentRegistry>,
    security: Arc<anclave_security::SecurityConfig>,
    /// Probed once at startup: probing spawns processes, and doing it per
    /// launch would put several process spawns on the create path.
    sandbox_runtime: Option<anclave_security::runtime::Runtime>,
    approvals: anclave_security::approval::ApprovalBroker,
    audit: Arc<Mutex<Option<anclave_audit::AuditLog>>>,
    workspace_manager: Option<anclave_workspace::manager::WorkspaceManager>,
}

impl Runtime {
    pub fn new(storage: Arc<Mutex<Storage>>, backend: SharedBackend) -> Self {
        Self {
            storage,
            backend,
            terminals: TerminalStore::new(),
            events: EventBus::new(),
            agents: Arc::new(crate::agent::AgentRegistry::builtins()),
            security: Arc::new(anclave_security::SecurityConfig::default()),
            sandbox_runtime: None,
            approvals: anclave_security::approval::ApprovalBroker::new(),
            audit: Arc::new(Mutex::new(None)),
            workspace_manager: None,
        }
    }

    pub fn set_workspace_root(&mut self, root: impl Into<PathBuf>) {
        self.workspace_manager = Some(anclave_workspace::manager::WorkspaceManager::new(root));
    }

    pub fn set_agents(&mut self, agents: crate::agent::AgentRegistry) {
        self.agents = Arc::new(agents);
    }

    /// Record security decisions to this log.
    ///
    /// Optional: with no log configured the daemon still runs, and says as
    /// much rather than pretending to keep a history it is not keeping.
    pub fn set_audit_log(&self, path: impl Into<std::path::PathBuf>) {
        *self.audit.lock().expect("audit mutex") = Some(anclave_audit::AuditLog::new(path));
    }

    /// Append to the audit log if one is configured.
    ///
    /// Best effort by design: a full disk must not stop a session being
    /// deleted. The failure is visible in the log's own sequence, which will
    /// show the gap on the next verify.
    fn record(&self, event: anclave_audit::AuditEvent) {
        if let Some(log) = self.audit.lock().expect("audit mutex").as_ref() {
            let _ = log.append(event);
        }
    }

    pub fn set_security(&mut self, security: anclave_security::SecurityConfig) {
        self.security = Arc::new(security);
    }

    /// Probe the host for a containment runtime, once.
    pub fn detect_sandbox_runtime(&mut self) {
        self.sandbox_runtime = anclave_security::runtime::detect().recommended;
    }

    pub fn set_sandbox_runtime(&mut self, runtime: Option<anclave_security::runtime::Runtime>) {
        self.sandbox_runtime = runtime;
    }

    /// Fill in a stored session's posture from the live configuration.
    ///
    /// Storage keeps only the profile *name*: deliberately, so a profile the
    /// operator tightens applies on the next launch. The consequence is that
    /// a row read back has no idea whether it is contained, and reporting
    /// `contained: false` for a session that is contained defeats the point
    /// of publishing a posture at all.
    fn with_posture(&self, mut session: SessionSummary) -> SessionSummary {
        if let Ok((posture, _)) = self.posture(Some(&session.security.profile)) {
            session.security = posture;
        }
        session
    }

    /// Resolve a requested profile name into the posture clients are shown.
    fn posture(
        &self,
        requested: Option<&str>,
    ) -> Result<
        (
            anclave_protocol::SecurityPosture,
            anclave_security::SecurityProfile,
        ),
        String,
    > {
        let name = requested.unwrap_or(&self.security.default).to_owned();
        let profile = self
            .security
            .get(&name)
            .map_err(|error| error.to_string())?
            .clone();
        let posture = anclave_protocol::SecurityPosture {
            profile: name,
            contained: profile.containment(),
            summary: profile.summary(),
            caveats: profile.caveats().into_iter().map(str::to_owned).collect(),
        };
        Ok((posture, profile))
    }

    pub fn events(&self) -> EventBus {
        self.events.clone()
    }

    pub fn recover_sessions(&self) {
        let sessions = self
            .storage
            .lock()
            .expect("storage mutex is not poisoned")
            .list_sessions()
            .unwrap_or_default();
        for session in sessions {
            let state = match self.backend.adopt(&session.id) {
                Ok(_) => {
                    let _ = self.terminals.insert(&session.id, DEFAULT_SIZE);
                    SessionState::Running
                }
                Err(BackendError::NotFound) => SessionState::Exited,
                Err(_) => SessionState::Unreachable,
            };
            let _ = self
                .storage
                .lock()
                .expect("storage mutex is not poisoned")
                .set_session_state(&session.id, state);
        }
    }

    pub fn poll_backend(&self) {
        let Ok(ids) = self.backend.sessions() else {
            return;
        };
        for id in ids {
            // A capture failure is transient (the pane may be mid-teardown),
            // so skip this round rather than tearing anything down.
            let Ok(output) = self.backend.capture(&id) else {
                continue;
            };
            // Publish only when the screen actually moved. The backend
            // returns the full pane every poll, so an unconditional event
            // meant ten notifications a second for an idle session.
            // Cursor and alternate-screen state come from the multiplexer,
            // not from parsing the captured text, which cannot carry them.
            // Best effort: a pane that vanished between the two calls should
            // not lose its screen as well.
            if let Ok(state) = self.backend.pane_state(&id) {
                let _ = self.terminals.set_pane_state(&id, state);
            }
            match self.terminals.apply_capture(&id, &output) {
                Ok(true) => self.events.publish_screen_changed(id),
                Ok(false) => {}
                Err(_) => continue,
            }
        }
    }

    pub fn handle(&self, request: Request) -> Response {
        match request {
            Request::Ping => Response::Pong,
            Request::GetVersion => Response::Version {
                protocol: anclave_protocol::PROTOCOL_VERSION,
                version: env!("CARGO_PKG_VERSION").to_owned(),
            },
            Request::ListSessions => self
                .storage
                .lock()
                .expect("storage mutex is not poisoned")
                .list_sessions()
                .map(|sessions| {
                    Response::Sessions(
                        sessions
                            .into_iter()
                            .map(|session| self.with_posture(session))
                            .collect(),
                    )
                })
                .unwrap_or_else(storage_error),
            Request::GetSession { id } => {
                match self
                    .storage
                    .lock()
                    .expect("storage mutex is not poisoned")
                    .get_session(&id)
                {
                    Ok(Some(session)) => Response::Session(self.with_posture(session)),
                    Ok(None) => response_error(ErrorCode::NotFound, "session not found"),
                    Err(error) => storage_error(error),
                }
            }
            Request::CreateSession(request) => self.create(request),
            Request::RestartSession { id } => self.restart(&id),
            Request::DeleteSession { id } => self.delete(&id),
            Request::GetSandboxReport => Response::Sandboxes(sandbox_report()),
            Request::ListApprovals => Response::Approvals(self.pending_approvals()),
            Request::ApproveAction { id } => self.decide_approval(&id, true),
            Request::DenyAction { id } => self.decide_approval(&id, false),
            Request::SubscribeEvents => Response::Subscribed,
            Request::Shutdown => Response::Accepted,
            Request::CaptureScreen { id } => match self.capture(&id) {
                Ok(screen) => Response::Screen(screen),
                Err(error) => terminal_error(error),
            },
            Request::SendInput { id, bytes } => match self.backend.send_input(&id, &bytes) {
                Ok(()) => Response::Accepted,
                Err(error) => backend_error(error),
            },
            Request::ResizeSession { id, size } => match self.backend.resize(&id, size) {
                Ok(()) => match self.terminals.resize(&id, size) {
                    Ok(()) => Response::Accepted,
                    Err(error) => terminal_error(error),
                },
                Err(error) => backend_error(error),
            },
            // Attach and detach are about *this client's* interest in a
            // session, not about the session's lifetime: the daemon holds the
            // terminal open regardless, which is what makes persistence work.
            // So attaching validates the session and hands back the current
            // screen, and detaching is a no-op beyond that validation.
            Request::AttachSession { id } => match self.capture(&id) {
                Ok(screen) => Response::Screen(screen),
                Err(error) => terminal_error(error),
            },
            Request::DetachSession { id } => {
                match self
                    .storage
                    .lock()
                    .expect("storage mutex is not poisoned")
                    .get_session(&id)
                {
                    Ok(Some(_)) => Response::Accepted,
                    Ok(None) => response_error(ErrorCode::NotFound, "session not found"),
                    Err(error) => storage_error(error),
                }
            }
        }
    }

    fn capture(
        &self,
        id: &anclave_protocol::SessionId,
    ) -> Result<anclave_protocol::ScreenSnapshot, TerminalError> {
        let backend_output = self.backend.capture(id).map_err(|error| match error {
            BackendError::NotFound => TerminalError::NotFound,
            _ => TerminalError::NotFound,
        })?;
        if let Ok(state) = self.backend.pane_state(id) {
            let _ = self.terminals.set_pane_state(id, state);
        }
        if self.terminals.apply_capture(id, &backend_output)? {
            self.events.publish_screen_changed(id.clone());
        }
        self.terminals.capture(id)
    }

    fn create(&self, request: CreateSession) -> Response {
        if request.name.trim().is_empty() {
            return response_error(ErrorCode::InvalidRequest, "session name cannot be empty");
        }

        let id = match self
            .storage
            .lock()
            .expect("storage mutex is not poisoned")
            .next_session_id()
        {
            Ok(id) => id,
            Err(error) => return storage_error(error),
        };
        let Some(agent) = self.agents.get(&request.agent) else {
            return response_error(
                ErrorCode::UnknownAgent,
                &format!("unknown agent: {}", request.agent),
            );
        };

        let (posture, profile) = match self.posture(request.security.as_deref()) {
            Ok(resolved) => resolved,
            Err(message) => return response_error(ErrorCode::InvalidRequest, &message),
        };

        let mut summary = SessionSummary {
            id: id.clone(),
            name: request.name,
            state: SessionState::Creating,
            agent: request.agent,
            workspace: request.workspace,
            security: posture,
        };

        if let Err(error) = self
            .storage
            .lock()
            .expect("storage mutex is not poisoned")
            .insert_session(&summary)
        {
            return if is_constraint_error(&error) {
                response_error(ErrorCode::InvalidRequest, "session name already exists")
            } else {
                storage_error(error)
            };
        }
        self.events.publish(Event::SessionCreated {
            session: summary.clone(),
        });

        summary.state = SessionState::Starting;
        if let Err(error) = self
            .storage
            .lock()
            .expect("storage mutex is not poisoned")
            .update_session(&summary)
        {
            return self.rollback_create(&summary.id, storage_error(error));
        }
        self.events.publish(Event::SessionStateChanged {
            session: summary.clone(),
        });

        let workspace_path = self.prepare_workspace(summary.workspace.as_ref());
        if workspace_path.is_none() && summary.workspace.is_some() {
            return self.rollback_create(
                &summary.id,
                response_error(ErrorCode::BackendFailure, "workspace preparation failed"),
            );
        }

        let launch = match self.launch_under(
            agent.launch(&summary.id),
            &profile,
            &summary.id,
            workspace_path.as_deref(),
        ) {
            Ok(launch) => launch,
            Err(message) => {
                if let Some(ref path) = workspace_path {
                    self.cleanup_workspace(path);
                }
                return self
                    .rollback_create(&summary.id, response_error(ErrorCode::Internal, &message));
            }
        };
        let backend_request = CreateRequest {
            session_id: summary.id.clone(),
            name: summary.name.clone(),
            size: DEFAULT_SIZE,
            launch,
        };
        if let Err(error) = self.backend.create(backend_request) {
            if let Some(ref path) = workspace_path {
                self.cleanup_workspace(path);
            }
            return self.rollback_create(&summary.id, backend_error(error));
        }
        if let Err(error) = self.terminals.insert(&summary.id, DEFAULT_SIZE) {
            let _ = self.backend.kill(&summary.id);
            if let Some(ref path) = workspace_path {
                self.cleanup_workspace(path);
            }
            return self.rollback_create(&summary.id, terminal_error(error));
        }

        summary.state = SessionState::Running;
        if let Err(error) = self
            .storage
            .lock()
            .expect("storage mutex is not poisoned")
            .update_session(&summary)
        {
            let response = storage_error(error);
            let _ = self.backend.kill(&summary.id);
            self.terminals.remove(&summary.id);
            return self.rollback_create(&summary.id, response);
        }
        self.events.publish(Event::SessionStateChanged {
            session: summary.clone(),
        });
        // The posture a session actually launched under is the fact an
        // auditor most wants later, and it cannot be recovered from a row
        // that only stores a profile name whose definition may since have
        // changed.
        self.record(
            anclave_audit::AuditEvent::now(
                "daemon",
                "create session",
                summary.security.profile.clone(),
                if summary.security.contained {
                    "contained"
                } else {
                    "uncontained"
                },
            )
            .for_session(summary.id.to_string()),
        );
        Response::Session(summary)
    }

    /// Turn an agent launch into the command that actually runs, under the
    /// sandbox the profile calls for.
    ///
    /// Every launch goes through a `Sandbox`, including the uncontained one.
    /// That is the point of the abstraction: if the host path bypassed it,
    /// adding containment later would only cover whatever someone remembered
    /// to route through it.
    fn launch_under(
        &self,
        launch: crate::agent::LaunchSpec,
        profile: &anclave_security::SecurityProfile,
        id: &anclave_protocol::SessionId,
        workspace: Option<&std::path::Path>,
    ) -> Result<crate::agent::LaunchSpec, String> {
        use anclave_security::sandbox::CommandSpec;

        let identity =
            std::collections::BTreeMap::from([("ANCLAVE_SESSION".to_owned(), id.to_string())]);
        let environment =
            anclave_security::environment::build_environment(profile, std::env::vars(), &identity);

        if !profile.containment() {
            // Uncontained: the sandbox has nothing to wrap, so the only
            // control is the environment: applied by the backend, and only
            // when the policy is not ambient.
            return Ok(crate::agent::LaunchSpec {
                environment: (profile.credentials != anclave_security::CredentialPolicy::Ambient)
                    .then_some(environment),
                ..launch
            });
        }

        // A contained session must have somewhere to be contained *around*.
        let workspace = workspace.ok_or_else(|| {
            "a contained session needs a workspace to mount; create it with one".to_owned()
        })?;

        let sandbox = anclave_security::sandbox::for_profile(profile, self.sandbox_runtime)
            .map_err(|e| e.to_string())?;
        let request = anclave_security::sandbox::SandboxRequest {
            session: id.clone(),
            profile: profile.clone(),
            workspace: workspace.to_path_buf(),
            size: DEFAULT_SIZE,
        };
        let handle = sandbox.prepare(&request).map_err(|e| e.to_string())?;
        let command = CommandSpec {
            program: launch.program.clone(),
            args: launch.args.clone(),
            environment,
            working_directory: handle.workspace.clone(),
        };
        let argv = sandbox.wrap(&handle, &command).map_err(|e| e.to_string())?;
        let (program, args) = argv
            .split_first()
            .ok_or_else(|| "sandbox produced an empty command".to_owned())?;

        Ok(crate::agent::LaunchSpec {
            program: program.clone(),
            args: args.to_vec(),
            // The environment is already inside the argv as `-e` flags;
            // wrapping again in `env -i` would strip what the runtime needs
            // to start the container in the first place.
            environment: None,
        })
    }

    /// Where a persisted session's workspace lives, if it has one.
    fn workspace_for(&self, session: &SessionSummary) -> Option<std::path::PathBuf> {
        let (Some(manager), Some(spec)) = (&self.workspace_manager, session.workspace.as_ref())
        else {
            return None;
        };
        Some(manager.workspace_path(spec))
    }

    /// Ask for a decision, telling every client what is waiting.
    ///
    /// Returns the approval so a caller can poll for the answer. Nothing here
    /// blocks the request thread: an approval that stalls the daemon would
    /// let one unanswered prompt stop every other session.
    pub fn request_approval(
        &self,
        session: anclave_protocol::SessionId,
        action: anclave_security::approval::Action,
        ttl: std::time::Duration,
    ) -> anclave_security::approval::PendingApproval {
        let pending = self.approvals.request(session, action, ttl);
        self.events.publish(Event::ApprovalRequested {
            approval: to_protocol_approval(&pending),
        });
        pending
    }

    pub fn approval_decision(&self, id: &str) -> Option<anclave_security::approval::Decision> {
        self.approvals.decision(id)
    }

    /// Expire anything past its deadline and announce it.
    ///
    /// Silence is not consent, so a request nobody answered ends as expired
    /// rather than waiting forever.
    pub fn expire_approvals(&self) {
        for expired in self.approvals.expire_due(std::time::SystemTime::now()) {
            self.events
                .publish(Event::ApprovalExpired { id: expired.id });
        }
    }

    fn pending_approvals(&self) -> Vec<anclave_protocol::ApprovalRequest> {
        self.approvals
            .pending()
            .iter()
            .map(to_protocol_approval)
            .collect()
    }

    fn decide_approval(&self, id: &str, approved: bool) -> Response {
        use anclave_security::approval::{ApprovalError, Decision};
        let decision = if approved {
            Decision::Approved
        } else {
            Decision::Denied
        };
        match self.approvals.decide(id, decision) {
            Ok(()) => {
                self.record(anclave_audit::AuditEvent::now(
                    "client",
                    format!("decide approval {id}"),
                    "anclave",
                    if approved { "approved" } else { "denied" },
                ));
                self.events.publish(Event::ApprovalResolved {
                    id: id.to_owned(),
                    approved,
                });
                Response::Accepted
            }
            Err(error @ ApprovalError::Unknown(_)) => {
                response_error(ErrorCode::NotFound, &error.to_string())
            }
            Err(error) => response_error(ErrorCode::InvalidRequest, &error.to_string()),
        }
    }

    /// Refuse a destructive delete that has not been approved.
    ///
    /// Only under `ApprovalPolicy::Anclave`. The default profile leaves the
    /// decision to the agent, which is what makes it the compatibility mode,
    /// and gating it there would break every existing caller.
    fn refuse_unapproved_destroy(&self, session: &SessionSummary) -> Option<Response> {
        use anclave_security::approval::{Action, Decision};
        use anclave_security::ApprovalPolicy;

        let profile = self
            .security
            .get(&session.security.profile)
            .ok()
            .cloned()
            .unwrap_or_default();
        if profile.approval != ApprovalPolicy::Anclave {
            return None;
        }
        let workspace = session.workspace.as_ref()?;

        let pending = self.request_approval(
            session.id.clone(),
            Action::DestroyWorkspace {
                path: workspace.id.to_string(),
            },
            APPROVAL_TTL,
        );
        match self.approval_decision(&pending.id) {
            Some(Decision::Approved) => None,
            // Nothing has decided yet, and the daemon must not block waiting.
            // Refusing with the id lets a client approve and retry, which is
            // the only answer that cannot become an accidental yes.
            _ => {
                self.record(
                    anclave_audit::AuditEvent::now(
                        "daemon",
                        "destroy workspace",
                        "anclave",
                        "refused: awaiting approval",
                    )
                    .for_session(session.id.to_string()),
                );
                Some(response_error(
                    ErrorCode::PermissionDenied,
                    &format!(
                        "destroying this workspace needs approval: approve {} and retry",
                        pending.id
                    ),
                ))
            }
        }
    }

    fn rollback_create(&self, id: &anclave_protocol::SessionId, response: Response) -> Response {
        self.terminals.remove(id);
        let _ = self
            .storage
            .lock()
            .expect("storage mutex is not poisoned")
            .remove_session(id);
        response
    }

    fn prepare_workspace(&self, spec: Option<&anclave_protocol::WorkspaceSpec>) -> Option<PathBuf> {
        let (Some(ref wm), Some(spec)) = (&self.workspace_manager, spec) else {
            return None;
        };
        wm.create(spec).ok()
    }

    fn cleanup_workspace(&self, path: &std::path::Path) {
        if let Some(ref wm) = self.workspace_manager {
            wm.cleanup_path(path);
        } else {
            let _ = std::fs::remove_dir_all(path);
        }
    }

    fn cleanup_workspace_for_spec(&self, spec: Option<&anclave_protocol::WorkspaceSpec>) {
        if let (Some(ref wm), Some(spec)) = (&self.workspace_manager, spec) {
            wm.cleanup(spec);
        }
    }

    fn restart(&self, id: &anclave_protocol::SessionId) -> Response {
        let Some(existing) = self
            .storage
            .lock()
            .expect("storage mutex is not poisoned")
            .get_session(id)
            .ok()
            .flatten()
        else {
            return response_error(ErrorCode::NotFound, "session not found");
        };

        let Some(agent) = self.agents.get(&existing.agent) else {
            return response_error(
                ErrorCode::UnknownAgent,
                &format!("configured agent is unavailable: {}", existing.agent),
            );
        };
        // Restarting must continue the agent's session where the agent can:
        // relaunching fresh silently discards the conversation, which is the
        // whole reason a restart is preferred over delete-and-create. An agent
        // with `ResumeStrategy::FreshOnly` reports no resume spec and starts
        // over, which is the documented fallback rather than a failure.
        // Restart re-resolves the *stored* profile: a session created under
        // `untrusted` must not come back uncontained because the default
        // changed underneath it.
        let (_, profile) = match self.posture(Some(&existing.security.profile)) {
            Ok(resolved) => resolved,
            Err(message) => return response_error(ErrorCode::InvalidRequest, &message),
        };
        let request = CreateRequest {
            session_id: id.clone(),
            name: existing.name.clone(),
            size: DEFAULT_SIZE,
            launch: match self.launch_under(
                agent.resume(id).unwrap_or_else(|| agent.launch(id)),
                &profile,
                id,
                self.workspace_for(&existing).as_deref(),
            ) {
                Ok(launch) => launch,
                Err(message) => return response_error(ErrorCode::Internal, &message),
            },
        };
        if let Err(error) = self.backend.restart(request) {
            return backend_error(error);
        }
        let _ = self
            .storage
            .lock()
            .expect("storage mutex is not poisoned")
            .set_session_state(id, SessionState::Running);
        let _ = self.terminals.insert(id, DEFAULT_SIZE);
        Response::Session(SessionSummary {
            state: SessionState::Running,
            ..existing
        })
    }

    fn delete(&self, id: &anclave_protocol::SessionId) -> Response {
        let existing = self
            .storage
            .lock()
            .expect("storage mutex is not poisoned")
            .get_session(id)
            .ok()
            .flatten();

        // Destroying a workspace is irreversible, so under an `anclave`
        // approval policy it needs a decision first. This is the shape every
        // gated action takes: the daemon holds a capability the agent does
        // not, so refusing is enforcement rather than a request.
        if let Some(ref session) = existing {
            if let Some(refusal) = self.refuse_unapproved_destroy(session) {
                return refusal;
            }
        }
        let deleted = self
            .storage
            .lock()
            .expect("storage mutex is not poisoned")
            .delete_session(id);
        match deleted {
            Ok(true) => {
                self.terminals.remove(id);
                if let Some(ref session) = existing {
                    self.cleanup_workspace_for_spec(session.workspace.as_ref());
                }
                match self.backend.kill(id) {
                    Ok(()) | Err(BackendError::NotFound) => Response::Accepted,
                    Err(error) => backend_error(error),
                }
            }
            Ok(false) => response_error(ErrorCode::NotFound, "session not found"),
            Err(error) => storage_error(error),
        }
    }
}

/// Probe this host and translate the finding into the protocol's shape.
fn sandbox_report() -> anclave_protocol::SandboxReport {
    let report = anclave_security::runtime::detect();
    anclave_protocol::SandboxReport {
        platform: format!("{:?}", report.platform).to_lowercase(),
        candidates: report
            .candidates
            .iter()
            .map(|candidate| anclave_protocol::SandboxCandidate {
                name: candidate.runtime.name().to_owned(),
                available: candidate.available,
                isolation: candidate.runtime.isolation().describe().to_owned(),
                detail: candidate.detail.clone(),
                caveat: candidate.runtime.caveat().to_owned(),
            })
            .collect(),
        recommended: report.recommended.map(|runtime| runtime.name().to_owned()),
    }
}

/// Present an approval in the protocol's shape.
fn to_protocol_approval(
    pending: &anclave_security::approval::PendingApproval,
) -> anclave_protocol::ApprovalRequest {
    anclave_protocol::ApprovalRequest {
        id: pending.id.clone(),
        session: pending.session.clone(),
        action: pending.action.describe(),
        destructive: pending.action.is_destructive(),
        expires_in_secs: pending
            .expires_at
            .duration_since(std::time::SystemTime::now())
            .map(|remaining| remaining.as_secs())
            .unwrap_or(0),
    }
}

fn is_constraint_error(error: &rusqlite::Error) -> bool {
    matches!(error, rusqlite::Error::SqliteFailure(inner, _) if inner.code == rusqlite::ErrorCode::ConstraintViolation)
}

fn backend_error(error: BackendError) -> Response {
    match error {
        BackendError::NotFound => response_error(ErrorCode::NotFound, "backend session not found"),
        BackendError::AlreadyExists => {
            response_error(ErrorCode::BackendFailure, "backend session already exists")
        }
        BackendError::InvalidSize => {
            response_error(ErrorCode::InvalidSize, "invalid terminal size")
        }
        BackendError::InputTooLarge => {
            response_error(ErrorCode::InvalidRequest, "input exceeds the maximum size")
        }
        BackendError::Failed(message) => response_error(ErrorCode::BackendFailure, &message),
    }
}

fn terminal_error(error: TerminalError) -> Response {
    match error {
        TerminalError::InvalidSize => {
            response_error(ErrorCode::InvalidSize, "invalid terminal size")
        }
        TerminalError::NotFound => {
            response_error(ErrorCode::NotFound, "terminal session not found")
        }
    }
}

fn storage_error(storage: rusqlite::Error) -> Response {
    response_error(ErrorCode::Internal, &format!("storage error: {storage}"))
}

fn response_error(code: ErrorCode, message: &str) -> Response {
    Response::Error {
        code,
        message: message.to_owned(),
    }
}

pub fn handle_envelope(runtime: &Runtime, request: Envelope<Request>) -> Envelope<Response> {
    Envelope::new(request.request_id, runtime.handle(request.payload))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::FakeBackend;
    use anclave_protocol::{AgentId, BackendId, RequestId};

    fn runtime() -> Runtime {
        Runtime::new(
            Arc::new(Mutex::new(Storage::open_in_memory().unwrap())),
            Arc::new(FakeBackend::new()),
        )
    }

    fn custom_runtime(backend: Arc<FakeBackend>) -> Runtime {
        let mut runtime = Runtime::new(
            Arc::new(Mutex::new(Storage::open_in_memory().unwrap())),
            backend,
        );
        let path =
            std::env::temp_dir().join(format!("anclave-runtime-agent-{}.toml", std::process::id()));
        std::fs::write(
            &path,
            "[[agents]]\nname = 'mock'\ncommand = 'mock-agent'\nargs = ['--session', '{id}', '--mode', 'test']\n",
        )
        .unwrap();
        runtime.set_agents(crate::agent::AgentRegistry::load(&path).unwrap());
        let _ = std::fs::remove_file(path);
        runtime
    }

    /// An agent that can resume must be resumed on restart, not relaunched:
    /// a fresh launch silently discards the conversation.
    #[test]
    fn restart_resumes_an_agent_that_supports_it() {
        let backend = Arc::new(FakeBackend::new());
        let mut runtime = Runtime::new(
            Arc::new(Mutex::new(Storage::open_in_memory().unwrap())),
            backend.clone(),
        );
        let path =
            std::env::temp_dir().join(format!("anclave-resume-agent-{}.toml", std::process::id()));
        std::fs::write(
            &path,
            "[[agents]]\nname = 'resuming'\ncommand = 'mock-agent'\nargs = ['--fresh']\n             resume = { strategy = 'exact_session_id', args = ['--resume', '{id}'] }\n",
        )
        .unwrap();
        runtime.set_agents(crate::agent::AgentRegistry::load(&path).unwrap());
        let _ = std::fs::remove_file(path);

        let Response::Session(session) = runtime.handle(Request::CreateSession(CreateSession {
            name: "demo".to_owned(),
            agent: AgentId::new("resuming").unwrap(),
            backend: BackendId::new("local").unwrap(),
            workspace: None,
            security: None,
        })) else {
            panic!("expected created session")
        };
        assert_eq!(backend.launches()[0].launch.args, vec!["--fresh"]);

        assert!(matches!(
            runtime.handle(Request::RestartSession {
                id: session.id.clone()
            }),
            Response::Session(_)
        ));
        let launches = backend.launches();
        assert_eq!(launches.len(), 2, "restart should reach the backend");
        assert_eq!(
            launches[1].launch.args,
            vec!["--resume", session.id.as_str()],
            "restart must resume rather than start over"
        );
    }

    /// Attaching returns the live screen and leaves the session running;
    /// detaching does not end it. The daemon owning the terminal across client
    /// comings and goings is the persistence promise.
    #[test]
    fn attach_returns_the_screen_and_detach_leaves_the_session_running() {
        let runtime = runtime();
        let Response::Session(session) = runtime.handle(create("demo")) else {
            panic!("expected created session")
        };

        assert!(matches!(
            runtime.handle(Request::AttachSession {
                id: session.id.clone()
            }),
            Response::Screen(_)
        ));
        assert!(matches!(
            runtime.handle(Request::DetachSession {
                id: session.id.clone()
            }),
            Response::Accepted
        ));

        let Response::Session(after) = runtime.handle(Request::GetSession { id: session.id })
        else {
            panic!("session should survive a detach")
        };
        assert_eq!(after.state, SessionState::Running);
    }

    #[test]
    fn attach_and_detach_reject_an_unknown_session() {
        let runtime = runtime();
        let missing = missing_id();
        assert!(matches!(
            runtime.handle(Request::AttachSession {
                id: missing.clone()
            }),
            Response::Error { .. }
        ));
        assert!(matches!(
            runtime.handle(Request::DetachSession { id: missing }),
            Response::Error {
                code: ErrorCode::NotFound,
                ..
            }
        ));
    }

    fn missing_id() -> anclave_protocol::SessionId {
        anclave_protocol::SessionId::new("missing").unwrap()
    }

    fn secured_runtime(backend: Arc<FakeBackend>, profile: &str) -> Runtime {
        let mut runtime = Runtime::new(
            Arc::new(Mutex::new(Storage::open_in_memory().unwrap())),
            backend,
        );
        // Contained enough that withholding credentials is honest.
        let text = format!(
            "default = \"{profile}\"\n\n\
             [profiles.default]\nsandbox = \"host\"\n\n\
             [profiles.locked]\nsandbox = \"container\"\nimage = \"anclave/agent:latest\"\n\
             credentials = {{ mode = \"none\" }}\n\n\
             [profiles.nocreds]\nsandbox = \"host\"\ncredentials = {{ mode = \"none\" }}\n"
        );
        runtime.set_security(anclave_security::SecurityConfig::parse(&text).unwrap());
        runtime
    }

    fn create_under(name: &str, profile: Option<&str>) -> Request {
        Request::CreateSession(CreateSession {
            name: name.to_owned(),
            agent: AgentId::new("default").unwrap(),
            backend: BackendId::new("local").unwrap(),
            workspace: None,
            security: profile.map(str::to_owned),
        })
    }

    /// The default posture is uncontained, and every client is told so.
    #[test]
    fn a_session_reports_its_posture() {
        let runtime = runtime();
        let Response::Session(session) = runtime.handle(create("demo")) else {
            panic!("expected created session")
        };
        assert_eq!(session.security.profile, "default");
        assert!(!session.security.contained);
        assert!(!session.security.caveats.is_empty());
    }

    /// Withholding credentials is enforceable on the host, because the
    /// daemon builds the child environment itself.
    #[test]
    fn a_host_profile_can_still_withhold_credentials() {
        let backend = Arc::new(FakeBackend::new());
        let runtime = secured_runtime(backend.clone(), "default");

        std::env::set_var("ANCLAVE_TEST_FAKE_TOKEN", "super-secret");
        let response = runtime.handle(create_under("nocreds", Some("nocreds")));
        std::env::remove_var("ANCLAVE_TEST_FAKE_TOKEN");

        let Response::Session(session) = response else {
            panic!("expected created session")
        };
        // Honest about itself: withholding variables is not containment.
        assert!(!session.security.contained);

        let environment = backend.launches()[0]
            .launch
            .environment
            .clone()
            .expect("a non-ambient launch carries a constructed environment");
        assert!(
            !environment.contains_key("ANCLAVE_TEST_FAKE_TOKEN"),
            "a credential variable reached the agent"
        );
        assert_eq!(
            environment.get("ANCLAVE_SESSION").map(String::as_str),
            Some(session.id.as_str())
        );
    }

    /// There is nothing to mount, so there is nothing to contain around. The
    /// error names the fix rather than failing at launch inside the runtime.
    #[test]
    fn a_contained_session_without_a_workspace_is_refused() {
        let backend = Arc::new(FakeBackend::new());
        let runtime = secured_runtime(backend.clone(), "default");
        let response = runtime.handle(create_under("locked", Some("locked")));
        let Response::Error { message, .. } = response else {
            panic!("a contained session with no workspace must be refused")
        };
        assert!(message.contains("workspace"), "unhelpful error: {message}");
        // And it must leave nothing behind.
        assert_eq!(
            runtime.handle(Request::ListSessions),
            Response::Sessions(vec![])
        );
        assert!(backend.launches().is_empty());
    }

    /// Ambient is the compatibility path and must stay a pass-through, or the
    /// default profile would quietly become something other than ambient.
    #[test]
    fn the_ambient_profile_leaves_the_environment_inherited() {
        let backend = Arc::new(FakeBackend::new());
        let runtime = secured_runtime(backend.clone(), "default");
        assert!(matches!(
            runtime.handle(create_under("compat", Some("default"))),
            Response::Session(_)
        ));
        assert!(backend.launches()[0].launch.environment.is_none());
    }

    #[test]
    fn an_unknown_profile_is_refused_before_anything_is_persisted() {
        let runtime = runtime();
        assert!(matches!(
            runtime.handle(create_under("demo", Some("nope"))),
            Response::Error { .. }
        ));
        assert_eq!(
            runtime.handle(Request::ListSessions),
            Response::Sessions(vec![])
        );
    }

    /// A session created under a stricter profile must not come back looser
    /// because the default moved underneath it.
    #[test]
    fn restart_re_resolves_the_stored_profile() {
        let backend = Arc::new(FakeBackend::new());
        let runtime = secured_runtime(backend.clone(), "default");
        let Response::Session(session) = runtime.handle(create_under("strict", Some("nocreds")))
        else {
            panic!("expected created session")
        };
        assert!(matches!(
            runtime.handle(Request::RestartSession { id: session.id }),
            Response::Session(_)
        ));
        let launches = backend.launches();
        assert!(
            launches[1].launch.environment.is_some(),
            "the restart must keep withholding credentials"
        );
    }

    /// A stored session must report the posture it actually has. Reading a
    /// row back gave `contained: false` for a contained session, which makes
    /// the published posture worse than useless.
    #[test]
    fn a_stored_session_reports_its_real_containment() {
        let backend = Arc::new(FakeBackend::new());
        let runtime = secured_runtime(backend, "default");
        let Response::Session(created) = runtime.handle(create_under("plain", Some("nocreds")))
        else {
            panic!("expected created session")
        };
        assert_eq!(created.security.profile, "nocreds");

        // The same facts must survive a read.
        let Response::Session(fetched) = runtime.handle(Request::GetSession { id: created.id })
        else {
            panic!("expected the session back")
        };
        assert_eq!(fetched.security.profile, "nocreds");
        assert_eq!(fetched.security.summary, created.security.summary);
        assert_eq!(fetched.security.caveats, created.security.caveats);

        let Response::Sessions(listed) = runtime.handle(Request::ListSessions) else {
            panic!("expected a listing")
        };
        assert_eq!(listed[0].security.summary, created.security.summary);
    }

    fn create(name: &str) -> Request {
        Request::CreateSession(CreateSession {
            name: name.to_owned(),
            agent: AgentId::new("default").unwrap(),
            backend: BackendId::new("local").unwrap(),
            workspace: None,
            security: None,
        })
    }

    #[test]
    fn unknown_agents_are_rejected_before_persisting_a_session() {
        let runtime = runtime();
        let response = runtime.handle(Request::CreateSession(CreateSession {
            name: "demo".to_owned(),
            agent: AgentId::new("missing").unwrap(),
            backend: BackendId::new("local").unwrap(),
            workspace: None,
            security: None,
        }));
        assert!(matches!(
            response,
            Response::Error {
                code: ErrorCode::UnknownAgent,
                ..
            }
        ));
        assert_eq!(
            runtime.handle(Request::ListSessions),
            Response::Sessions(vec![])
        );
    }

    #[test]
    fn custom_agent_launch_spec_reaches_the_backend() {
        let backend = Arc::new(FakeBackend::new());
        let runtime = custom_runtime(backend.clone());
        let response = runtime.handle(Request::CreateSession(CreateSession {
            name: "custom".to_owned(),
            agent: AgentId::new("mock").unwrap(),
            backend: BackendId::new("local").unwrap(),
            workspace: None,
            security: None,
        }));
        let Response::Session(session) = response else {
            panic!("expected created session")
        };
        let launches = backend.launches();
        assert_eq!(launches.len(), 1);
        assert_eq!(launches[0].session_id, session.id);
        assert_eq!(launches[0].launch.program, "mock-agent");
        assert_eq!(
            launches[0].launch.args,
            vec!["--session", "session-0", "--mode", "test"]
        );
    }

    #[test]
    fn runtime_starts_empty() {
        let runtime = runtime();
        assert_eq!(
            runtime.handle(Request::ListSessions),
            Response::Sessions(vec![])
        );
    }

    #[test]
    fn create_list_get_resize_and_delete_session() {
        let runtime = runtime();
        let Response::Session(session) = runtime.handle(create("demo")) else {
            panic!("expected created session")
        };
        assert_eq!(session.state, SessionState::Running);
        assert!(matches!(
            runtime.handle(Request::GetSession {
                id: session.id.clone()
            }),
            Response::Session(_)
        ));
        assert!(matches!(
            runtime.handle(Request::ResizeSession {
                id: session.id.clone(),
                size: anclave_protocol::Size {
                    columns: 100,
                    rows: 30
                }
            }),
            Response::Accepted
        ));
        assert!(matches!(
            runtime.handle(Request::DeleteSession { id: session.id }),
            Response::Accepted
        ));
        assert_eq!(
            runtime.handle(Request::ListSessions),
            Response::Sessions(vec![])
        );
    }

    #[test]
    fn duplicate_names_are_rejected() {
        let runtime = runtime();
        assert!(matches!(
            runtime.handle(create("demo")),
            Response::Session(_)
        ));
        assert!(matches!(
            runtime.handle(create("demo")),
            Response::Error { .. }
        ));
    }

    #[test]
    fn recovery_adopts_existing_backend_sessions() {
        let backend = Arc::new(FakeBackend::new());
        let storage = Arc::new(Mutex::new(Storage::open_in_memory().unwrap()));
        let first = Runtime::new(storage.clone(), backend.clone());
        let Response::Session(session) = first.handle(create("demo")) else {
            panic!("expected created session")
        };
        let recovered = Runtime::new(storage, backend);
        recovered.recover_sessions();
        assert!(matches!(
            recovered.handle(Request::CaptureScreen {
                id: session.id.clone()
            }),
            Response::Screen(_)
        ));
        let Response::Session(recovered_session) =
            recovered.handle(Request::GetSession { id: session.id })
        else {
            panic!("expected recovered session")
        };
        assert_eq!(recovered_session.state, SessionState::Running);
    }

    #[test]
    fn recovery_marks_missing_backend_sessions_exited() {
        let storage = Arc::new(Mutex::new(Storage::open_in_memory().unwrap()));
        let backend = Arc::new(FakeBackend::new());
        let id = anclave_protocol::SessionId::new("session-1").unwrap();
        storage
            .lock()
            .unwrap()
            .insert_session(&SessionSummary {
                id: id.clone(),
                name: "demo".to_owned(),
                state: SessionState::Starting,
                agent: AgentId::new("default").unwrap(),
                workspace: None,
                security: Default::default(),
            })
            .unwrap();
        let recovered = Runtime::new(storage, backend);
        recovered.recover_sessions();
        let Response::Session(recovered_session) = recovered.handle(Request::GetSession { id })
        else {
            panic!("expected recovered session")
        };
        assert_eq!(recovered_session.state, SessionState::Exited);
    }

    #[test]
    fn create_publishes_lifecycle_events() {
        let runtime = runtime();
        let mut events = runtime.events().subscribe();
        let Response::Session(session) = runtime.handle(create("demo")) else {
            panic!("expected created session")
        };
        assert!(matches!(
            events.try_recv().unwrap(),
            Event::SessionCreated { .. }
        ));
        assert!(matches!(
            events.try_recv().unwrap(),
            Event::SessionStateChanged { session: value }
                if value.state == SessionState::Starting
        ));
        assert!(matches!(
            events.try_recv().unwrap(),
            Event::SessionStateChanged { session: value }
                if value.id == session.id && value.state == SessionState::Running
        ));
    }

    #[test]
    fn failed_create_rolls_back_persisted_metadata() {
        let runtime = runtime();
        let response = runtime.handle(create("demo"));
        assert!(matches!(response, Response::Session(_)));
        let response = runtime.handle(create("demo"));
        assert!(matches!(response, Response::Error { .. }));
        assert_eq!(
            runtime.handle(Request::ListSessions),
            Response::Sessions(vec![SessionSummary {
                id: anclave_protocol::SessionId::new("session-0").unwrap(),
                name: "demo".to_owned(),
                state: SessionState::Running,
                agent: AgentId::new("default").unwrap(),
                workspace: None,
                // Read back through the live configuration, like any client
                // listing sessions.
                security: anclave_protocol::SecurityPosture {
                    profile: "default".to_owned(),
                    contained: false,
                    summary: anclave_security::SecurityProfile::host().summary(),
                    caveats: anclave_security::SecurityProfile::host()
                        .caveats()
                        .into_iter()
                        .map(str::to_owned)
                        .collect(),
                },
            }])
        );
    }

    #[test]
    fn restart_recreates_the_backend_session() {
        let runtime = runtime();
        let Response::Session(session) = runtime.handle(create("demo")) else {
            panic!("expected created session")
        };
        assert!(matches!(
            runtime.handle(Request::RestartSession {
                id: session.id.clone()
            }),
            Response::Session(_)
        ));
    }

    #[test]
    fn restart_missing_session_returns_not_found() {
        let runtime = runtime();
        let response = runtime.handle(Request::RestartSession {
            id: anclave_protocol::SessionId::new("missing").unwrap(),
        });
        assert!(matches!(
            response,
            Response::Error {
                code: ErrorCode::NotFound,
                ..
            }
        ));
    }

    #[test]
    fn envelope_preserves_request_id() {
        let runtime = runtime();
        let request = Envelope::new(Some(RequestId::new("req-1").unwrap()), Request::Ping);
        let response = handle_envelope(&runtime, request);
        assert_eq!(response.request_id.unwrap().as_str(), "req-1");
        assert_eq!(response.payload, Response::Pong);
    }
}
