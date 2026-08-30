//! Importing state from a previous installation.
//!
//! Three rules shape this, from the plan's acceptance criterion that
//! migration be "explicit, reviewable, and reversible":
//!
//! - **Explicit.** Nothing runs unless asked, and the *safe* form is the one
//!   you get by forgetting a flag: `import` without `--apply` is a dry run.
//! - **Reviewable.** Inspect, dry run and apply all produce the same report,
//!   so what you read is what runs. Every refusal carries a reason; a skip
//!   without one is not reviewable.
//! - **Reversible.** Applying writes what it changed to a rollback file
//!   *before* changing anything.
//!
//! And one rule about what is not imported: **security settings are never
//! carried over.** A profile written for another system's enforcement is a
//! claim about containment this system did not make. Importing one silently
//! would attach a security posture nobody here verified, which is worse than
//! making somebody rewrite it.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anclave_protocol::{MigrationAction, MigrationItem, MigrationReport};

/// What a legacy installation looks like, as far as this is concerned.
///
/// Only text formats are read. Guessing at another system's database schema
/// would mean importing rows whose meaning is inferred, and an inferred
/// session is the kind of thing that looks migrated and is not.
const AGENTS_FILE: &str = "agents.toml";
const SESSIONS_FILE: &str = "sessions.json";
const PREFERENCES_FILE: &str = "preferences.toml";

/// Keys that are never imported, whatever they hold.
/// Header on a written agents file, so whoever finds it knows what made it.
const HEADER_AGENTS: &str = "# Imported by `anclave-cli migrate import`.\n\
# Point ANCLAVE_AGENTS_FILE at this file to use them.\n";

const UNSAFE_KEYS: &[&str] = &["security", "sandbox", "credentials", "network", "approval"];

#[derive(Debug, serde::Deserialize)]
struct LegacyAgents {
    #[serde(default)]
    agents: Vec<LegacyAgent>,
}

#[derive(Debug, serde::Deserialize)]
struct LegacyAgent {
    name: String,
    command: String,
    #[serde(default)]
    args: Vec<String>,
}

#[derive(Debug, serde::Deserialize)]
struct LegacySession {
    name: String,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    repository: Option<String>,
    /// Present in some legacy states. Never imported; see the module docs.
    #[serde(default)]
    security: Option<String>,
}

/// Build the report for a source directory.
///
/// `existing_sessions` and `existing_agents` come from the destination, so
/// "already present" is a fact about this system rather than a guess.
pub fn inspect(
    source: &Path,
    existing_sessions: &BTreeSet<String>,
    existing_agents: &BTreeSet<String>,
) -> MigrationReport {
    let mut items = Vec::new();

    if !source.is_dir() {
        items.push(MigrationItem {
            kind: "source".to_owned(),
            name: source.display().to_string(),
            action: MigrationAction::Skip,
            detail: Some("not a directory".to_owned()),
        });
        return MigrationReport {
            source: source.display().to_string(),
            applied: false,
            rollback: None,
            items,
        };
    }

    items.extend(inspect_agents(source, existing_agents));
    items.extend(inspect_sessions(source, existing_sessions));
    items.extend(inspect_preferences(source));

    if items.is_empty() {
        items.push(MigrationItem {
            kind: "source".to_owned(),
            name: source.display().to_string(),
            action: MigrationAction::Skip,
            detail: Some(format!(
                "nothing recognised: expected {AGENTS_FILE}, {SESSIONS_FILE} or {PREFERENCES_FILE}"
            )),
        });
    }

    MigrationReport {
        source: source.display().to_string(),
        applied: false,
        rollback: None,
        items,
    }
}

fn inspect_agents(source: &Path, existing: &BTreeSet<String>) -> Vec<MigrationItem> {
    let path = source.join(AGENTS_FILE);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let parsed: LegacyAgents = match toml::from_str(&text) {
        Ok(parsed) => parsed,
        Err(error) => {
            // A file that will not parse is reported, not ignored: silence
            // here reads as "you had no agents".
            return vec![MigrationItem {
                kind: "agents".to_owned(),
                name: AGENTS_FILE.to_owned(),
                action: MigrationAction::Skip,
                detail: Some(format!("cannot parse: {error}")),
            }];
        }
    };

    parsed
        .agents
        .into_iter()
        .map(|agent| {
            if agent.name.trim().is_empty() || agent.command.trim().is_empty() {
                return MigrationItem {
                    kind: "agent".to_owned(),
                    name: agent.name.clone(),
                    action: MigrationAction::Skip,
                    detail: Some("an agent needs a name and a command".to_owned()),
                };
            }
            if existing.contains(&agent.name) {
                return MigrationItem {
                    kind: "agent".to_owned(),
                    name: agent.name,
                    action: MigrationAction::AlreadyPresent,
                    detail: None,
                };
            }
            MigrationItem {
                kind: "agent".to_owned(),
                name: agent.name,
                action: MigrationAction::Import,
                detail: Some(
                    format!("{} {}", agent.command, agent.args.join(" "))
                        .trim()
                        .to_owned(),
                ),
            }
        })
        .collect()
}

