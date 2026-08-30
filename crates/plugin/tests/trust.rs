//! Plugin trust and capabilities, probed.
//!
//! **These controls govern UI plugins only.** They are not agent security and
//! share no mechanism with it. Trusting a plugin grants a pane the ability to
//! ask the client to do something the client already does; it does not widen
//! what any coding agent may do, which is decided by that agent's security
//! profile in `anclave-security`.

use anclave_plugin::{Capability, Command, PluginHost, Snapshot, TrustState, TrustStore};

const ASKS_FOR_COMMANDS: &str = "return {
    api_version = 1,
    id = 'asker',
    capabilities = { 'commands' },
    render = function(ctx)
        if ctx.command then
            ctx.command('focus', 'session-0')
        end
        return { kind = 'text', content = tostring(ctx.command ~= nil) }
    end,
}";

fn dir_with(files: &[(&str, &str)]) -> tempfile::TempDir {
    let directory = tempfile::tempdir().unwrap();
    for (name, source) in files {
        std::fs::write(directory.path().join(name), source).unwrap();
    }
    directory
}

/// An untrusted plugin loads and draws, but is granted nothing.
///
/// It must still work: trust gates capabilities, not existence. A model that
/// refused to load an untrusted plugin would teach people to trust every
/// plugin just to see what it does.
#[test]
fn an_untrusted_plugin_runs_without_its_capability() {
    let directory = dir_with(&[("a.lua", ASKS_FOR_COMMANDS)]);
    let (mut host, errors) = PluginHost::load_directory(directory.path());
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(host.plugins()[0].trust, TrustState::Untrusted);
    assert_eq!(host.plugins()[0].declared, vec![Capability::Commands]);
    assert!(host.plugins()[0].granted.is_empty());

    let tree = host.render(0, &Snapshot::default()).expect("still renders");
    assert_eq!(
        tree,
        anclave_plugin::Node::Text {
            content: "false".to_owned()
        },
        "an ungranted plugin must see no command function at all"
    );
    assert!(host.plugins()[0].commands.is_empty());
}

/// Trusting a plugin grants what it declared, from the next load.
#[test]
fn a_trusted_plugin_is_granted_what_it_declared() {
    let directory = dir_with(&[("a.lua", ASKS_FOR_COMMANDS)]);
    let (mut host, _errors) = PluginHost::load_directory(directory.path());
    host.trust_plugin(0).unwrap();
    host.reload();

    assert_eq!(host.plugins()[0].trust, TrustState::Trusted);
    assert_eq!(host.plugins()[0].granted, vec![Capability::Commands]);

    let tree = host.render(0, &Snapshot::default()).expect("renders");
    assert_eq!(
        tree,
        anclave_plugin::Node::Text {
            content: "true".to_owned()
        }
    );
    assert_eq!(
        host.plugins()[0].commands,
        vec![Command::Focus {
            session: "session-0".to_owned()
        }]
    );
}

/// Editing a trusted plugin withdraws the grant, and says why.
///
/// `Modified` is reported apart from `Untrusted`: "this changed since you
/// approved it" is a different thing to tell someone than "you never approved
/// this", and collapsing them is how an edited plugin passes as a new one.
#[test]
fn a_modified_trusted_plugin_loses_its_grant() {
    let directory = dir_with(&[("a.lua", ASKS_FOR_COMMANDS)]);
    let (mut host, _errors) = PluginHost::load_directory(directory.path());
    host.trust_plugin(0).unwrap();
    host.reload();
    assert_eq!(host.plugins()[0].trust, TrustState::Trusted);

    // The same path, different bytes.
    std::fs::write(
        directory.path().join("a.lua"),
        ASKS_FOR_COMMANDS.replace("'asker'", "'asker' --[[ edited ]]"),
    )
    .unwrap();
    host.reload();

    assert_eq!(host.plugins()[0].trust, TrustState::Modified);
    assert!(host.plugins()[0].granted.is_empty());
    let tree = host.render(0, &Snapshot::default()).expect("still renders");
    assert_eq!(
        tree,
        anclave_plugin::Node::Text {
            content: "false".to_owned()
        }
    );
}

/// Revoking trust takes the capability away again.
#[test]
fn revoked_trust_withdraws_the_capability() {
    let directory = dir_with(&[("a.lua", ASKS_FOR_COMMANDS)]);
    let (mut host, _errors) = PluginHost::load_directory(directory.path());
    host.trust_plugin(0).unwrap();
    host.reload();
    assert!(!host.plugins()[0].granted.is_empty());

    host.revoke_plugin(0);
    host.reload();
    assert_eq!(host.plugins()[0].trust, TrustState::Untrusted);
    assert!(host.plugins()[0].granted.is_empty());
}

/// A plugin declaring nothing needs no trust.
#[test]
fn a_plugin_with_no_capabilities_needs_no_trust() {
    let directory = dir_with(&[(
        "a.lua",
        "return { api_version = 1, id = 'plain', render = function()
             return { kind = 'text', content = 'hi' }
         end }",
    )]);
    let (host, errors) = PluginHost::load_directory(directory.path());
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(host.plugins()[0].trust, TrustState::NotRequired);
}

