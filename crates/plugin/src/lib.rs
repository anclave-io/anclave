//! A versioned Lua plugin API for the terminal client.
//!
//! **This is UI plugin security, not agent security.** The two are separate
//! and share no controls. A plugin is an optional *client* of the daemon: it
//! reads a snapshot the client already has and returns a tree to draw. It
//! cannot spawn a process, open a file, reach the network, or talk to the
//! daemon. An agent, by contrast, runs real code under a security profile
//! (see `anclave-security`), and nothing here constrains one.
//!
//! Three properties hold the boundary:
//!
//! - **Capabilities by absence.** `io`, `os`, `debug`, `package` and the
//!   loaders are not in the environment. An absent capability cannot be
//!   reached by a bug in a check that was never written.
//! - **Bounded execution.** A plugin runs under an instruction budget, so a
//!   runaway loop is an error rather than a hung client.
//! - **A declared API version.** A plugin states the version it was written
//!   against and is refused if the host does not implement it, rather than
//!   half-working against a shape that has changed.

pub mod node;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub use node::Node;

/// The plugin API version this host implements.
pub const API_VERSION: u32 = 1;

/// How many Lua VM instructions one plugin call may execute.
///
/// Enough for a pane over a few hundred sessions, far short of a loop that
/// never ends. A plugin that exceeds it fails that call and is disabled, in
/// preference to a client that stops responding.
pub const INSTRUCTION_BUDGET: u32 = 200_000;

/// Bounds on a returned tree, so a plugin cannot exhaust the renderer.
pub const MAX_NODES: usize = 10_000;
pub const MAX_DEPTH: usize = 64;

#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    #[error("{path}: {message}")]
    Load { path: PathBuf, message: String },
    #[error("{id}: plugin declares api_version {declared}, host implements {API_VERSION}")]
    ApiVersion { id: String, declared: u32 },
    #[error("{id}: {message}")]
    Runtime { id: String, message: String },
    #[error("{id}: exceeded its instruction budget of {INSTRUCTION_BUDGET}")]
    Budget { id: String },
    #[error("{id}: returned a malformed tree: {message}")]
    Tree { id: String, message: String },
}

impl PluginError {
    /// The plugin this error is about, when it has one.
    pub fn plugin_id(&self) -> Option<&str> {
        match self {
            PluginError::Load { .. } => None,
            PluginError::ApiVersion { id, .. }
            | PluginError::Runtime { id, .. }
            | PluginError::Budget { id }
            | PluginError::Tree { id, .. } => Some(id),
        }
    }
}

/// What a plugin may read: a copy, not a handle.
///
/// Passing the client's own state would let a plugin hold a reference to it
/// across calls. This is built per render from what the client already has,
/// so a plugin sees the world and cannot change it.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub sessions: Vec<SessionView>,
    pub selected: Option<usize>,
    pub connected: bool,
}

#[derive(Debug, Clone)]
pub struct SessionView {
    pub id: String,
    pub name: String,
    pub state: String,
    pub agent: String,
}

/// A loaded, version-checked plugin.
pub struct Plugin {
    pub id: String,
    pub path: PathBuf,
    /// Set when a call failed. A failed plugin is not called again until the
    /// set is reloaded: a plugin that throws every frame would otherwise
    /// throw every frame forever, and the client would spend its time
    /// building error messages nobody reads.
    pub failure: Option<String>,
    table: mlua::RegistryKey,
}

/// The Lua host: one VM, many plugins.
pub struct PluginHost {
    lua: mlua::Lua,
    plugins: Vec<Plugin>,
    directory: Option<PathBuf>,
}

impl std::fmt::Debug for PluginHost {
    /// Deliberately shallow: the VM and the registry keys are not printable
    /// and a client that derives `Debug` should not be able to dump a
    /// plugin's internals into a log.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PluginHost")
            .field("plugins", &self.plugins.len())
            .field("directory", &self.directory)
            .finish()
    }
}

impl PluginHost {
    /// A host with no plugins, which is the normal state.
    ///
    /// The client must work with none, so this is not an error case.
    pub fn empty() -> Result<Self, PluginError> {
        Ok(Self {
            lua: sandboxed_vm()?,
            plugins: Vec::new(),
            directory: None,
        })
    }

    /// Load every `.lua` file in a directory, in name order.
    ///
    /// Returns the host plus the errors of the plugins that did not load. A
    /// bad plugin never prevents the good ones loading, and never prevents
    /// the client starting: that is the whole of "a broken extensible UI
    /// cannot brick the application".
    pub fn load_directory(directory: impl AsRef<Path>) -> (Self, Vec<PluginError>) {
        let directory = directory.as_ref().to_path_buf();
        let lua = match sandboxed_vm() {
            Ok(lua) => lua,
            Err(error) => {
                return (
                    Self {
                        lua: mlua::Lua::new(),
                        plugins: Vec::new(),
                        directory: Some(directory),
                    },
                    vec![error],
                )
            }
        };
        let mut host = Self {
            lua,
            plugins: Vec::new(),
            directory: Some(directory.clone()),
        };
        let errors = host.reload_from(&directory);
        (host, errors)
    }