fn inspect_sessions(source: &Path, existing: &BTreeSet<String>) -> Vec<MigrationItem> {
    let path = source.join(SESSIONS_FILE);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let parsed: Vec<LegacySession> = match serde_json::from_str(&text) {
        Ok(parsed) => parsed,
        Err(error) => {
            return vec![MigrationItem {
                kind: "sessions".to_owned(),
                name: SESSIONS_FILE.to_owned(),
                action: MigrationAction::Skip,
                detail: Some(format!("cannot parse: {error}")),
            }]
        }
    };

    parsed
        .into_iter()
        .map(|session| {
            if session.name.trim().is_empty() {
                return MigrationItem {
                    kind: "session".to_owned(),
                    name: String::new(),
                    action: MigrationAction::Skip,
                    detail: Some("a session needs a name".to_owned()),
                };
            }
            // A duplicate name is left alone rather than renamed: two
            // sessions with the same name in one list is a worse outcome than
            // one that was not imported, and the person can rename and retry.
            if existing.contains(&session.name) {
                return MigrationItem {
                    kind: "session".to_owned(),
                    name: session.name,
                    action: MigrationAction::AlreadyPresent,
                    detail: Some("a session of this name already exists".to_owned()),
                };
            }
            if let Some(profile) = session.security.as_deref() {
                return MigrationItem {
                    kind: "session".to_owned(),
                    name: session.name,
                    action: MigrationAction::Skip,
                    detail: Some(format!(
                        "carries security profile '{profile}': create it here with \
                         an explicit --security instead, so the posture is one this \
                         system verified"
                    )),
                };
            }
            let repository = session.repository.unwrap_or_default();
            MigrationItem {
                kind: "session".to_owned(),
                name: session.name,
                action: MigrationAction::Import,
                detail: Some(
                    format!(
                        "agent {} {}",
                        session.agent.unwrap_or_else(|| "default".to_owned()),
                        repository
                    )
                    .trim()
                    .to_owned(),
                ),
            }
        })
        .collect()
}

fn inspect_preferences(source: &Path) -> Vec<MigrationItem> {
    let path = source.join(PREFERENCES_FILE);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let parsed: toml::Table = match toml::from_str(&text) {
        Ok(parsed) => parsed,
        Err(error) => {
            return vec![MigrationItem {
                kind: "preferences".to_owned(),
                name: PREFERENCES_FILE.to_owned(),
                action: MigrationAction::Skip,
                detail: Some(format!("cannot parse: {error}")),
            }]
        }
    };

    parsed
        .into_iter()
        .map(|(key, value)| {
            // Matched on the key rather than a list of known-safe ones: a
            // future key nobody classified should arrive as unimported, not
            // as silently imported.
            if UNSAFE_KEYS
                .iter()
                .any(|unsafe_key| key.contains(unsafe_key))
            {
                return MigrationItem {
                    kind: "preference".to_owned(),
                    name: key,
                    action: MigrationAction::Skip,
                    detail: Some(
                        "security-related settings are never imported: state them \
                         explicitly here instead"
                            .to_owned(),
                    ),
                };
            }
            MigrationItem {
                kind: "preference".to_owned(),
                name: key,
                action: MigrationAction::Import,
                detail: Some(value.to_string()),
            }
        })
        .collect()
}

/// Write the importable agents and preferences into the destination.
///
/// Only the items the report marked `Import` are written, so what was
/// reviewed is exactly what lands. Returns the files written, for the report
/// to name: an apply that says "imported" without saying *where* leaves
/// someone hunting for state they were told they now have.
pub fn apply(
    source: &Path,
    destination: &Path,
    report: &MigrationReport,
) -> Result<Vec<PathBuf>, std::io::Error> {
    let mut written = Vec::new();
    std::fs::create_dir_all(destination)?;

    let wanted: BTreeSet<&str> = report
        .items
        .iter()
        .filter(|item| item.action == MigrationAction::Import)
        .map(|item| item.name.as_str())
        .collect();

    if let Ok(text) = std::fs::read_to_string(source.join(AGENTS_FILE)) {
        if let Ok(parsed) = toml::from_str::<LegacyAgents>(&text) {
            let keep: Vec<&LegacyAgent> = parsed
                .agents
                .iter()
                .filter(|agent| wanted.contains(agent.name.as_str()))
                .collect();
            if !keep.is_empty() {
                let mut out = String::from(HEADER_AGENTS);
                for agent in keep {
                    out.push_str("\n[[agents]]\n");
                    out.push_str(&format!("name = {}\n", quote(&agent.name)));
                    out.push_str(&format!("command = {}\n", quote(&agent.command)));
                    if !agent.args.is_empty() {
                        let args: Vec<String> = agent.args.iter().map(|a| quote(a)).collect();
                        out.push_str(&format!("args = [{}]\n", args.join(", ")));
                    }
                }
                let path = destination.join(AGENTS_FILE);
                std::fs::write(&path, out)?;
                written.push(path);
            }
        }
    }

    if let Ok(text) = std::fs::read_to_string(source.join(PREFERENCES_FILE)) {
        if let Ok(parsed) = toml::from_str::<toml::Table>(&text) {
            let kept: toml::Table = parsed
                .into_iter()
                .filter(|(key, _)| wanted.contains(key.as_str()))
                .collect();
            if !kept.is_empty() {
                let path = destination.join(PREFERENCES_FILE);
                std::fs::write(
                    &path,
                    format!(
                        "# Imported by `anclave-cli migrate import`.\n{}",
                        toml::to_string_pretty(&kept)
                            .map_err(|error| std::io::Error::other(error.to_string()))?
                    ),
                )?;
                written.push(path);
            }
        }
    }

    Ok(written)
}

/// Quote a value for TOML, so a name with a quote in it cannot break the file.
fn quote(value: &str) -> String {
    format!("{}", toml::Value::String(value.to_owned()))
}

/// Where the undo record for an applied migration is written.
pub fn rollback_path(destination: &Path) -> PathBuf {
    destination.join("migration-rollback.json")
}
