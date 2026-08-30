//! The plugin sandbox, probed capability by capability.
//!
//! These are the tests the security claim rests on. A claim about what a
//! plugin cannot reach is worth exactly as much as the probe that tries to
//! reach it, so each withheld capability is attempted here rather than
//! asserted about.

use anclave_plugin::{PluginError, PluginHost, Snapshot, WITHHELD_GLOBALS};

fn write(directory: &std::path::Path, name: &str, source: &str) {
    std::fs::write(directory.join(name), source).unwrap();
}

fn host_with(source: &str) -> (PluginHost, Vec<PluginError>, tempfile::TempDir) {
    let directory = tempfile::tempdir().unwrap();
    write(directory.path(), "a.lua", source);
    let (host, errors) = PluginHost::load_directory(directory.path());
    (host, errors, directory)
}

/// Every withheld global must be absent, not merely unusable.
///
/// Probed one at a time so a failure names the capability that leaked. A
/// blanket "the sandbox works" assertion tells you nothing about which of ten
/// names got through.
#[test]
fn withheld_globals_are_absent() {
    for name in WITHHELD_GLOBALS {
        let source = format!(
            "return {{ api_version = 1, id = 'probe', render = function()
                 if {name} ~= nil then error('{name} is reachable') end
                 return {{ kind = 'text', content = 'ok' }}
             end }}"
        );
        let (mut host, errors, _dir) = host_with(&source);
        assert!(
            errors.is_empty(),
            "{name}: plugin failed to load: {errors:?}"
        );
        let rendered = host.render(0, &Snapshot::default());
        assert!(
            rendered.is_ok(),
            "{name} is reachable from a plugin: {rendered:?}"
        );
    }
}

/// The granted names are present, or plugins cannot be written at all.
#[test]
fn granted_globals_are_present() {
    let source = "return { api_version = 1, id = 'probe', render = function()
        local parts = {}
        table.insert(parts, string.upper('a'))
        table.insert(parts, tostring(math.floor(1.5)))
        for _, v in ipairs({1, 2}) do table.insert(parts, tostring(v)) end
        return { kind = 'text', content = table.concat(parts) }
    end }";
    let (mut host, errors, _dir) = host_with(source);
    assert!(errors.is_empty(), "{errors:?}");
    assert!(host.render(0, &Snapshot::default()).is_ok());
}

/// A plugin written against another API version is refused, not adapted.
#[test]
fn a_mismatched_api_version_is_refused() {
    let (_host, errors, _dir) =
        host_with("return { api_version = 99, id = 'future', render = function() return {} end }");
    assert!(
        matches!(
            errors.as_slice(),
            [PluginError::ApiVersion { declared: 99, .. }]
        ),
        "{errors:?}"
    );
}

/// A runaway plugin is stopped, rather than hanging the client.
#[test]
fn an_endless_loop_is_stopped_by_the_budget() {
    let (mut host, errors, _dir) = host_with(
        "return { api_version = 1, id = 'spin', render = function()
             while true do end
         end }",
    );
    assert!(errors.is_empty(), "{errors:?}");
    assert!(
        matches!(
            host.render(0, &Snapshot::default()),
            Err(PluginError::Budget { .. })
        ),
        "an endless loop must be stopped by the instruction budget"
    );
}

/// A plugin that fails is not called again until a reload.
#[test]
fn a_failed_plugin_is_not_called_again() {
    let (mut host, _errors, _dir) =
        host_with("return { api_version = 1, id = 'boom', render = function() error('no') end }");
    assert!(host.render(0, &Snapshot::default()).is_err());
    assert!(host.plugins()[0].failure.is_some());
    assert!(host.render(0, &Snapshot::default()).is_err());
    assert_eq!(anclave_plugin::failures(&host).len(), 1);
}

/// Malformed trees are refused with a reason, in each of their shapes.
#[test]
fn malformed_trees_are_refused() {
    for (name, body, expected) in [
        ("not-a-table", "return 42", "expected a node table"),
        (
            "no-kind",
            "return { content = 'x' }",
            "a node needs a string 'kind'",
        ),
        (
            "unknown-kind",
            "return { kind = 'canvas' }",
            "unknown node kind",
        ),
    ] {
        let source =
            format!("return {{ api_version = 1, id = '{name}', render = function() {body} end }}");
        let (mut host, errors, _dir) = host_with(&source);
        assert!(errors.is_empty(), "{name}: {errors:?}");
        match host.render(0, &Snapshot::default()) {
            Err(PluginError::Tree { message, .. }) => assert!(
                message.contains(expected),
                "{name}: expected {expected:?}, got {message:?}"
            ),
            other => panic!("{name}: expected a tree error, got {other:?}"),
        }
    }
}