    /// Discard every loaded plugin and load the directory again.
    pub fn reload(&mut self) -> Vec<PluginError> {
        let Some(directory) = self.directory.clone() else {
            return Vec::new();
        };
        self.reload_from(&directory)
    }

    fn reload_from(&mut self, directory: &Path) -> Vec<PluginError> {
        self.plugins.clear();
        let mut errors = Vec::new();
        let mut paths: Vec<PathBuf> = match std::fs::read_dir(directory) {
            Ok(entries) => entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.extension().is_some_and(|e| e == "lua"))
                .collect(),
            // A directory that is not there is not an error: it is the normal
            // case of a client with no plugins.
            Err(_) => return errors,
        };
        paths.sort();

        for path in paths {
            match self.load_one(&path) {
                Ok(plugin) => self.plugins.push(plugin),
                Err(error) => errors.push(error),
            }
        }
        errors
    }

    fn load_one(&mut self, path: &Path) -> Result<Plugin, PluginError> {
        let source = std::fs::read_to_string(path).map_err(|error| PluginError::Load {
            path: path.to_path_buf(),
            message: error.to_string(),
        })?;

        let value: mlua::Value = self
            .lua
            .load(&source)
            .set_name(path.to_string_lossy().as_ref())
            .eval()
            .map_err(|error| PluginError::Load {
                path: path.to_path_buf(),
                message: error.to_string(),
            })?;

        let table = match value {
            mlua::Value::Table(table) => table,
            _ => {
                return Err(PluginError::Load {
                    path: path.to_path_buf(),
                    message: "a plugin must return a table".to_owned(),
                })
            }
        };

        let id: String = table.get("id").map_err(|_| PluginError::Load {
            path: path.to_path_buf(),
            message: "a plugin must declare a string id".to_owned(),
        })?;

        // The version is checked before anything else is trusted about the
        // table: every other field's meaning is defined by it.
        let declared: u32 = table.get("api_version").map_err(|_| PluginError::Load {
            path: path.to_path_buf(),
            message: "a plugin must declare a numeric api_version".to_owned(),
        })?;
        if declared != API_VERSION {
            return Err(PluginError::ApiVersion { id, declared });
        }

        if !matches!(
            table.get::<mlua::Value>("render"),
            Ok(mlua::Value::Function(_))
        ) {
            return Err(PluginError::Load {
                path: path.to_path_buf(),
                message: format!("plugin '{id}' must define a render function"),
            });
        }

        let key = self
            .lua
            .create_registry_value(table)
            .map_err(|error| PluginError::Load {
                path: path.to_path_buf(),
                message: error.to_string(),
            })?;

        Ok(Plugin {
            id,
            path: path.to_path_buf(),
            failure: None,
            table: key,
        })
    }

    pub fn plugins(&self) -> &[Plugin] {
        &self.plugins
    }

    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }

    /// Render one plugin, by index.
    ///
    /// A plugin that fails is recorded as failed and skipped from then on.
    pub fn render(&mut self, index: usize, snapshot: &Snapshot) -> Result<Node, PluginError> {
        let Some(plugin) = self.plugins.get(index) else {
            return Err(PluginError::Runtime {
                id: format!("#{index}"),
                message: "no such plugin".to_owned(),
            });
        };
        if let Some(ref failure) = plugin.failure {
            return Err(PluginError::Runtime {
                id: plugin.id.clone(),
                message: failure.clone(),
            });
        }
        let id = plugin.id.clone();

        let result = self.call_render(index, snapshot);
        if let Err(ref error) = result {
            if let Some(plugin) = self.plugins.get_mut(index) {
                plugin.failure = Some(error.to_string());
            }
            debug_assert_eq!(error.plugin_id(), Some(id.as_str()));
        }
        result
    }

    fn call_render(&self, index: usize, snapshot: &Snapshot) -> Result<Node, PluginError> {
        let plugin = &self.plugins[index];
        let id = plugin.id.clone();
        let table: mlua::Table =
            self.lua
                .registry_value(&plugin.table)
                .map_err(|error| PluginError::Runtime {
                    id: id.clone(),
                    message: error.to_string(),
                })?;
        let render: mlua::Function = table.get("render").map_err(|error| PluginError::Runtime {
            id: id.clone(),
            message: error.to_string(),
        })?;
        let context = self
            .build_context(snapshot)
            .map_err(|error| PluginError::Runtime {
                id: id.clone(),
                message: error.to_string(),
            })?;

        // The budget is armed immediately before the call and disarmed after,
        // so loading a plugin and building its context are not charged to it.
        let exhausted = std::rc::Rc::new(std::cell::Cell::new(false));
        let flag = exhausted.clone();
        self.lua.set_hook(
            mlua::HookTriggers::new().every_nth_instruction(INSTRUCTION_BUDGET),
            move |_lua, _debug| {
                flag.set(true);
                Err(mlua::Error::runtime("instruction budget exhausted"))
            },
        );
        let returned: mlua::Result<mlua::Value> = render.call(context);
        self.lua.remove_hook();

        let value = match returned {
            Ok(value) => value,
            Err(error) => {
                if exhausted.get() {
                    return Err(PluginError::Budget { id });
                }
                return Err(PluginError::Runtime {
                    id,
                    message: error.to_string(),
                });
            }
        };

        convert(&value, 1).map_err(|message| PluginError::Tree { id, message })
    }

    fn build_context(&self, snapshot: &Snapshot) -> mlua::Result<mlua::Table> {
        let context = self.lua.create_table()?;
        let sessions = self.lua.create_table()?;
        for (index, session) in snapshot.sessions.iter().enumerate() {
            let row = self.lua.create_table()?;
            row.set("id", session.id.clone())?;
            row.set("name", session.name.clone())?;
            row.set("state", session.state.clone())?;
            row.set("agent", session.agent.clone())?;
            // Lua is 1-based, and a plugin comparing against `selected`
            // should not have to know the host is not.
            sessions.set(index + 1, row)?;
        }
        context.set("sessions", sessions)?;
        context.set("selected", snapshot.selected.map(|index| index + 1))?;
        context.set("connected", snapshot.connected)?;
        Ok(context)
    }
}

