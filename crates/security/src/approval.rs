//! Approving actions independently of the agent's own interface.
//!
//! # What this can and cannot gate
//!
//! The daemon cannot intercept an agent running `git push --force`: that
//! happens inside the agent's own process, and inspecting its command line to
//! guess intent is exactly the shell-string parsing the plan forbids treating
//! as a boundary. A command can be spelled a thousand ways and only one has
//! to be missed.
//!
//! What *can* be gated is what the **daemon does on the agent's behalf**:
//! issuing a credential, destroying a workspace, widening a network policy.
//! Those cross a real interface the daemon owns, so a decision there is
//! enforced rather than requested.
//!
//! The consequence is a design constraint rather than a limitation to work
//! around: to make force-push approvable, the sandbox must not hold push
//! credentials and the agent must ask the daemon to push. Gating is a
//! property of who holds the capability, not of who reads the command.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use anclave_protocol::SessionId;

/// Something the daemon will not do without a decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Hand a session a credential for a scope.
    GrantCredential { scope: String },
    /// Remove a workspace that holds work.
    DestroyWorkspace { path: String },
    /// Give a session more network than its profile allows.
    WidenNetwork { from: String, to: String },
    /// Run a push the daemon performs for the agent.
    Push { repository: String, force: bool },
    /// Delete a branch the daemon manages.
    DeleteBranch { repository: String, branch: String },
}

impl Action {
    /// A description for a person deciding, not for a machine matching.
    pub fn describe(&self) -> String {
        match self {
            Self::GrantCredential { scope } => format!("grant credential: {scope}"),
            Self::DestroyWorkspace { path } => format!("destroy workspace {path}"),
            Self::WidenNetwork { from, to } => format!("widen network from {from} to {to}"),
            Self::Push {
                repository,
                force: true,
            } => format!("force-push {repository}"),
            Self::Push {
                repository,
                force: false,
            } => format!("push {repository}"),
            Self::DeleteBranch { repository, branch } => {
                format!("delete branch {branch} in {repository}")
            }
        }
    }

    /// Whether this action is destructive, so a client can say so loudly.
    pub fn is_destructive(&self) -> bool {
        match self {
            Self::DestroyWorkspace { .. } | Self::DeleteBranch { .. } => true,
            Self::Push { force, .. } => *force,
            Self::GrantCredential { .. } | Self::WidenNetwork { .. } => false,
        }
    }
}

