//! Agent execution security: what a coding agent is allowed to reach.
//!
//! This is **not** the plugin sandbox and shares nothing with it. Plugin
//! isolation protects Anclave from its own extensions; this protects the host
//! and the user's credentials from the agent. Neither substitutes for the
//! other, and a grant in one confers nothing in the other.
//!
//! Every session has a profile, and the profile is inspectable. The default
//! profile provides **no containment at all** — it runs the agent on the host
//! with the user's authority — and the type system is arranged so that fact
//! has to be stated rather than assumed: see [`SandboxKind::contains`] and
//! [`SecurityProfile::containment`].

pub mod environment;

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// How an agent's process is confined.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxKind {
    /// The agent runs directly on the host, with the user's full authority.
    ///
    /// This is a compatibility mode, not a sandbox. It is the default because
    /// the alternative is refusing to run at all on a machine with no
    /// container runtime — but it must be reported as uncontained everywhere
    /// it is used.
    #[default]
    Host,
    /// An OS-level container.
    Container,
    /// A hardware-virtualised guest.
    MicroVm,
}

impl SandboxKind {
    /// Whether this kind confines the process at all.
    ///
    /// Exists so "is this contained?" is answered by one function rather than
    /// by each call site remembering that `Host` is special.
    pub fn contains(self) -> bool {
        match self {
            Self::Host => false,
            Self::Container | Self::MicroVm => true,
        }
    }

    /// A short phrase for a person reading a session's posture.
    pub fn describe(self) -> &'static str {
        match self {
            Self::Host => "host (ambient trust — no containment)",
            Self::Container => "container",
            Self::MicroVm => "microVM",
        }
    }
}

/// What the agent may see of the filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilesystemPolicy {
    /// Everything the user can reach.
    #[default]
    Host,
    /// The session's workspace, writable; nothing else.
    Workspace,
    /// The session's workspace, read-only.
    WorkspaceReadOnly,
}

/// What the agent may reach over the network.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "mode", content = "hosts")]
pub enum NetworkPolicy {
    /// Unrestricted.
    #[default]
    Full,
    /// No network at all.
    None,
    /// Only these hosts.
    Allowlist(Vec<String>),
    /// Only through a proxy the daemon controls.
    ProxyOnly,
}

/// Which of the user's credentials the agent inherits.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "mode", content = "files")]
pub enum CredentialPolicy {
    /// Whatever is in the daemon's environment — SSH agent socket, cloud
    /// variables, git credentials. The compatibility default, and the reason
    /// `Host` mode is called ambient trust.
    #[default]
    Ambient,
    /// Nothing.
    None,
    /// Only these files, mounted read-only.
    Files(Vec<String>),
}

/// Who approves a policy-sensitive action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalPolicy {
    /// The agent's own UI decides. Anclave is not consulted.
    #[default]
    Agent,
    /// Anclave asks, through the approval broker, below the agent.
    Anclave,
}

/// What survives the session ending.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PersistencePolicy {
    /// The workspace outlives the session.
    #[default]
    Workspace,
    /// Everything is discarded when the session ends.
    Ephemeral,
}

/// A session's complete, inspectable security posture.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SecurityProfile {
    pub sandbox: SandboxKind,
    pub filesystem: FilesystemPolicy,
    pub network: NetworkPolicy,
    pub credentials: CredentialPolicy,
    pub approval: ApprovalPolicy,
    pub persistence: PersistencePolicy,
}

/// Why a profile's combination cannot be honoured.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProfileError {
    #[error(
        "profile '{0}' asks for {1} but runs on the host, which cannot enforce it; \
         either set a sandbox or state the weaker policy explicitly"
    )]
    UncontainedEnforcement(String, &'static str),
    #[error("unknown security profile: {0}")]
    Unknown(String),
    #[error("invalid security configuration: {0}")]
    Invalid(String),
}

impl SecurityProfile {
    /// The compatibility profile: no containment, ambient credentials.
    pub fn host() -> Self {
        Self::default()
    }

    /// A profile that reaches nothing it was not given.
    pub fn untrusted() -> Self {
        Self {
            sandbox: SandboxKind::Container,
            filesystem: FilesystemPolicy::Workspace,
            network: NetworkPolicy::None,
            credentials: CredentialPolicy::None,
            approval: ApprovalPolicy::Anclave,
            persistence: PersistencePolicy::Workspace,
        }
    }

