//! Trust for UI plugins, keyed by path *and* content.
//!
//! **This governs UI plugins only.** It is not agent security and shares no
//! controls with it: a coding agent's authority comes from its security
//! profile (`anclave-security`), and nothing a person does here widens or
//! narrows what an agent can do. The two are documented apart because
//! conflating them is how a UI convenience becomes an agent capability.
//!
//! What trust decides is narrow by design: an untrusted plugin still loads,
//! still reads the snapshot, and still draws. Trust grants *capabilities*,
//! and a plugin that declares none needs no trust at all.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// What a plugin may do beyond reading a snapshot and returning a tree.
///
/// Every capability is a thing the client will do *on the plugin's behalf*,
/// because the plugin itself can do nothing: it has no file, process or
/// network access to widen. Adding one here is adding a way for a pane to
/// reach past drawing, so the list stays short and each entry is justified.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Capability {
    /// Ask the client to act on a session: focus it, restart it.
    ///
    /// Granting this lets a pane cause a daemon request the user did not
    /// type. It cannot invent a request the client does not already make.
    Commands,
}

impl Capability {
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "commands" => Some(Capability::Commands),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Capability::Commands => "commands",
        }
    }
}

/// Why a plugin's declared capabilities were not granted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrustState {
    /// No capabilities declared, so trust is not needed.
    NotRequired,
    /// Trusted, and the file is the one that was trusted.
    Trusted,
    /// Trusted at this path, but the content has changed since.
    ///
    /// Reported apart from untrusted on purpose: "this changed" is a
    /// different thing to tell someone than "you never approved this", and
    /// collapsing them is how a modified plugin passes as a new one.
    Modified,
    /// Never trusted at this path.
    Untrusted,
}

impl TrustState {
    /// Whether declared capabilities are granted in this state.
    pub fn grants(&self) -> bool {
        matches!(self, TrustState::Trusted | TrustState::NotRequired)
    }
}

/// The sha256 of a plugin's bytes, as lowercase hex.
pub fn digest(source: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(source.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Which plugins the user has trusted, and what they looked like then.
///
/// Keyed by absolute path with the digest recorded alongside, because either
/// alone is insufficient: a path alone would carry a grant across an edit
/// that replaced the file's contents, and a digest alone would let the same
/// bytes inherit a grant from anywhere on disk.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct TrustStore {
    entries: BTreeMap<String, String>,
}

impl TrustStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Read a store, treating an unreadable or corrupt one as empty.
    ///
    /// Failing closed: a store that cannot be parsed grants nothing, rather
    /// than the client refusing to start or, worse, assuming trust.
    pub fn load(path: impl AsRef<Path>) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: impl AsRef<Path>) -> std::io::Result<()> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(self)
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        std::fs::write(path, text)
    }

    /// Trust a plugin as it is right now.
    pub fn trust(&mut self, path: &Path, source: &str) {
        self.entries.insert(key(path), digest(source));
    }

    /// Withdraw trust. A revoked plugin keeps loading and drawing; it just
    /// stops being granted anything.
    pub fn revoke(&mut self, path: &Path) {
        self.entries.remove(&key(path));
    }

    /// What state this plugin is in, given what it declared and what it is.
    pub fn state_of(&self, path: &Path, source: &str, declares_capabilities: bool) -> TrustState {
        if !declares_capabilities {
            return TrustState::NotRequired;
        }
        match self.entries.get(&key(path)) {
            None => TrustState::Untrusted,
            Some(recorded) if recorded == &digest(source) => TrustState::Trusted,
            Some(_) => TrustState::Modified,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// The key a plugin is trusted under.
///
/// Absolutised so the same file is one entry however it was reached, but not
/// canonicalised: that resolves `/var` to `/private/var` on macOS and returns
/// extended-length paths on Windows, and this string is shown to people.
fn key(path: &Path) -> String {
    if path.is_absolute() {
        path.to_string_lossy().into_owned()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| PathBuf::from(path))
            .to_string_lossy()
            .into_owned()
    }
}