/// Convert a Lua value into a render tree, refusing anything malformed.
///
/// Depth is carried rather than recovered from the tree, because a cyclic
/// table would otherwise recurse until the stack ran out: a plugin returning
/// `local t = {} t.children = {t}` is a malformed tree, not a crash.
fn convert(value: &mlua::Value, depth: usize) -> Result<Node, String> {
    if depth > MAX_DEPTH {
        return Err(format!("tree deeper than {MAX_DEPTH}"));
    }
    let table = match value {
        mlua::Value::Table(table) => table,
        other => {
            return Err(format!(
                "expected a node table, found {}",
                other.type_name()
            ))
        }
    };

    let kind: String = table
        .get::<Option<String>>("kind")
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "a node needs a string 'kind'".to_owned())?;

    match kind.as_str() {
        "text" => {
            let content: String = table
                .get::<Option<String>>("content")
                .map_err(|error| error.to_string())?
                .unwrap_or_default();
            Ok(Node::Text { content })
        }
        "box" => {
            let title: Option<String> = table.get("title").map_err(|error| error.to_string())?;
            let border: Option<bool> = table.get("border").map_err(|error| error.to_string())?;
            let mut children = Vec::new();
            if let Ok(Some(list)) = table.get::<Option<mlua::Table>>("children") {
                for child in list.sequence_values::<mlua::Value>() {
                    let child = child.map_err(|error| error.to_string())?;
                    children.push(convert(&child, depth + 1)?);
                    if children.iter().map(Node::count).sum::<usize>() > MAX_NODES {
                        return Err(format!("tree larger than {MAX_NODES} nodes"));
                    }
                }
            }
            Ok(Node::Box {
                title,
                border: border.unwrap_or(false),
                children,
            })
        }
        other => Err(format!("unknown node kind '{other}'")),
    }
}

/// A VM with the dangerous halves of the standard library absent.
///
/// Absent, not overwritten: `os.execute = nil` leaves `os` reachable through
/// anything that captured it earlier, and leaves the next added function
/// exposed by default. Loading only the safe libraries means a capability
/// that is not granted was never in the VM to begin with.
fn sandboxed_vm() -> Result<mlua::Lua, PluginError> {
    let lua = mlua::Lua::new_with(
        mlua::StdLib::STRING | mlua::StdLib::TABLE | mlua::StdLib::MATH,
        mlua::LuaOptions::default(),
    )
    .map_err(|error| PluginError::Load {
        path: PathBuf::new(),
        message: error.to_string(),
    })?;

    // `new_with` still leaves the base library's loaders in globals. Remove
    // the ones that reach outside the VM: `load` and friends would let a
    // plugin build a chunk with a different environment, and `require` would
    // let it pull in a library this function deliberately did not load.
    let globals = lua.globals();
    for name in [
        "load",
        "loadstring",
        "loadfile",
        "dofile",
        "require",
        "collectgarbage",
        "print",
    ] {
        let _ = globals.set(name, mlua::Value::Nil);
    }
    Ok(lua)
}

/// The names a plugin must not be able to reach, checked by test.
pub const WITHHELD_GLOBALS: &[&str] = &[
    "io",
    "os",
    "debug",
    "package",
    "require",
    "load",
    "loadstring",
    "loadfile",
    "dofile",
    "print",
];

/// Names a plugin legitimately uses.
pub const GRANTED_GLOBALS: &[&str] = &["string", "table", "math", "pairs", "ipairs", "type"];

/// A map of plugin id to its failure, for a diagnostics view.
pub fn failures(host: &PluginHost) -> BTreeMap<String, String> {
    host.plugins()
        .iter()
        .filter_map(|plugin| {
            plugin
                .failure
                .as_ref()
                .map(|failure| (plugin.id.clone(), failure.clone()))
        })
        .collect()
}