/// A cyclic tree is a malformed tree, not a stack overflow.
#[test]
fn a_cyclic_tree_is_refused_rather_than_crashing() {
    let (mut host, errors, _dir) = host_with(
        "return { api_version = 1, id = 'cycle', render = function()
             local node = { kind = 'box', children = {} }
             table.insert(node.children, node)
             return node
         end }",
    );
    assert!(errors.is_empty(), "{errors:?}");
    match host.render(0, &Snapshot::default()) {
        Err(PluginError::Tree { message, .. }) => {
            assert!(message.contains("deeper than"), "{message}")
        }
        other => panic!("expected a depth refusal, got {other:?}"),
    }
}

/// One bad plugin must not stop the good ones loading.
#[test]
fn a_broken_plugin_does_not_prevent_the_others_loading() {
    let directory = tempfile::tempdir().unwrap();
    write(directory.path(), "10_good.lua", "return { api_version = 1, id = 'good', render = function() return { kind = 'text', content = 'hi' } end }");
    write(directory.path(), "20_bad.lua", "this is not lua");
    write(directory.path(), "30_also_good.lua", "return { api_version = 1, id = 'also', render = function() return { kind = 'text', content = 'yo' } end }");

    let (mut host, errors) = PluginHost::load_directory(directory.path());
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(host.plugins().len(), 2);
    assert!(host.render(0, &Snapshot::default()).is_ok());
    assert!(host.render(1, &Snapshot::default()).is_ok());
}

/// A directory that is not there is the normal case, not an error.
#[test]
fn a_missing_directory_is_not_an_error() {
    let (host, errors) = PluginHost::load_directory("/nonexistent/anclave/plugins");
    assert!(errors.is_empty(), "{errors:?}");
    assert!(host.is_empty());
}

/// Reload picks up an edit, and clears a previous failure.
#[test]
fn reload_replaces_what_was_loaded() {
    let directory = tempfile::tempdir().unwrap();
    write(
        directory.path(),
        "a.lua",
        "return { api_version = 1, id = 'a', render = function() error('broken') end }",
    );
    let (mut host, errors) = PluginHost::load_directory(directory.path());
    assert!(errors.is_empty(), "{errors:?}");
    assert!(host.render(0, &Snapshot::default()).is_err());
    assert!(host.plugins()[0].failure.is_some());

    write(
        directory.path(),
        "a.lua",
        "return { api_version = 1, id = 'a', render = function() return { kind = 'text', content = 'fixed' } end }",
    );
    let errors = host.reload();
    assert!(errors.is_empty(), "{errors:?}");
    assert!(
        host.plugins()[0].failure.is_none(),
        "reload must clear a failure"
    );
    assert!(host.render(0, &Snapshot::default()).is_ok());
}

/// A plugin sees the sessions the client sees, and cannot change them.
#[test]
fn a_plugin_reads_the_snapshot() {
    let (mut host, errors, _dir) = host_with(
        "return { api_version = 1, id = 'list', render = function(ctx)
             local names = {}
             for _, s in ipairs(ctx.sessions) do table.insert(names, s.name) end
             return { kind = 'box', title = 'Sessions', border = true, children = {
                 { kind = 'text', content = table.concat(names, ',') },
                 { kind = 'text', content = tostring(ctx.selected) },
             } }
         end }",
    );
    assert!(errors.is_empty(), "{errors:?}");
    let snapshot = Snapshot {
        sessions: vec![
            anclave_plugin::SessionView {
                id: "session-0".to_owned(),
                name: "one".to_owned(),
                state: "Running".to_owned(),
                agent: "default".to_owned(),
            },
            anclave_plugin::SessionView {
                id: "session-1".to_owned(),
                name: "two".to_owned(),
                state: "Exited".to_owned(),
                agent: "default".to_owned(),
            },
        ],
        selected: Some(1),
        connected: true,
    };
    let node = host.render(0, &snapshot).expect("render");
    match node {
        anclave_plugin::Node::Box {
            title,
            border,
            children,
        } => {
            assert_eq!(title.as_deref(), Some("Sessions"));
            assert!(border);
            assert_eq!(
                children[0],
                anclave_plugin::Node::Text {
                    content: "one,two".to_owned()
                }
            );
            // Lua is 1-based: a plugin comparing against `selected` should
            // not have to know the host counts from zero.
            assert_eq!(
                children[1],
                anclave_plugin::Node::Text {
                    content: "2".to_owned()
                }
            );
        }
        other => panic!("expected a box, got {other:?}"),
    }
}
