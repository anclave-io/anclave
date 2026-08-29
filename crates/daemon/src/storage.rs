use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use anclave_protocol::{AgentId, SessionId, SessionState, SessionSummary, WorkspaceSpec};

const SCHEMA_VERSION: i64 = 2;
const NEXT_SESSION_ID_KEY: &str = "next_session_id";

#[derive(Debug)]
pub struct Storage {
    connection: Connection,
}

impl Storage {
    pub fn open(path: impl AsRef<Path>) -> rusqlite::Result<Self> {
        let connection = Connection::open(path)?;
        let storage = Self { connection };
        storage.migrate()?;
        Ok(storage)
    }

    pub fn open_in_memory() -> rusqlite::Result<Self> {
        let connection = Connection::open_in_memory()?;
        let storage = Self { connection };
        storage.migrate()?;
        Ok(storage)
    }

    pub fn list_sessions(&self) -> rusqlite::Result<Vec<SessionSummary>> {
        let mut statement = self.connection.prepare(
            "SELECT id, name, state, agent, workspace_id, workspace_repository, workspace_branch, workspace_base
             FROM sessions WHERE state != 'deleted' ORDER BY rowid",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(SessionSummary {
                id: SessionId::new(row.get::<_, String>(0)?).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })?,
                name: row.get(1)?,
                state: parse_state(&row.get::<_, String>(2)?)?,
                agent: AgentId::new(&row.get::<_, String>(3)?)
                    .map_err(|e| {
                        rusqlite::Error::FromSqlConversionFailure(
                            3,
                            rusqlite::types::Type::Text,
                            Box::new(e),
                        )
                    })?,
                workspace: workspace_from_row(row)?,
            })
        })?;
        rows.collect()
    }

    pub fn get_session(&self, id: &SessionId) -> rusqlite::Result<Option<SessionSummary>> {
        self.connection
            .query_row(
                "SELECT id, name, state, agent, workspace_id, workspace_repository, workspace_branch, workspace_base
                 FROM sessions WHERE id = ?1 AND state != 'deleted'",
                [id.as_str()],
                |row| {
                    Ok(SessionSummary {
                        id: id.clone(),
                        name: row.get(1)?,
                        state: parse_state(&row.get::<_, String>(2)?)?,
                        agent: AgentId::new(&row.get::<_, String>(3)?)
                            .map_err(|e| {
                                rusqlite::Error::FromSqlConversionFailure(
                                    3,
                                    rusqlite::types::Type::Text,
                                    Box::new(e),
                                )
                            })?,
                        workspace: workspace_from_row(row)?,
                    })
                },
            )
            .optional()
    }

    pub fn next_session_id(&self) -> rusqlite::Result<SessionId> {
        let current = self
            .connection
            .query_row(
                "SELECT CAST(value AS INTEGER) FROM schema_meta WHERE key = ?1",
                [NEXT_SESSION_ID_KEY],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .unwrap_or(0);
        if current < 0 {
            return Err(rusqlite::Error::InvalidParameterName(
                "next session ID must be nonnegative".to_owned(),
            ));
        }
        self.connection.execute(
            "INSERT INTO schema_meta (key,value) VALUES (?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![NEXT_SESSION_ID_KEY, (current + 1).to_string()],
        )?;
        SessionId::new(format!("session-{current}")).map_err(|_| {
            rusqlite::Error::InvalidParameterName("generated session ID is invalid".to_owned())
        })
    }

    pub fn insert_session(&self, session: &SessionSummary) -> rusqlite::Result<()> {
        let (ws_id, repo, branch, base) = workspace_columns(&session.workspace);
        self.connection.execute(
            "INSERT INTO sessions (id, name, state, agent, workspace_id, workspace_repository, workspace_branch, workspace_base)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                session.id.as_str(),
                session.name,
                state_name(&session.state),
                session.agent.as_str(),
                ws_id,
                repo,
                branch,
                base,
            ],
        )?;
        Ok(())
    }

    pub fn update_session(&self, session: &SessionSummary) -> rusqlite::Result<bool> {
        let (ws_id, repo, branch, base) = workspace_columns(&session.workspace);
        Ok(self.connection.execute(
            "UPDATE sessions SET name=?1, state=?2, agent=?3, workspace_id=?4, workspace_repository=?5, workspace_branch=?6, workspace_base=?7
             WHERE id=?8 AND state != 'deleted'",
            params![
                session.name,
                state_name(&session.state),
                session.agent.as_str(),
                ws_id,
                repo,
                branch,
                base,
                session.id.as_str(),
            ],
        )? == 1)
    }

    pub fn remove_session(&self, id: &SessionId) -> rusqlite::Result<bool> {
        Ok(self
            .connection
            .execute("DELETE FROM sessions WHERE id=?1", [id.as_str()])?
            == 1)
    }

    pub fn set_session_state(&self, id: &SessionId, state: SessionState) -> rusqlite::Result<bool> {
        Ok(self.connection.execute(
            "UPDATE sessions SET state=?1 WHERE id=?2 AND state != 'deleted'",
            params![state_name(&state), id.as_str()],
        )? == 1)
    }

    pub fn delete_session(&self, id: &SessionId) -> rusqlite::Result<bool> {
        Ok(self
            .connection
            .execute(
                "UPDATE sessions SET state='deleted' WHERE id=?1 AND state != 'deleted'",
                [id.as_str()],
            )?
            == 1)
    }

    fn migrate(&self) -> rusqlite::Result<()> {
        self.connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS sessions (
                 id TEXT PRIMARY KEY,
                 name TEXT NOT NULL UNIQUE,
                 state TEXT NOT NULL CHECK (state IN ('creating','starting','running','detached','unreachable','exited','deleted'))
             );",
        )?;
        self.connection.execute(
            "INSERT INTO schema_meta (key,value) VALUES (?1,'0') ON CONFLICT(key) DO NOTHING",
            [NEXT_SESSION_ID_KEY],
        )?;

        // Migration v1 -> v2: add agent and workspace columns
        let current_version: i64 = self
            .connection
            .query_row(
                "SELECT CAST(value AS INTEGER) FROM schema_meta WHERE key = 'schema_version'",
                [],
                |row| row.get(0),
            )
            .unwrap_or(1);
        if current_version < 2 {
            self.connection.execute_batch(
                "ALTER TABLE sessions ADD COLUMN agent TEXT NOT NULL DEFAULT 'default';
                 ALTER TABLE sessions ADD COLUMN workspace_id TEXT;
                 ALTER TABLE sessions ADD COLUMN workspace_repository TEXT;
                 ALTER TABLE sessions ADD COLUMN workspace_branch TEXT;
                 ALTER TABLE sessions ADD COLUMN workspace_base TEXT;",
            )?;
        }

        self.connection.execute(
            "INSERT INTO schema_meta (key,value) VALUES ('schema_version',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [SCHEMA_VERSION.to_string()],
        )?;

        Ok(())
    }
}    fn workspace_columns(
        workspace: &Option<WorkspaceSpec>,
    ) -> (Option<String>, Option<String>, Option<String>, Option<String>) {
        match workspace {
            Some(ws) => (
                Some(ws.id.as_str().to_owned()),
                Some(ws.repository.clone()),
                Some(ws.branch.clone()),
                ws.base.clone(),
            ),
            None => (None, None, None, None),
        }
    }

    fn workspace_from_row(
        row: &rusqlite::Row<'_>,
    ) -> rusqlite::Result<Option<WorkspaceSpec>> {
        let ws_id: Option<String> = row.get(4)?;
        let repo: Option<String> = row.get(5)?;
        let branch: Option<String> = row.get(6)?;
        let base: Option<String> = row.get(7)?;
        match (ws_id, repo, branch) {
            (Some(id), Some(repository), Some(branch)) => Ok(Some(WorkspaceSpec {
                id: anclave_protocol::WorkspaceId::new(id)
                    .map_err(|e| {
                        rusqlite::Error::FromSqlConversionFailure(
                            4,
                            rusqlite::types::Type::Text,
                            Box::new(e),
                        )
                    })?,
                repository,
                branch,
                base,
            })),
            _ => Ok(None),
        }
    }