    /// Whether this profile actually confines the agent.
    pub fn containment(&self) -> bool {
        self.sandbox.contains()
    }

    /// One line describing the posture, for a CLI or a status panel.
    pub fn summary(&self) -> String {
        format!(
            "sandbox={} network={} credentials={} approval={}",
            self.sandbox.describe(),
            match &self.network {
                NetworkPolicy::Full => "full".to_owned(),
                NetworkPolicy::None => "none".to_owned(),
                NetworkPolicy::ProxyOnly => "proxy-only".to_owned(),
                NetworkPolicy::Allowlist(hosts) => format!("allowlist({})", hosts.len()),
            },
            match &self.credentials {
                CredentialPolicy::Ambient => "ambient".to_owned(),
                CredentialPolicy::None => "none".to_owned(),
                CredentialPolicy::Files(files) => format!("files({})", files.len()),
            },
            match self.approval {
                ApprovalPolicy::Agent => "agent",
                ApprovalPolicy::Anclave => "anclave",
            },
        )
    }

    /// What this profile does **not** enforce, in the user's words.
    ///
    /// A posture is only honest if its gaps are as visible as its controls.
    /// Empty means the profile enforces everything it declares.
    pub fn caveats(&self) -> Vec<&'static str> {
        let mut caveats = Vec::new();
        if !self.sandbox.contains() {
            caveats.push(
                "runs on the host with your full authority: it can read and write \
                 anything you can, and reach any network you can",
            );
            if self.credentials != CredentialPolicy::Ambient {
                caveats.push(
                    "credential variables are withheld from the environment, but \
                     credential *files* on disk remain readable without a \
                     filesystem policy",
                );
            }
        }
        if self.approval == ApprovalPolicy::Agent {
            caveats.push("destructive actions are approved by the agent, not by Anclave");
        }
        caveats
    }

    /// Reject combinations that promise enforcement the sandbox cannot deliver.
    ///
    /// This is the check that keeps the security model honest. A profile
    /// saying `network = "none"` while running on the host would read as
    /// enforced and be enforced by nothing — the exact confusion this codebase
    /// exists to prevent. Better to refuse to load than to display a control
    /// that does not exist.
    pub fn validate(&self, name: &str) -> Result<(), ProfileError> {
        if self.sandbox.contains() {
            return Ok(());
        }
        let unenforceable = match () {
            _ if self.network != NetworkPolicy::Full => Some("a restricted network"),
            _ if self.filesystem != FilesystemPolicy::Host => Some("a restricted filesystem"),
            // Credentials are deliberately absent from this list. The daemon
            // *builds* the child environment, so withholding SSH_AUTH_SOCK and
            // the cloud variables is real enforcement even on the host. What
            // the host cannot do is stop the agent reading a credential file
            // off disk — that needs a filesystem policy, and `caveats` says so
            // rather than the profile quietly overclaiming.
            _ => None,
        };
        match unenforceable {
            Some(what) => Err(ProfileError::UncontainedEnforcement(name.to_owned(), what)),
            None => Ok(()),
        }
    }
}

/// The configured set of profiles, and which one is used by default.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SecurityConfig {
    #[serde(default = "default_profile_name")]
    pub default: String,
    #[serde(default)]
    pub profiles: BTreeMap<String, SecurityProfile>,
}

fn default_profile_name() -> String {
    "default".to_owned()
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            default: default_profile_name(),
            profiles: BTreeMap::from([
                ("default".to_owned(), SecurityProfile::host()),
                ("untrusted".to_owned(), SecurityProfile::untrusted()),
            ]),
        }
    }
}

impl SecurityConfig {
    /// Parse and validate a configuration.
    ///
    /// Every profile is validated, not just the default one: a profile that
    /// cannot be honoured is a problem when it is written, not when it is
    /// first selected months later.
    pub fn parse(text: &str) -> Result<Self, ProfileError> {
        let config: Self =
            toml::from_str(text).map_err(|error| ProfileError::Invalid(error.to_string()))?;
        for (name, profile) in &config.profiles {
            profile.validate(name)?;
        }
        if !config.profiles.contains_key(&config.default) {
            return Err(ProfileError::Unknown(config.default.clone()));
        }
        Ok(config)
    }

    pub fn get(&self, name: &str) -> Result<&SecurityProfile, ProfileError> {
        self.profiles
            .get(name)
            .ok_or_else(|| ProfileError::Unknown(name.to_owned()))
    }

