//! Credentials as issued resources rather than ambient inheritance.
//!
//! The distinction the plan draws: an agent should *hold a grant* that was
//! issued to it for a scope and a duration, not simply find a socket in its
//! environment. A grant can be listed, expired and revoked; ambient
//! inheritance can be none of those things.
//!
//! **No secret value is ever stored in a grant.** A grant records what was
//! issued, to whom, for how long — the material itself stays with the
//! provider and reaches the agent through the environment or a mount. That is
//! what lets a grant be logged, persisted and shown to a person without the
//! log becoming the thing an attacker wants.

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime};

use anclave_protocol::SessionId;

/// What a session is asking to be given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialRequest {
    pub session: SessionId,
    pub scope: CredentialScope,
    /// How long the grant should live. The provider may issue less, never
    /// more.
    pub requested_ttl: Duration,
}

/// What a credential is *for*.
///
/// A scope, not a secret: "read this repository" rather than a token. The
/// provider decides what material satisfies it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum CredentialScope {
    /// Read access to a named repository.
    RepositoryRead(String),
    /// Push access to a named repository — separated from read because these
    /// are very different things to hand an autonomous process.
    RepositoryWrite(String),
    /// A named file, mounted read-only.
    File(String),
}

impl CredentialScope {
    /// A description safe to put in a log or on a screen.
    pub fn describe(&self) -> String {
        match self {
            Self::RepositoryRead(name) => format!("read {name}"),
            Self::RepositoryWrite(name) => format!("write {name}"),
            Self::File(path) => format!("file {path}"),
        }
    }
}

/// Proof that something was issued. Carries no secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialGrant {
    pub id: String,
    pub session: SessionId,
    pub scope: CredentialScope,
    pub issued_at: SystemTime,
    pub expires_at: SystemTime,
    /// Environment variables the agent should receive, by name only. The
    /// values live in the provider, never here.
    pub variables: Vec<String>,
}

