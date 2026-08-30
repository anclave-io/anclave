use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use anclave_protocol::{AgentId, SessionId};
use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentDefinition {
    pub id: AgentId,
    pub command: String,
    pub args: Vec<String>,
    pub resume: ResumeStrategy,
    pub supports_fork: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeStrategy {
    ExactSessionId {
        args: Vec<String>,
    },
    Latest {
        args: Vec<String>,
    },
    SessionFile {
        create_args: Vec<String>,
        resume_args: Vec<String>,
    },
    FreshOnly,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchSpec {
    pub program: String,
    pub args: Vec<String>,
    /// The exact environment the agent gets, or `None` to inherit whatever
    /// the backend already has.
    ///
    /// `None` is the compatibility path and means ambient trust. `Some` is a
    /// complete set: the backend must give the process that and nothing
    /// else, or the policy that produced it is decoration.
    pub environment: Option<std::collections::BTreeMap<String, String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRegistry {
    agents: BTreeMap<String, AgentDefinition>,
}

#[derive(Debug, thiserror::Error)]
pub enum AgentConfigError {
    #[error("could not read agent configuration: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid agent configuration: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("agent command cannot be empty")]
    EmptyCommand,
    #[error("agent name cannot be empty")]
    EmptyName,
}

#[derive(Debug, Deserialize)]
struct AgentFile {
    #[serde(default)]
    agents: Vec<AgentEntry>,
}

#[derive(Debug, Deserialize)]
struct AgentEntry {
    name: String,
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    resume: ResumeFile,
    #[serde(default)]
    supports_fork: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(tag = "strategy", rename_all = "snake_case")]
enum ResumeFile {
    ExactSessionId {
        args: Vec<String>,
    },
    Latest {
        args: Vec<String>,
    },
    SessionFile {
        create_args: Vec<String>,
        resume_args: Vec<String>,
    },
    #[default]
    FreshOnly,
}

impl AgentRegistry {
    pub fn builtins() -> Self {
        let default = AgentDefinition::default();
        let mut agents = BTreeMap::new();
        agents.insert(default.id.as_str().to_owned(), default);
        Self { agents }
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Self, AgentConfigError> {
        let contents = fs::read_to_string(path)?;
        let file: AgentFile = toml::from_str(&contents)?;
        let mut registry = Self::builtins();
        for entry in file.agents {
            if entry.name.trim().is_empty() {
                return Err(AgentConfigError::EmptyName);
            }
            if entry.command.trim().is_empty() {
                return Err(AgentConfigError::EmptyCommand);
            }
            let id = AgentId::new(entry.name).map_err(|_| AgentConfigError::EmptyName)?;
            let resume = match entry.resume {
                ResumeFile::ExactSessionId { args } => ResumeStrategy::ExactSessionId { args },
                ResumeFile::Latest { args } => ResumeStrategy::Latest { args },
                ResumeFile::SessionFile {
                    create_args,
                    resume_args,
                } => ResumeStrategy::SessionFile {
                    create_args,
                    resume_args,
                },
                ResumeFile::FreshOnly => ResumeStrategy::FreshOnly,
            };
            registry.agents.insert(
                id.as_str().to_owned(),
                AgentDefinition {
                    id,
                    command: entry.command,
                    args: entry.args,
                    resume,
                    supports_fork: entry.supports_fork,
                },
            );
        }
        Ok(registry)
    }

    pub fn get(&self, id: &AgentId) -> Option<&AgentDefinition> {
        self.agents.get(id.as_str())
    }

    pub fn default_agent(&self) -> &AgentDefinition {
        self.agents
            .get("default")
            .expect("registry always contains the default agent")
    }
}

impl Default for AgentDefinition {
    fn default() -> Self {
        Self {
            id: AgentId::new("default").expect("static agent ID is valid"),
            command: "sh".to_owned(),
            args: Vec::new(),
            resume: ResumeStrategy::FreshOnly,
            supports_fork: false,
        }
    }
}

impl AgentDefinition {
    pub fn launch(&self, session_id: &SessionId) -> LaunchSpec {
        LaunchSpec {
            program: self.command.clone(),
            args: substitute(&self.args, session_id),
            environment: None,
        }
    }

    pub fn resume(&self, session_id: &SessionId) -> Option<LaunchSpec> {
        let args = match &self.resume {
            ResumeStrategy::ExactSessionId { args }
            | ResumeStrategy::Latest { args }
            | ResumeStrategy::SessionFile {
                resume_args: args, ..
            } => args,
            ResumeStrategy::FreshOnly => return None,
        };
        Some(LaunchSpec {
            program: self.command.clone(),
            args: substitute(args, session_id),
            environment: None,
        })
    }

    pub fn fork(&self, session_id: &SessionId) -> Option<LaunchSpec> {
        if self.supports_fork {
            self.resume(session_id)
        } else {
            None
        }
    }
}

fn substitute(args: &[String], session_id: &SessionId) -> Vec<String> {
    args.iter()
        .map(|arg| arg.replace("{id}", session_id.as_str()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_include_default_agent() {
        let registry = AgentRegistry::builtins();
        assert_eq!(registry.default_agent().command, "sh");
    }

    #[test]
    fn loads_custom_agents() {
        let path = std::env::temp_dir().join(format!("anclave-agents-{}.toml", std::process::id()));
        fs::write(
            &path,
            "[[agents]]\nname = 'mock'\ncommand = 'mock-agent'\nargs = ['--id', '{id}']\n",
        )
        .unwrap();
        let registry = AgentRegistry::load(&path).unwrap();
        assert_eq!(
            registry
                .get(&AgentId::new("mock").unwrap())
                .unwrap()
                .launch(&SessionId::new("s1").unwrap())
                .args,
            vec!["--id", "s1"]
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn rejects_empty_commands() {
        let path = std::env::temp_dir().join(format!(
            "anclave-agents-invalid-{}.toml",
            std::process::id()
        ));
        fs::write(&path, "[[agents]]\nname = 'mock'\ncommand = ''\n").unwrap();
        assert!(matches!(
            AgentRegistry::load(&path),
            Err(AgentConfigError::EmptyCommand)
        ));
        let _ = fs::remove_file(path);
    }
}
