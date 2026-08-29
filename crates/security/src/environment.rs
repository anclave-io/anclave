//! Building a child process's environment explicitly.
//!
//! The rule is **construct, never inherit**: the returned map is built from
//! nothing, and a variable is present only because a policy put it there.
//! Inheriting and then removing is the same idea written the dangerous way
//! round — a credential variable nobody thought of leaks by default, and the
//! set of things nobody thought of grows with every new cloud provider.

use std::collections::BTreeMap;

use crate::{CredentialPolicy, SecurityProfile};

/// Variables every process needs to function at all.
///
/// Deliberately short. Anything not here has to earn its place, and `PATH`
/// aside these are about locale and terminal behaviour rather than authority.
const ESSENTIAL: &[&str] = &[
    "PATH", "HOME", "USER", "LOGNAME", "SHELL", "TERM", "LANG", "LC_ALL", "TZ", "TMPDIR",
];

/// Variables that carry authority, matched by exact name.
const CREDENTIAL_NAMES: &[&str] = &[
    "SSH_AUTH_SOCK",
    "SSH_AGENT_PID",
    "GITHUB_TOKEN",
    "GH_TOKEN",
    "GITLAB_TOKEN",
    "GIT_ASKPASS",
    "SSH_ASKPASS",
    "GIT_SSH_COMMAND",
    "NPM_TOKEN",
    "CARGO_REGISTRY_TOKEN",
    "DOCKER_AUTH_CONFIG",
    "KUBECONFIG",
    "ANTHROPIC_API_KEY",
    "OPENAI_API_KEY",
];

/// Prefixes whose whole family carries authority.
///
/// Prefix matching rather than an exhaustive list because these families grow:
/// `AWS_SESSION_TOKEN` did not always exist, and the failure mode of missing
/// one is a leaked credential.
const CREDENTIAL_PREFIXES: &[&str] = &[
    "AWS_",
    "AZURE_",
    "GOOGLE_",
    "GCP_",
    "GCLOUD_",
    "DIGITALOCEAN_",
    "CLOUDFLARE_",
    "VAULT_",
    "NPM_CONFIG_",
];

/// Whether a variable name carries authority.
pub fn is_credential(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    CREDENTIAL_NAMES.contains(&upper.as_str())
        || CREDENTIAL_PREFIXES
            .iter()
            .any(|prefix| upper.starts_with(prefix))
        || upper.ends_with("_TOKEN")
        || upper.ends_with("_SECRET")
        || upper.ends_with("_PASSWORD")
        || upper.ends_with("_API_KEY")
}

/// Build the environment a session's agent will run with.
///
/// `inherited` is the daemon's own environment. `identity` is the set of
/// `ANCLAVE_*` variables the session needs to know itself; these are opaque
/// identifiers, never secrets, and are always passed.
pub fn build_environment<I>(
    profile: &SecurityProfile,
    inherited: I,
    identity: &BTreeMap<String, String>,
) -> BTreeMap<String, String>
where
    I: IntoIterator<Item = (String, String)>,
{
    let mut environment = BTreeMap::new();

    for (name, value) in inherited {
        let keep = match profile.credentials {
            // Compatibility: the daemon's environment passes through, which is
            // exactly what makes this ambient trust rather than a policy.
            CredentialPolicy::Ambient => true,
            // Otherwise only the essentials, and never a credential — even one
            // that happens to be spelled like an essential.
            CredentialPolicy::None | CredentialPolicy::Files(_) => {
                ESSENTIAL.contains(&name.as_str()) && !is_credential(&name)
            }
        };
        if keep {
            environment.insert(name, value);
        }
    }

    // Identity last: it is Anclave's own, and no inherited value may shadow it.
    for (name, value) in identity {
        environment.insert(name.clone(), value.clone());
    }
    environment
}