impl CredentialGrant {
    pub fn is_expired_at(&self, now: SystemTime) -> bool {
        now >= self.expires_at
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CredentialError {
    #[error("this provider issues nothing")]
    NotAvailable,
    #[error("scope not permitted for this session: {0}")]
    OutOfScope(String),
    #[error("no such grant: {0}")]
    UnknownGrant(String),
    #[error("credential provider failed: {0}")]
    Failed(String),
}

/// Issues and revokes credentials.
///
/// Deliberately synchronous, matching the rest of the daemon's interfaces at
/// this stage; the daemon runs these off its request path.
pub trait CredentialProvider: Send + Sync {
    fn issue(&self, request: &CredentialRequest) -> Result<CredentialGrant, CredentialError>;
    fn revoke(&self, grant: &CredentialGrant) -> Result<(), CredentialError>;
    /// Values for a live grant's variables, fetched at launch and never
    /// persisted.
    fn materialize(
        &self,
        grant: &CredentialGrant,
    ) -> Result<BTreeMap<String, String>, CredentialError>;
}

/// The provider for `CredentialPolicy::None`: issues nothing, ever.
///
/// Not a stub — this is the correct provider for a profile that grants no
/// credentials, and having it be a real implementation means the "no
/// credentials" path goes through the same interface as every other.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoCredentials;

impl CredentialProvider for NoCredentials {
    fn issue(&self, _request: &CredentialRequest) -> Result<CredentialGrant, CredentialError> {
        Err(CredentialError::NotAvailable)
    }

    fn revoke(&self, _grant: &CredentialGrant) -> Result<(), CredentialError> {
        Ok(())
    }

    fn materialize(
        &self,
        _grant: &CredentialGrant,
    ) -> Result<BTreeMap<String, String>, CredentialError> {
        Ok(BTreeMap::new())
    }
}

/// Issues read-only access to files the operator selected in advance.
///
/// The only provider that hands out anything real today. It reads from disk
/// at materialize time so a rotated file is picked up without reissuing, and
/// it refuses any path the operator did not list — the allowlist is the whole
/// control, so a request naming a path is a request to be checked, never a
/// path to be opened.
#[derive(Debug, Clone, Default)]
pub struct SelectedFiles {
    allowed: BTreeMap<String, String>,
    ttl: Duration,
}

impl SelectedFiles {
    /// `entries` maps an environment variable name to the file whose contents
    /// it receives.
    pub fn new(entries: BTreeMap<String, String>, ttl: Duration) -> Self {
        Self {
            allowed: entries,
            ttl,
        }
    }

    fn variable_for(&self, path: &str) -> Option<&String> {
        self.allowed
            .iter()
            .find(|(_, allowed)| allowed.as_str() == path)
            .map(|(variable, _)| variable)
    }
}

impl CredentialProvider for SelectedFiles {
    fn issue(&self, request: &CredentialRequest) -> Result<CredentialGrant, CredentialError> {
        let CredentialScope::File(path) = &request.scope else {
            return Err(CredentialError::OutOfScope(request.scope.describe()));
        };
        let variable = self
            .variable_for(path)
            .ok_or_else(|| CredentialError::OutOfScope(request.scope.describe()))?;

        // The provider caps the lifetime; a caller cannot ask for longer.
        let ttl = request.requested_ttl.min(self.ttl);
        let issued_at = SystemTime::now();
        Ok(CredentialGrant {
            id: format!("{}-{}", request.session, variable),
            session: request.session.clone(),
            scope: request.scope.clone(),
            issued_at,
            expires_at: issued_at + ttl,
            variables: vec![variable.clone()],
        })
    }

    fn revoke(&self, _grant: &CredentialGrant) -> Result<(), CredentialError> {
        // Nothing to withdraw: the material is only ever read at launch, so a
        // revoked grant simply stops being materialized.
        Ok(())
    }

    fn materialize(
        &self,
        grant: &CredentialGrant,
    ) -> Result<BTreeMap<String, String>, CredentialError> {
        if grant.is_expired_at(SystemTime::now()) {
            return Err(CredentialError::UnknownGrant(grant.id.clone()));
        }
        let mut values = BTreeMap::new();
        for variable in &grant.variables {
            let path = self
                .allowed
                .get(variable)
                .ok_or_else(|| CredentialError::UnknownGrant(grant.id.clone()))?;
            let value = std::fs::read_to_string(path)
                .map_err(|error| CredentialError::Failed(format!("{path}: {error}")))?;
            values.insert(variable.clone(), value.trim_end().to_owned());
        }
        Ok(values)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> SessionId {
        SessionId::new("session-1").unwrap()
    }

    fn request(scope: CredentialScope) -> CredentialRequest {
        CredentialRequest {
            session: session(),
            scope,
            requested_ttl: Duration::from_secs(3600),
        }
    }

    #[test]
    fn the_empty_provider_issues_nothing() {
        let provider = NoCredentials;
        assert_eq!(
            provider.issue(&request(CredentialScope::File("/etc/passwd".to_owned()))),
            Err(CredentialError::NotAvailable)
        );
    }

    #[test]
    fn a_path_the_operator_did_not_select_is_refused() {
        let provider = SelectedFiles::new(BTreeMap::new(), Duration::from_secs(60));
        assert!(matches!(
            provider.issue(&request(CredentialScope::File("/etc/shadow".to_owned()))),
            Err(CredentialError::OutOfScope(_))
        ));
    }

    #[test]
    fn a_scope_this_provider_does_not_serve_is_refused() {
        let provider = SelectedFiles::new(BTreeMap::new(), Duration::from_secs(60));
        assert!(matches!(
            provider.issue(&request(CredentialScope::RepositoryWrite(
                "repo".to_owned()
            ))),
            Err(CredentialError::OutOfScope(_))
        ));
    }

    #[test]
    fn a_selected_file_is_issued_and_read_at_materialize_time() {
        let path = std::env::temp_dir().join(format!("anclave-cred-{}", std::process::id()));
        std::fs::write(&path, "s3cret\n").unwrap();
        let path_string = path.to_string_lossy().into_owned();

        let provider = SelectedFiles::new(
            BTreeMap::from([("TOKEN".to_owned(), path_string.clone())]),
            Duration::from_secs(60),
        );
        let grant = provider
            .issue(&request(CredentialScope::File(path_string)))
            .unwrap();
        assert_eq!(grant.variables, vec!["TOKEN".to_owned()]);

        let values = provider.materialize(&grant).unwrap();
        assert_eq!(values.get("TOKEN").map(String::as_str), Some("s3cret"));

        // Rotation is picked up without reissuing, because the value is read
        // at launch rather than captured at issue.
        std::fs::write(&path, "rotated\n").unwrap();
        assert_eq!(
            provider.materialize(&grant).unwrap().get("TOKEN"),
            Some(&"rotated".to_owned())
        );
        let _ = std::fs::remove_file(path);
    }

    /// The grant is the thing that gets logged and persisted, so it must not
    /// be able to carry the secret in the first place.
    #[test]
    fn a_grant_records_variable_names_and_never_values() {
        let path = std::env::temp_dir().join(format!("anclave-cred-v-{}", std::process::id()));
        std::fs::write(&path, "top-secret-value").unwrap();
        let path_string = path.to_string_lossy().into_owned();
        let provider = SelectedFiles::new(
            BTreeMap::from([("TOKEN".to_owned(), path_string.clone())]),
            Duration::from_secs(60),
        );
        let grant = provider
            .issue(&request(CredentialScope::File(path_string)))
            .unwrap();
        assert!(!format!("{grant:?}").contains("top-secret-value"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn the_provider_caps_the_requested_lifetime() {
        let path = std::env::temp_dir().join(format!("anclave-cred-t-{}", std::process::id()));
        std::fs::write(&path, "x").unwrap();
        let path_string = path.to_string_lossy().into_owned();
        let provider = SelectedFiles::new(
            BTreeMap::from([("TOKEN".to_owned(), path_string.clone())]),
            Duration::from_secs(30),
        );
        let grant = provider
            .issue(&CredentialRequest {
                session: session(),
                scope: CredentialScope::File(path_string),
                // Ask for a year.
                requested_ttl: Duration::from_secs(31_536_000),
            })
            .unwrap();
        let lifetime = grant.expires_at.duration_since(grant.issued_at).unwrap();
        assert!(lifetime <= Duration::from_secs(30), "{lifetime:?}");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn an_expired_grant_materializes_nothing() {
        let path = std::env::temp_dir().join(format!("anclave-cred-e-{}", std::process::id()));
        std::fs::write(&path, "x").unwrap();
        let path_string = path.to_string_lossy().into_owned();
        let provider = SelectedFiles::new(
            BTreeMap::from([("TOKEN".to_owned(), path_string)]),
            Duration::from_secs(60),
        );
        let issued_at = SystemTime::now() - Duration::from_secs(120);
        let expired = CredentialGrant {
            id: "old".to_owned(),
            session: session(),
            scope: CredentialScope::File("x".to_owned()),
            issued_at,
            expires_at: issued_at + Duration::from_secs(60),
            variables: vec!["TOKEN".to_owned()],
        };
        assert!(expired.is_expired_at(SystemTime::now()));
        assert!(provider.materialize(&expired).is_err());
        let _ = std::fs::remove_file(path);
    }
}
