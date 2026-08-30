//! Migration decisions, probed one shape at a time.
//!
//! The acceptance criterion is that migration be "explicit, reviewable, and
//! reversible", and each of those is a property a test can hold:
//! *explicit* means nothing is imported that was not reported; *reviewable*
//! means every refusal names its reason; *reversible* means a dry run writes
//! nothing and an apply leaves a record.

use std::collections::BTreeSet;

use anclave_protocol::{MigrationAction, MigrationReport};
use anclaved::migration::inspect;

fn source(files: &[(&str, &str)]) -> tempfile::TempDir {
    let directory = tempfile::tempdir().unwrap();
    for (name, body) in files {
        std::fs::write(directory.path().join(name), body).unwrap();
    }
    directory
}

fn report(files: &[(&str, &str)]) -> MigrationReport {
    let directory = source(files);
    inspect(directory.path(), &BTreeSet::new(), &BTreeSet::new())
}

fn find<'a>(report: &'a MigrationReport, name: &str) -> &'a anclave_protocol::MigrationItem {
    report
        .items
        .iter()
        .find(|item| item.name == name)
        .unwrap_or_else(|| panic!("no item named {name} in {:?}", report.items))
}

const AGENTS: &str = r#"
[[agents]]
name = "claude"
command = "claude"
args = ["--flag"]

[[agents]]
name = "shell"
command = "sh"
"#;

/// Agent definitions are the safe case and import cleanly.
#[test]
fn agent_definitions_are_imported() {
    let report = report(&[("agents.toml", AGENTS)]);
    assert_eq!(report.count(MigrationAction::Import), 2);
    assert_eq!(find(&report, "claude").action, MigrationAction::Import);
    assert!(find(&report, "claude")
        .detail
        .as_deref()
        .unwrap()
        .contains("--flag"));
}

/// What is already here is left alone, and said so.
#[test]
fn what_already_exists_is_reported_as_present() {
    let directory = source(&[("agents.toml", AGENTS)]);
    let existing = BTreeSet::from(["claude".to_owned()]);
    let report = inspect(directory.path(), &BTreeSet::new(), &existing);
    assert_eq!(
        find(&report, "claude").action,
        MigrationAction::AlreadyPresent
    );
    assert_eq!(find(&report, "shell").action, MigrationAction::Import);
}