/// How a pending approval ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Approved,
    Denied,
    /// Nobody decided in time. Distinct from denial: a client may want to
    /// retry a timeout and must not retry a refusal.
    Expired,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingApproval {
    pub id: String,
    pub session: SessionId,
    pub action: Action,
    pub requested_at: SystemTime,
    pub expires_at: SystemTime,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ApprovalError {
    #[error("no approval is pending with id {0}")]
    Unknown(String),
    #[error("approval {0} was already decided")]
    AlreadyDecided(String),
    #[error("this session may not decide approval {0}")]
    NotPermitted(String),
}

/// Holds approvals awaiting a decision.
///
/// Deliberately not a queue of one: several sessions can be blocked at once,
/// and a decision names the approval it answers so two prompts cannot be
/// confused for each other.
#[derive(Clone, Default)]
pub struct ApprovalBroker {
    pending: Arc<Mutex<HashMap<String, PendingApproval>>>,
    decided: Arc<Mutex<HashMap<String, Decision>>>,
    /// Every approval ever raised, so a decided one can still be matched to
    /// the action it answered.
    history: Arc<Mutex<Vec<PendingApproval>>>,
    next_id: Arc<Mutex<u64>>,
}

impl ApprovalBroker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register an action awaiting a decision, or return the one already
    /// standing for it.
    ///
    /// Idempotent per session and action while the answer is still live:
    /// pending, or already approved. A caller that asks, is refused, and
    /// retries must land on the *same* approval, or the decision a person
    /// just made would apply to an approval nobody will ask about again.
    ///
    /// A denied or expired request does not dedupe, so a timeout can be
    /// retried and a refusal can be reconsidered deliberately rather than by
    /// a caller that simply kept asking.
    pub fn request(&self, session: SessionId, action: Action, ttl: Duration) -> PendingApproval {
        if let Some(existing) = self.live_for(&session, &action) {
            return existing;
        }
        let mut next = self.next_id.lock().expect("approval id mutex");
        *next += 1;
        let id = format!("approval-{next}");
        drop(next);

        let requested_at = SystemTime::now();
        let pending = PendingApproval {
            id: id.clone(),
            session,
            action,
            requested_at,
            expires_at: requested_at + ttl,
        };
        self.pending
            .lock()
            .expect("approval mutex")
            .insert(id, pending.clone());
        self.history
            .lock()
            .expect("approval history mutex")
            .push(pending.clone());
        pending
    }

    /// Record a decision.
    ///
    /// A second decision on the same approval is an error rather than a
    /// silent overwrite: two clients racing must not produce a result that
    /// depends on arrival order, and an approval that was already denied must
    /// never be talked into approval by a later message.
    pub fn decide(&self, id: &str, decision: Decision) -> Result<(), ApprovalError> {
        let mut pending = self.pending.lock().expect("approval mutex");
        let mut decided = self.decided.lock().expect("approval decided mutex");

        if decided.contains_key(id) {
            return Err(ApprovalError::AlreadyDecided(id.to_owned()));
        }
        if pending.remove(id).is_none() {
            return Err(ApprovalError::Unknown(id.to_owned()));
        }
        decided.insert(id.to_owned(), decision);
        Ok(())
    }

    /// A pending or approved request for this exact session and action.
    fn live_for(&self, session: &SessionId, action: &Action) -> Option<PendingApproval> {
        let pending = self.pending.lock().expect("approval mutex");
        if let Some(found) = pending
            .values()
            .find(|approval| &approval.session == session && &approval.action == action)
        {
            return Some(found.clone());
        }
        drop(pending);

        let history = self.history.lock().expect("approval history mutex");
        let decided = self.decided.lock().expect("approval decided mutex");
        history
            .iter()
            .find(|approval| {
                &approval.session == session
                    && &approval.action == action
                    && decided.get(&approval.id) == Some(&Decision::Approved)
            })
            .cloned()
    }

    /// The decision, if one has been made.
    pub fn decision(&self, id: &str) -> Option<Decision> {
        self.decided
            .lock()
            .expect("approval decided mutex")
            .get(id)
            .copied()
    }

    /// Expire everything past its deadline, returning what expired.
    ///
    /// Expiry is a decision. An approval left pending forever blocks whatever
    /// asked for it, and "the operator went to lunch" must not read as
    /// consent.
    pub fn expire_due(&self, now: SystemTime) -> Vec<PendingApproval> {
        let mut pending = self.pending.lock().expect("approval mutex");
        let mut decided = self.decided.lock().expect("approval decided mutex");

        let due: Vec<PendingApproval> = pending
            .values()
            .filter(|approval| now >= approval.expires_at)
            .cloned()
            .collect();
        for approval in &due {
            pending.remove(&approval.id);
            decided.insert(approval.id.clone(), Decision::Expired);
        }
        due
    }

    pub fn pending(&self) -> Vec<PendingApproval> {
        let mut all: Vec<PendingApproval> = self
            .pending
            .lock()
            .expect("approval mutex")
            .values()
            .cloned()
            .collect();
        all.sort_by(|a, b| a.id.cmp(&b.id));
        all
    }

    /// Abandon everything for a session that has gone.
    pub fn forget_session(&self, session: &SessionId) {
        let mut pending = self.pending.lock().expect("approval mutex");
        let mut decided = self.decided.lock().expect("approval decided mutex");
        let gone: Vec<String> = pending
            .values()
            .filter(|approval| &approval.session == session)
            .map(|approval| approval.id.clone())
            .collect();
        for id in gone {
            pending.remove(&id);
            decided.insert(id, Decision::Expired);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> SessionId {
        SessionId::new("session-1").unwrap()
    }

    fn action() -> Action {
        Action::DestroyWorkspace {
            path: "/w".to_owned(),
        }
    }

    #[test]
    fn an_approval_is_pending_until_it_is_decided() {
        let broker = ApprovalBroker::new();
        let pending = broker.request(session(), action(), Duration::from_secs(60));

        assert_eq!(broker.pending().len(), 1);
        assert_eq!(broker.decision(&pending.id), None);

        broker.decide(&pending.id, Decision::Approved).unwrap();
        assert_eq!(broker.decision(&pending.id), Some(Decision::Approved));
        assert!(broker.pending().is_empty());
    }

    #[test]
    fn denial_is_recorded_as_denial() {
        let broker = ApprovalBroker::new();
        let pending = broker.request(session(), action(), Duration::from_secs(60));
        broker.decide(&pending.id, Decision::Denied).unwrap();
        assert_eq!(broker.decision(&pending.id), Some(Decision::Denied));
    }

    /// Two clients racing must not produce an order-dependent result, and a
    /// denial must never be overturned by a later approval.
    #[test]
    fn an_approval_cannot_be_decided_twice() {
        let broker = ApprovalBroker::new();
        let pending = broker.request(session(), action(), Duration::from_secs(60));

        broker.decide(&pending.id, Decision::Denied).unwrap();
        assert_eq!(
            broker.decide(&pending.id, Decision::Approved),
            Err(ApprovalError::AlreadyDecided(pending.id.clone()))
        );
        assert_eq!(broker.decision(&pending.id), Some(Decision::Denied));
    }

    #[test]
    fn deciding_an_unknown_approval_is_refused() {
        let broker = ApprovalBroker::new();
        assert_eq!(
            broker.decide("approval-nope", Decision::Approved),
            Err(ApprovalError::Unknown("approval-nope".to_owned()))
        );
    }

    /// Silence is not consent.
    #[test]
    fn an_expired_approval_is_not_approved() {
        let broker = ApprovalBroker::new();
        let pending = broker.request(session(), action(), Duration::from_secs(0));

        let expired = broker.expire_due(SystemTime::now() + Duration::from_secs(1));
        assert_eq!(expired.len(), 1);
        assert_eq!(broker.decision(&pending.id), Some(Decision::Expired));
        assert!(broker.pending().is_empty());
    }

    #[test]
    fn an_approval_still_in_time_is_not_expired() {
        let broker = ApprovalBroker::new();
        let pending = broker.request(session(), action(), Duration::from_secs(300));
        assert!(broker.expire_due(SystemTime::now()).is_empty());
        assert_eq!(broker.decision(&pending.id), None);
    }

    /// A client that vanishes leaves its request behind; nothing should stay
    /// blocked on a decision nobody can now make.
    #[test]
    fn a_departed_session_leaves_nothing_pending() {
        let broker = ApprovalBroker::new();
        let mine = broker.request(session(), action(), Duration::from_secs(300));
        let other = broker.request(
            SessionId::new("session-2").unwrap(),
            action(),
            Duration::from_secs(300),
        );

        broker.forget_session(&session());
        assert_eq!(broker.decision(&mine.id), Some(Decision::Expired));
        assert_eq!(
            broker.decision(&other.id),
            None,
            "another session's approval must survive"
        );
        assert_eq!(broker.pending().len(), 1);
    }

    #[test]
    fn several_approvals_are_distinguishable() {
        let broker = ApprovalBroker::new();
        let a = broker.request(session(), action(), Duration::from_secs(60));
        let b = broker.request(
            session(),
            Action::Push {
                repository: "r".to_owned(),
                force: true,
            },
            Duration::from_secs(60),
        );
        assert_ne!(a.id, b.id);

        broker.decide(&b.id, Decision::Approved).unwrap();
        assert_eq!(
            broker.decision(&a.id),
            None,
            "deciding one must not decide the other"
        );
    }

    /// A caller that is refused and retries must land on the same approval,
    /// or the decision a person just made would apply to a request nobody
    /// asks about again.
    #[test]
    fn asking_twice_for_the_same_action_returns_the_same_approval() {
        let broker = ApprovalBroker::new();
        let first = broker.request(session(), action(), Duration::from_secs(60));
        let second = broker.request(session(), action(), Duration::from_secs(60));
        assert_eq!(first.id, second.id);
        assert_eq!(broker.pending().len(), 1);
    }

    #[test]
    fn an_approved_action_stays_approved_when_asked_again() {
        let broker = ApprovalBroker::new();
        let first = broker.request(session(), action(), Duration::from_secs(60));
        broker.decide(&first.id, Decision::Approved).unwrap();

        let again = broker.request(session(), action(), Duration::from_secs(60));
        assert_eq!(again.id, first.id);
        assert_eq!(broker.decision(&again.id), Some(Decision::Approved));
    }

    /// A denial must not be dodged by asking again in a loop, but it must
    /// also not be permanent by accident: a fresh request is a new decision
    /// for a person to make, not an inherited yes.
    #[test]
    fn a_denied_action_can_be_raised_again_as_a_new_decision() {
        let broker = ApprovalBroker::new();
        let first = broker.request(session(), action(), Duration::from_secs(60));
        broker.decide(&first.id, Decision::Denied).unwrap();

        let again = broker.request(session(), action(), Duration::from_secs(60));
        assert_ne!(
            again.id, first.id,
            "a denial must not be reused as an answer"
        );
        assert_eq!(broker.decision(&again.id), None, "and must not inherit one");
    }

    #[test]
    fn different_actions_do_not_share_an_approval() {
        let broker = ApprovalBroker::new();
        let destroy = broker.request(session(), action(), Duration::from_secs(60));
        let push = broker.request(
            session(),
            Action::Push {
                repository: "r".to_owned(),
                force: true,
            },
            Duration::from_secs(60),
        );
        assert_ne!(destroy.id, push.id);
    }

    #[test]
    fn destructive_actions_are_marked_as_such() {
        assert!(Action::DestroyWorkspace { path: "/w".into() }.is_destructive());
        assert!(Action::Push {
            repository: "r".into(),
            force: true
        }
        .is_destructive());
        assert!(!Action::Push {
            repository: "r".into(),
            force: false
        }
        .is_destructive());
        assert!(!Action::GrantCredential { scope: "s".into() }.is_destructive());
    }

    #[test]
    fn actions_describe_themselves_for_a_person() {
        assert_eq!(
            Action::Push {
                repository: "anclave".into(),
                force: true
            }
            .describe(),
            "force-push anclave"
        );
    }
}