/// Asking for a capability the host does not have is refused at load.
///
/// Ignoring the unknown name would let a plugin written for a later host
/// believe it had been granted something this one has never heard of.
#[test]
fn an_unknown_capability_is_refused() {
    let directory = dir_with(&[(
        "a.lua",
        "return { api_version = 1, id = 'greedy', capabilities = { 'spawn' },
                  render = function() return { kind = 'text' } end }",
    )]);
    let (host, errors) = PluginHost::load_directory(directory.path());
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(
        errors[0].to_string().contains("unknown capability 'spawn'"),
        "{}",
        errors[0]
    );
    assert!(host.plugins().is_empty());
}

/// A granted plugin cannot invent an action the client does not perform.
#[test]
fn an_unknown_command_action_is_refused() {
    let directory = dir_with(&[(
        "a.lua",
        "return { api_version = 1, id = 'sneaky', capabilities = { 'commands' },
          render = function(ctx)
              local ok = ctx.command('delete_everything', 'session-0')
              return { kind = 'text', content = tostring(ok) }
          end }",
    )]);
    let (mut host, _errors) = PluginHost::load_directory(directory.path());
    host.trust_plugin(0).unwrap();
    host.reload();

    let tree = host.render(0, &Snapshot::default()).expect("renders");
    assert_eq!(
        tree,
        anclave_plugin::Node::Text {
            content: "false".to_owned()
        },
        "an unknown action must be refused visibly, not queued"
    );
    assert!(
        host.plugins()[0].commands.is_empty(),
        "no request may reach the client"
    );
}

/// One plugin cannot see or change another's globals.
///
/// A shared environment let a plugin's top-level assignment reach the next
/// one: a pane could overwrite a function another relied on, and two panes
/// could talk through a channel neither the client nor the user knew about.
#[test]
fn plugins_cannot_reach_each_other() {
    let directory = dir_with(&[
        (
            "10_writer.lua",
            "smuggled = 'from the writer'
             return { api_version = 1, id = 'writer', render = function()
                 smuggled = 'written during render'
                 return { kind = 'text', content = 'ok' }
             end }",
        ),
        (
            "20_reader.lua",
            "return { api_version = 1, id = 'reader', render = function()
                 return { kind = 'text', content = tostring(smuggled) }
             end }",
        ),
    ]);
    let (mut host, errors) = PluginHost::load_directory(directory.path());
    assert!(errors.is_empty(), "{errors:?}");

    host.render(0, &Snapshot::default())
        .expect("writer renders");
    let seen = host
        .render(1, &Snapshot::default())
        .expect("reader renders");
    assert_eq!(
        seen,
        anclave_plugin::Node::Text {
            content: "nil".to_owned()
        },
        "a plugin must not see another plugin's global"
    );
}

/// A grant follows the file, not the bytes, and not the path alone.
#[test]
fn trust_is_keyed_by_both_path_and_content() {
    let directory = tempfile::tempdir().unwrap();
    let first = directory.path().join("first.lua");
    let second = directory.path().join("second.lua");
    std::fs::write(&first, ASKS_FOR_COMMANDS).unwrap();
    std::fs::write(&second, ASKS_FOR_COMMANDS).unwrap();

    let mut store = TrustStore::new();
    store.trust(&first, ASKS_FOR_COMMANDS);

    // Identical bytes at another path inherit nothing.
    assert_eq!(
        store.state_of(&second, ASKS_FOR_COMMANDS, true),
        TrustState::Untrusted,
        "a digest alone must not carry a grant across paths"
    );
    // The trusted path with different bytes is modified, not trusted.
    assert_eq!(
        store.state_of(&first, "return {}", true),
        TrustState::Modified,
        "a path alone must not carry a grant across an edit"
    );
    assert_eq!(
        store.state_of(&first, ASKS_FOR_COMMANDS, true),
        TrustState::Trusted
    );
}

/// A store round-trips, and an unreadable one grants nothing.
#[test]
fn a_trust_store_persists_and_fails_closed() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("trust.json");
    let plugin = directory.path().join("a.lua");

    let mut store = TrustStore::new();
    store.trust(&plugin, ASKS_FOR_COMMANDS);
    store.save(&path).unwrap();

    let loaded = TrustStore::load(&path);
    assert_eq!(
        loaded.state_of(&plugin, ASKS_FOR_COMMANDS, true),
        TrustState::Trusted
    );

    // Corrupt it: the client must grant nothing rather than assume trust.
    std::fs::write(&path, "{ not json").unwrap();
    let broken = TrustStore::load(&path);
    assert!(broken.is_empty());
    assert_eq!(
        broken.state_of(&plugin, ASKS_FOR_COMMANDS, true),
        TrustState::Untrusted
    );
}