    pub fn default_profile(&self) -> &SecurityProfile {
        self.profiles
            .get(&self.default)
            .expect("the default profile exists: parse() and Default both guarantee it")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_profile_provides_no_containment_and_says_so() {
        let profile = SecurityProfile::host();
        assert!(!profile.containment());
        assert!(profile.summary().contains("no containment"));
    }

    #[test]
    fn the_untrusted_profile_contains() {
        let profile = SecurityProfile::untrusted();
        assert!(profile.containment());
        assert!(profile.validate("untrusted").is_ok());
    }

    /// The central rule: the host cannot enforce a restriction, so a profile
    /// must not claim one.
    #[test]
    fn host_mode_cannot_claim_enforcement_it_does_not_have() {
        for (label, profile) in [
            (
                "network",
                SecurityProfile {
                    network: NetworkPolicy::None,
                    ..SecurityProfile::host()
                },
            ),
            (
                "filesystem",
                SecurityProfile {
                    filesystem: FilesystemPolicy::Workspace,
                    ..SecurityProfile::host()
                },
            ),
        ] {
            assert!(
                matches!(
                    profile.validate(label),
                    Err(ProfileError::UncontainedEnforcement(..))
                ),
                "host + restricted {label} must be refused"
            );
        }
    }

    /// Withholding credential *variables* is real enforcement even on the
    /// host, because the daemon builds the child environment itself. The
    /// filesystem gap it leaves must be stated, not hidden.
    #[test]
    fn host_mode_may_withhold_credentials_but_says_what_it_cannot_do() {
        let profile = SecurityProfile {
            credentials: CredentialPolicy::None,
            ..SecurityProfile::host()
        };
        assert!(profile.validate("compat-nocreds").is_ok());
        assert!(profile
            .caveats()
            .iter()
            .any(|caveat| caveat.contains("credential *files*")));
    }

    #[test]
    fn a_contained_profile_has_fewer_caveats_than_the_host_one() {
        assert!(!SecurityProfile::host().caveats().is_empty());
        assert!(SecurityProfile::untrusted().caveats().is_empty());
    }

    #[test]
    fn a_contained_profile_may_restrict_anything() {
        let profile = SecurityProfile {
            sandbox: SandboxKind::MicroVm,
            filesystem: FilesystemPolicy::WorkspaceReadOnly,
            network: NetworkPolicy::Allowlist(vec!["example.test".to_owned()]),
            credentials: CredentialPolicy::None,
            approval: ApprovalPolicy::Anclave,
            persistence: PersistencePolicy::Ephemeral,
        };
        assert!(profile.validate("locked").is_ok());
    }

    #[test]
    fn the_seeded_configuration_parses_and_validates() {
        let config = SecurityConfig::default();
        assert!(!config.default_profile().containment());
        assert!(config.get("untrusted").unwrap().containment());
        assert!(config.get("nope").is_err());
    }

    #[test]
    fn profiles_round_trip_through_toml() {
        let text = r#"
default = "locked"

[profiles.locked]
sandbox = "container"
filesystem = "workspace"
network = { mode = "allowlist", hosts = ["crates.io"] }
credentials = { mode = "files", files = ["/home/me/.netrc"] }
approval = "anclave"
persistence = "ephemeral"

[profiles.compat]
sandbox = "host"
"#;
        let config = SecurityConfig::parse(text).unwrap();
        assert_eq!(config.default, "locked");
        let locked = config.get("locked").unwrap();
        assert!(locked.containment());
        assert_eq!(
            locked.network,
            NetworkPolicy::Allowlist(vec!["crates.io".to_owned()])
        );
        // An omitted field takes the compatibility default, not a strict one:
        // a partially written profile must not look contained.
        assert!(!config.get("compat").unwrap().containment());
    }

    #[test]
    fn an_unenforceable_profile_fails_to_load_rather_than_misleading() {
        let text = r#"
default = "compat"

[profiles.compat]
sandbox = "host"
network = { mode = "none" }
"#;
        assert!(matches!(
            SecurityConfig::parse(text),
            Err(ProfileError::UncontainedEnforcement(..))
        ));
    }

    #[test]
    fn a_default_naming_a_missing_profile_is_refused() {
        let text = r#"
default = "absent"

[profiles.compat]
sandbox = "host"
"#;
        assert!(matches!(
            SecurityConfig::parse(text),
            Err(ProfileError::Unknown(_))
        ));
    }
}
