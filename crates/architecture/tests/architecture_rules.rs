//! Dependency direction, enforced rather than described.
//!
//! `ARCHITECTURE.md` states which crate may depend on which. That statement is
//! worth nothing on its own: the rules that matter are the ones a plausible
//! edit breaks silently:
//!
//! * the protocol is the one crate both sides of the socket compile, so it
//!   must not drag an implementation dependency (SQLite, tokio, vt100) into a
//!   client;
//! * the daemon must never depend on a client crate, because that is the arrow
//!   along which lifecycle logic leaks back out of the daemon;
//! * leaf crates must not reach sideways to each other.
//!
//! Read from the manifests rather than from source, so a dependency that is
//! declared but not yet used is still caught.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crate sits two levels below the workspace root")
        .to_path_buf()
}

/// Dependency names declared by one crate's manifest, across every
/// `*dependencies*` section.
fn dependencies_of(crate_dir: &str) -> BTreeSet<String> {
    let manifest = workspace_root()
        .join("crates")
        .join(crate_dir)
        .join("Cargo.toml");
    let text = std::fs::read_to_string(&manifest)
        .unwrap_or_else(|_| panic!("manifest is readable: {}", manifest.display()));

    let mut names = BTreeSet::new();
    let mut in_dependencies = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_dependencies = line.contains("dependencies");
            continue;
        }
        if !in_dependencies || line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((name, _)) = line.split_once('=') {
            names.insert(name.trim().trim_matches('"').to_owned());
        }
    }
    names
}

fn assert_forbidden(crate_dir: &str, forbidden: &[&str]) {
    let declared = dependencies_of(crate_dir);
    for name in forbidden {
        assert!(
            !declared.contains(*name),
            "{crate_dir} must not depend on {name}: see ARCHITECTURE.md"
        );
    }
}

#[test]
fn every_workspace_member_is_covered_by_a_rule() {
    // A new crate fails this test until its place in the dependency order is
    // declared below. That is the point: the allowlist cannot silently drift
    // behind the workspace.
    let known: BTreeSet<String> = [
        "protocol",
        "terminal",
        "workspace",
        "daemon",
        "cli",
        "tui",
        "security",
        "architecture",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();

    let mut found = BTreeSet::new();
    for entry in std::fs::read_dir(workspace_root().join("crates")).expect("crates/ is readable") {
        let entry = entry.expect("readable entry");
        if entry.path().join("Cargo.toml").exists() {
            found.insert(entry.file_name().to_string_lossy().into_owned());
        }
    }
    assert_eq!(
        found, known,
        "a crate was added or removed without updating the architecture rules"
    );
}

#[test]
fn the_protocol_carries_no_implementation_dependencies() {
    assert_forbidden(
        "protocol",
        &[
            "rusqlite",
            "tokio",
            "vt100",
            "ratatui",
            "crossterm",
            "anclaved",
            "anclave-cli",
            "anclave-terminal",
            "anclave-workspace",
            "anclave-security",
        ],
    );
}

#[test]
fn security_is_a_leaf_and_never_reaches_the_daemon() {
    // The policy layer describes what an agent may reach. It must not depend
    // on the thing that launches agents, or the decision and the action end up
    // in one place and the decision stops being auditable on its own.
    assert_forbidden(
        "security",
        &[
            "anclaved",
            "anclave-cli",
            "anclave",
            "rusqlite",
            "anclave-terminal",
        ],
    );
}

#[test]
fn the_daemon_never_depends_on_a_client() {
    assert_forbidden("daemon", &["anclave-cli", "anclave"]);
}

#[test]
fn leaf_crates_do_not_reach_sideways() {
    assert_forbidden("terminal", &["anclave-workspace", "anclaved", "rusqlite"]);
    assert_forbidden("workspace", &["anclave-terminal", "anclaved", "rusqlite"]);
}

#[test]
fn clients_speak_the_protocol_and_never_open_the_database() {
    // The architectural change the rewrite exists to make: no client links a
    // database driver, because a client that can open SQLite will eventually
    // read it instead of asking the daemon.
    assert_forbidden("cli", &["rusqlite", "anclaved"]);
    assert_forbidden("tui", &["rusqlite", "anclaved"]);
}

#[test]
fn architecture_documentation_accompanies_the_rules() {
    assert!(
        workspace_root().join("ARCHITECTURE.md").exists(),
        "these rules are the enforcement half of ARCHITECTURE.md"
    );
}