/// A session whose name is taken is not imported and not renamed.
///
/// Two sessions with one name is a worse outcome than one not imported: the
/// person can rename and retry, but cannot un-confuse a duplicated list.
#[test]
fn a_duplicate_session_is_left_alone() {
    let directory = source(&[("sessions.json", r#"[{"name": "work"}, {"name": "fresh"}]"#)]);
    let existing = BTreeSet::from(["work".to_owned()]);
    let report = inspect(directory.path(), &existing, &BTreeSet::new());
    assert_eq!(
        find(&report, "work").action,
        MigrationAction::AlreadyPresent
    );
    assert!(
        find(&report, "work").detail.is_some(),
        "a skip needs a reason"
    );
    assert_eq!(find(&report, "fresh").action, MigrationAction::Import);
}

/// Security settings are never carried over, and say why.
///
/// A profile written for another system's enforcement is a claim about
/// containment this system did not make. Importing one silently would attach
/// a posture nobody here verified.
#[test]
fn security_settings_are_never_imported() {
    let report = report(&[
        (
            "sessions.json",
            r#"[{"name": "caged", "security": "untrusted"}]"#,
        ),
        (
            "preferences.toml",
            "theme = \"dark\"\nsecurity_profile = \"trusted\"\nnetwork_policy = \"full\"\n",
        ),
    ]);

    for name in ["caged", "security_profile", "network_policy"] {
        let item = find(&report, name);
        assert_eq!(
            item.action,
            MigrationAction::Skip,
            "{name} must not be imported"
        );
        assert!(
            item.detail.is_some(),
            "{name} was skipped without saying why"
        );
    }
    // An ordinary preference still comes across, or the rule is just
    // "import nothing".
    assert_eq!(find(&report, "theme").action, MigrationAction::Import);
}

/// A file that will not parse is reported, not passed over in silence.
///
/// Silence here reads as "you had no agents", which is the one wrong answer.
#[test]
fn invalid_data_is_reported_rather_than_ignored() {
    let report = report(&[
        ("agents.toml", "this is not toml {{{"),
        ("sessions.json", "{not json"),
    ]);
    for name in ["agents.toml", "sessions.json"] {
        let item = find(&report, name);
        assert_eq!(item.action, MigrationAction::Skip);
        assert!(
            item.detail.as_deref().unwrap().contains("cannot parse"),
            "{name}: {:?}",
            item.detail
        );
    }
}

/// A partial source imports what it has, rather than refusing the lot.
#[test]
fn a_partial_source_imports_what_it_has() {
    let report = report(&[("agents.toml", AGENTS)]);
    assert_eq!(report.count(MigrationAction::Import), 2);
    assert!(
        !report.items.iter().any(|item| item.kind == "session"),
        "a source with no sessions must not invent any"
    );
}

/// An entry missing what it needs is skipped with a reason.
#[test]
fn incomplete_entries_are_skipped_with_a_reason() {
    let report = report(&[
        ("agents.toml", "[[agents]]\nname = \"\"\ncommand = \"sh\"\n"),
        ("sessions.json", r#"[{"name": "   "}]"#),
    ]);
    assert_eq!(report.count(MigrationAction::Import), 0);
    for item in &report.items {
        assert_eq!(item.action, MigrationAction::Skip);
        assert!(item.detail.is_some(), "{item:?} was skipped silently");
    }
}

/// A source with nothing recognisable says so, naming what it looked for.
#[test]
fn an_unrecognised_source_says_what_it_expected() {
    let report = report(&[("notes.txt", "hello")]);
    assert_eq!(report.count(MigrationAction::Skip), 1);
    let detail = report.items[0].detail.as_deref().unwrap();
    for expected in ["agents.toml", "sessions.json", "preferences.toml"] {
        assert!(detail.contains(expected), "{detail}");
    }
}

/// A source that is not there is a reported skip, not a panic.
#[test]
fn a_missing_source_is_reported() {
    let report = inspect(
        std::path::Path::new("/nonexistent/anclave/legacy"),
        &BTreeSet::new(),
        &BTreeSet::new(),
    );
    assert_eq!(report.count(MigrationAction::Skip), 1);
    assert!(!report.applied);
}

/// Inspecting never claims to have applied anything.
#[test]
fn inspect_is_read_only() {
    let directory = source(&[("agents.toml", AGENTS)]);
    let before: Vec<_> = std::fs::read_dir(directory.path())
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name())
        .collect();

    let report = inspect(directory.path(), &BTreeSet::new(), &BTreeSet::new());
    assert!(!report.applied);
    assert!(report.rollback.is_none());

    let after: Vec<_> = std::fs::read_dir(directory.path())
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name())
        .collect();
    assert_eq!(before.len(), after.len(), "inspect wrote into the source");
}

/// What `apply` writes must load with the parser that will read it.
///
/// A migration that produces a file the daemon cannot parse has moved the
/// failure rather than fixed it, and moved it to a place nobody looks: the
/// next start-up, of a config the person believes is already working.
#[test]
fn what_is_written_parses_as_an_agent_registry() {
    let directory = source(&[(
        "agents.toml",
        "[[agents]]\nname = \"has \\\"quotes\\\"\"\ncommand = \"sh\"\nargs = [\"-c\", \"echo hi\"]\n",
    )]);
    let destination = tempfile::tempdir().unwrap();
    let report = inspect(directory.path(), &BTreeSet::new(), &BTreeSet::new());

    let written =
        anclaved::migration::apply(directory.path(), destination.path(), &report).unwrap();
    assert_eq!(written.len(), 1, "one agents file should be written");

    // The registry the daemon actually uses, not a stand-in parser.
    let registry = anclaved::agent::AgentRegistry::load(&written[0])
        .expect("the written file must load as an agent registry");
    assert!(
        registry.names().contains("has \"quotes\""),
        "names: {:?}",
        registry.names()
    );
}

/// Only what the report marked `Import` is written.
#[test]
fn apply_writes_only_what_was_reported() {
    let directory = source(&[("agents.toml", AGENTS)]);
    let destination = tempfile::tempdir().unwrap();
    // Pretend `claude` is already present, so only `shell` is importable.
    let existing = BTreeSet::from(["claude".to_owned()]);
    let report = inspect(directory.path(), &BTreeSet::new(), &existing);

    let written =
        anclaved::migration::apply(directory.path(), destination.path(), &report).unwrap();
    let registry = anclaved::agent::AgentRegistry::load(&written[0]).unwrap();
    assert!(registry.names().contains("shell"));
    assert!(
        !registry.names().contains("claude"),
        "an already-present agent must not be rewritten"
    );
}