/// Redact anything that looks like a credential, for logs and error messages.
///
/// Secrets must never reach a log, and the value is dropped entirely rather
/// than partially masked: a prefix of a token is still a leak, and length
/// alone tells an attacker something.
pub fn redact(environment: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    environment
        .iter()
        .map(|(name, value)| {
            let value = if is_credential(name) {
                "<redacted>".to_owned()
            } else {
                value.clone()
            };
            (name.clone(), value)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SecurityProfile;

    fn inherited() -> Vec<(String, String)> {
        [
            ("PATH", "/usr/bin"),
            ("HOME", "/home/me"),
            ("TERM", "xterm"),
            ("SSH_AUTH_SOCK", "/tmp/agent.sock"),
            ("AWS_SECRET_ACCESS_KEY", "shhh"),
            ("GITHUB_TOKEN", "ghp_xxx"),
            ("MY_SERVICE_TOKEN", "abc"),
            ("EDITOR", "vim"),
        ]
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
    }

    fn identity() -> BTreeMap<String, String> {
        BTreeMap::from([("ANCLAVE_SESSION".to_owned(), "session-1".to_owned())])
    }

    #[test]
    fn ambient_passes_the_daemon_environment_through() {
        let environment = build_environment(&SecurityProfile::host(), inherited(), &identity());
        assert_eq!(
            environment.get("SSH_AUTH_SOCK").map(String::as_str),
            Some("/tmp/agent.sock")
        );
        assert_eq!(
            environment.get("ANCLAVE_SESSION").map(String::as_str),
            Some("session-1")
        );
    }

    #[test]
    fn withholding_credentials_removes_every_family() {
        let profile = SecurityProfile {
            credentials: CredentialPolicy::None,
            ..SecurityProfile::host()
        };
        let environment = build_environment(&profile, inherited(), &identity());

        for denied in [
            "SSH_AUTH_SOCK",
            "AWS_SECRET_ACCESS_KEY",
            "GITHUB_TOKEN",
            "MY_SERVICE_TOKEN",
        ] {
            assert!(
                !environment.contains_key(denied),
                "{denied} must not reach the agent"
            );
        }
        // The essentials survive, or nothing runs.
        assert!(environment.contains_key("PATH"));
        assert!(environment.contains_key("HOME"));
        // And an ordinary non-essential is dropped too: the map is built, not
        // filtered, so anything unlisted is simply absent.
        assert!(!environment.contains_key("EDITOR"));
        assert!(environment.contains_key("ANCLAVE_SESSION"));
    }

    /// Construction, not subtraction: a credential variable invented tomorrow
    /// is absent because it was never added, not because someone listed it.
    #[test]
    fn an_unknown_variable_is_absent_by_default() {
        let profile = SecurityProfile {
            credentials: CredentialPolicy::None,
            ..SecurityProfile::host()
        };
        let inherited = vec![("BRAND_NEW_CLOUD_CREDENTIAL".to_owned(), "x".to_owned())];
        let environment = build_environment(&profile, inherited, &BTreeMap::new());
        assert!(environment.is_empty());
    }

    #[test]
    fn identity_cannot_be_shadowed_by_the_inherited_environment() {
        let hostile = vec![("ANCLAVE_SESSION".to_owned(), "impostor".to_owned())];
        let environment = build_environment(&SecurityProfile::host(), hostile, &identity());
        assert_eq!(
            environment.get("ANCLAVE_SESSION").map(String::as_str),
            Some("session-1")
        );
    }

    #[test]
    fn credential_names_are_recognised_by_family_and_suffix() {
        for name in [
            "SSH_AUTH_SOCK",
            "AWS_ACCESS_KEY_ID",
            "aws_secret_access_key",
            "GOOGLE_APPLICATION_CREDENTIALS",
            "SOME_TOKEN",
            "DB_PASSWORD",
            "SERVICE_API_KEY",
        ] {
            assert!(is_credential(name), "{name} should be treated as a secret");
        }
        for name in ["PATH", "HOME", "TERM", "EDITOR", "ANCLAVE_SESSION"] {
            assert!(!is_credential(name), "{name} is not a secret");
        }
    }

    #[test]
    fn redaction_drops_the_value_entirely() {
        let environment = BTreeMap::from([
            ("GITHUB_TOKEN".to_owned(), "ghp_secret".to_owned()),
            ("PATH".to_owned(), "/usr/bin".to_owned()),
        ]);
        let redacted = redact(&environment);
        assert_eq!(
            redacted.get("GITHUB_TOKEN").map(String::as_str),
            Some("<redacted>")
        );
        // Not even a prefix survives.
        assert!(!redacted.values().any(|value| value.contains("ghp_")));
        assert_eq!(redacted.get("PATH").map(String::as_str), Some("/usr/bin"));
    }
}