fn state_name(state: &SessionState) -> &'static str {
    match state {
        SessionState::Creating => "creating",
        SessionState::Starting => "starting",
        SessionState::Running => "running",
        SessionState::Detached => "detached",
        SessionState::Unreachable => "unreachable",
        SessionState::Exited => "exited",
        SessionState::Deleted => "deleted",
    }
}

fn parse_state(value: &str) -> rusqlite::Result<SessionState> {
    match value {
        "creating" => Ok(SessionState::Creating),
        "starting" => Ok(SessionState::Starting),
        "running" => Ok(SessionState::Running),
        "detached" => Ok(SessionState::Detached),
        "unreachable" => Ok(SessionState::Unreachable),
        "exited" => Ok(SessionState::Exited),
        "deleted" => Ok(SessionState::Deleted),
        _ => Err(rusqlite::Error::InvalidParameterName(format!(
            "unknown session state: {value}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: &str, name: &str) -> SessionSummary {
        SessionSummary {
            id: SessionId::new(id).unwrap(),
            name: name.to_owned(),
            state: SessionState::Creating,
            agent: AgentId::new("default").unwrap(),
            workspace: None,
        }
    }

    #[test]
    fn memory_storage_migrates_and_starts_empty() {
        assert!(Storage::open_in_memory()
            .unwrap()
            .list_sessions()
            .unwrap()
            .is_empty())
    }

    #[test]
    fn session_state_can_be_updated_without_resurrecting_deleted_rows() {
        let s = Storage::open_in_memory().unwrap();
        let v = session("session-1", "demo");
        s.insert_session(&v).unwrap();
        assert!(s.set_session_state(&v.id, SessionState::Running).unwrap());
        s.delete_session(&v.id).unwrap();
        assert!(!s.set_session_state(&v.id, SessionState::Exited).unwrap())
    }

    #[test]
    fn sessions_round_trip_and_deleted_rows_are_hidden() {
        let s = Storage::open_in_memory().unwrap();
        let v = session("session-1", "demo");
        s.insert_session(&v).unwrap();
        assert_eq!(s.get_session(&v.id).unwrap(), Some(v.clone()));
        assert!(s.delete_session(&v.id).unwrap());
        assert!(s.get_session(&v.id).unwrap().is_none())
    }

    #[test]
    fn workspace_metadata_round_trips_through_storage() {
        use anclave_protocol::{WorkspaceId, WorkspaceSpec};

        let s = Storage::open_in_memory().unwrap();
        let ws = WorkspaceSpec {
            id: WorkspaceId::new("ws-1").unwrap(),
            repository: "/repo".to_owned(),
            branch: "feat/test".to_owned(),
            base: Some("main".to_owned()),
        };
        let mut v = session("session-1", "demo");
        v.workspace = Some(ws.clone());
        s.insert_session(&v).unwrap();
        let retrieved = s.get_session(&v.id).unwrap().unwrap();
        assert_eq!(retrieved.workspace.as_ref().unwrap().repository, "/repo");
        assert_eq!(retrieved.workspace.as_ref().unwrap().branch, "feat/test");
        assert_eq!(
            retrieved.workspace.as_ref().unwrap().base.as_deref(),
            Some("main")
        );
    }
}
