//! What a plugin may draw.
//!
//! Two node kinds, deliberately. Every widget a pane needs composes from a
//! box holding text, and each kind added here is one the renderer, the
//! converter, the tests and every future plugin must carry forever. Growing
//! this set is a decision to make on purpose, not by reflex when a plugin
//! wants something.

/// A render tree returned by a plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    /// A run of text on one line.
    Text { content: String },
    /// A container, optionally framed, stacking its children vertically.
    Box {
        title: Option<String>,
        border: bool,
        children: Vec<Node>,
    },
}

impl Node {
    /// How many nodes this tree holds, itself included.
    ///
    /// The host bounds this: a plugin that returns a deeply nested tree
    /// should be refused rather than allowed to exhaust the renderer.
    pub fn count(&self) -> usize {
        match self {
            Node::Text { .. } => 1,
            Node::Box { children, .. } => 1 + children.iter().map(Node::count).sum::<usize>(),
        }
    }

    /// The deepest path through this tree.
    pub fn depth(&self) -> usize {
        match self {
            Node::Text { .. } => 1,
            Node::Box { children, .. } => 1 + children.iter().map(Node::depth).max().unwrap_or(0),
        }
    }
}
