use anclave_protocol::{AgentId, SessionId};

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
        }
    }

    pub fn resume(&self, session_id: &SessionId) -> Option<LaunchSpec> {
        let args = match &self.resume {
            ResumeStrategy::ExactSessionId { args } => args,
            ResumeStrategy::Latest { args } => args,
            ResumeStrategy::SessionFile { resume_args, .. } => resume_args,
            ResumeStrategy::FreshOnly => return None,
        };
        Some(LaunchSpec {
            program: self.command.clone(),
            args: substitute(args, session_id),
        })
    }

    pub fn fork(&self, session_id: &SessionId) -> Option<LaunchSpec> {
        if !self.supports_fork {
            return None;
        }
        self.resume(session_id)
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

    fn agent() -> AgentDefinition {
        AgentDefinition {
            id: AgentId::new("mock").unwrap(),
            command: "mock-agent".to_owned(),
            args: vec!["--config".to_owned(), "{id}.json".to_owned()],
            resume: ResumeStrategy::ExactSessionId {
                args: vec!["resume".to_owned(), "{id}".to_owned()],
            },
            supports_fork: true,
        }
    }

    #[test]
    fn launch_substitutes_the_stable_session_id() {
        let spec = agent().launch(&SessionId::new("session-7").unwrap());
        assert_eq!(spec.program, "mock-agent");
        assert_eq!(spec.args, vec!["--config", "session-7.json"]);
    }

    #[test]
    fn resume_and_fork_are_explicit_and_substituted() {
        let agent = agent();
        let id = SessionId::new("session-7").unwrap();
        assert_eq!(agent.resume(&id).unwrap().args, vec!["resume", "session-7"]);
        assert_eq!(agent.fork(&id).unwrap().args, vec!["resume", "session-7"]);
    }

    #[test]
    fn fresh_only_and_non_forking_agents_decline_operations() {
        let mut agent = agent();
        agent.resume = ResumeStrategy::FreshOnly;
        agent.supports_fork = false;
        let id = SessionId::new("session-7").unwrap();
        assert!(agent.resume(&id).is_none());
        assert!(agent.fork(&id).is_none());
    }
}
