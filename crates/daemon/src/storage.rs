use std::path::Path;

use rusqlite::{params, Connection, OptionalExtension};
use anclave_protocol::{AgentId, SessionId, SessionState, SessionSummary};

const SCHEMA_VERSION: i64 = 1;
const NEXT_SESSION_ID_KEY: &str = "next_session_id";
const DEFAULT_AGENT: &str = "default";

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
            "SELECT id, name, state FROM sessions WHERE state != 'deleted' ORDER BY rowid",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(SessionSummary {
                id: SessionId::new(row.get::<_, String>(0)?).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })?,
                name: row.get(1)?,
                state: parse_state(&row.get::<_, String>(2)?)?,
                agent: AgentId::new(DEFAULT_AGENT).expect("static agent ID is valid"),
                workspace: None,
            })
        })?;
        rows.collect()
    }

    pub fn get_session(&self, id: &SessionId) -> rusqlite::Result<Option<SessionSummary>> {
        self.connection
            .query_row(
                "SELECT id, name, state FROM sessions WHERE id = ?1 AND state != 'deleted'",
                [id.as_str()],
                |row| {
                    Ok(SessionSummary {
                        id: id.clone(),
                        name: row.get(1)?,
                        state: parse_state(&row.get::<_, String>(2)?)?,
                        agent: AgentId::new(DEFAULT_AGENT).expect("static agent ID is valid"),
                        workspace: None,
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
            "INSERT INTO schema_meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![NEXT_SESSION_ID_KEY, (current + 1).to_string()],
        )?;
        SessionId::new(format!("session-{current}")).map_err(|_| {
            rusqlite::Error::InvalidParameterName("generated session ID is invalid".to_owned())
        })
    }

    pub fn insert_session(&self, session: &SessionSummary) -> rusqlite::Result<()> {
        self.connection.execute(
            "INSERT INTO sessions (id, name, state) VALUES (?1, ?2, ?3)",
            params![
                session.id.as_str(),
                session.name,
                state_name(&session.state)
            ],
        )?;
        Ok(())
    }

    pub fn update_session(&self, session: &SessionSummary) -> rusqlite::Result<bool> {
        let changed = self.connection.execute(
            "UPDATE sessions SET name = ?1, state = ?2 WHERE id = ?3 AND state != 'deleted'",
            params![
                session.name,
                state_name(&session.state),
                session.id.as_str()
            ],
        )?;
        Ok(changed == 1)
    }

    pub fn remove_session(&self, id: &SessionId) -> rusqlite::Result<bool> {
        let changed = self
            .connection
            .execute("DELETE FROM sessions WHERE id = ?1", [id.as_str()])?;
        Ok(changed == 1)
    }

    pub fn set_session_state(&self, id: &SessionId, state: SessionState) -> rusqlite::Result<bool> {
        let changed = self.connection.execute(
            "UPDATE sessions SET state = ?1 WHERE id = ?2 AND state != 'deleted'",
            params![state_name(&state), id.as_str()],
        )?;
        Ok(changed == 1)
    }

    pub fn delete_session(&self, id: &SessionId) -> rusqlite::Result<bool> {
        let changed = self.connection.execute(
            "UPDATE sessions SET state = 'deleted' WHERE id = ?1 AND state != 'deleted'",
            [id.as_str()],
        )?;
        Ok(changed == 1)
    }

    fn migrate(&self) -> rusqlite::Result<()> {
        self.connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS sessions (
                 id TEXT PRIMARY KEY,
                 name TEXT NOT NULL UNIQUE,
                 state TEXT NOT NULL CHECK (state IN ('creating', 'starting', 'running', 'detached', 'unreachable', 'exited', 'deleted'))
             );",
        )?;
        self.connection.execute(
            "INSERT INTO schema_meta (key, value) VALUES ('schema_version', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [SCHEMA_VERSION.to_string()],
        )?;
        self.connection.execute(
            "INSERT INTO schema_meta (key, value) VALUES (?1, '0')
             ON CONFLICT(key) DO NOTHING",
            [NEXT_SESSION_ID_KEY],
        )?;
        Ok(())
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
    use anclave_protocol::SessionState;

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
        let storage = Storage::open_in_memory().unwrap();
        assert!(storage.list_sessions().unwrap().is_empty());
    }

    #[test]
    fn session_state_can_be_updated_without_resurrecting_deleted_rows() {
        let storage = Storage::open_in_memory().unwrap();
        let value = session("session-1", "demo");
        storage.insert_session(&value).unwrap();
        assert!(storage
            .set_session_state(&value.id, SessionState::Running)
            .unwrap());
        assert_eq!(
            storage.get_session(&value.id).unwrap().unwrap().state,
            SessionState::Running
        );
        storage.delete_session(&value.id).unwrap();
        assert!(!storage
            .set_session_state(&value.id, SessionState::Exited)
            .unwrap());
    }

    #[test]
    fn sessions_round_trip_and_deleted_rows_are_hidden() {
        let storage = Storage::open_in_memory().unwrap();
        let value = session("session-1", "demo");
        storage.insert_session(&value).unwrap();
        assert_eq!(storage.get_session(&value.id).unwrap(), Some(value.clone()));
        assert_eq!(storage.list_sessions().unwrap(), vec![value.clone()]);
        assert!(storage.delete_session(&value.id).unwrap());
        assert!(storage.get_session(&value.id).unwrap().is_none());
        assert!(storage.list_sessions().unwrap().is_empty());
        assert!(!storage.delete_session(&value.id).unwrap());
    }
}
