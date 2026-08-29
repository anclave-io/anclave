use anclave_protocol::{
    AgentId, SecurityPosture, SessionId, SessionState, SessionSummary, WorkspaceSpec,
};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;

const SCHEMA_VERSION: i64 = 4;
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
            "SELECT id, name, state, agent, workspace_id, workspace_members, security_profile
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
                agent: AgentId::new(&row.get::<_, String>(3)?).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        3,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })?,
                workspace: workspace_from_row(row)?,
                security: posture_from_row(row)?,
            })
        })?;
        rows.collect()
    }

    pub fn get_session(&self, id: &SessionId) -> rusqlite::Result<Option<SessionSummary>> {
        self.connection
            .query_row(
                "SELECT id, name, state, agent, workspace_id, workspace_members, security_profile
                 FROM sessions WHERE id = ?1 AND state != 'deleted'",
                [id.as_str()],
                |row| {
                    Ok(SessionSummary {
                        id: id.clone(),
                        name: row.get(1)?,
                        state: parse_state(&row.get::<_, String>(2)?)?,
                        agent: AgentId::new(&row.get::<_, String>(3)?).map_err(|e| {
                            rusqlite::Error::FromSqlConversionFailure(
                                3,
                                rusqlite::types::Type::Text,
                                Box::new(e),
                            )
                        })?,
                        workspace: workspace_from_row(row)?,
                        security: posture_from_row(row)?,
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
        let (ws_id, members) = workspace_columns(&session.workspace);
        self.connection.execute(
            "INSERT INTO sessions (id, name, state, agent, workspace_id, workspace_members, security_profile)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                session.id.as_str(),
                session.name,
                state_name(&session.state),
                session.agent.as_str(),
                ws_id,
                members,
                session.security.profile,
            ],
        )?;
        Ok(())
    }

    pub fn update_session(&self, session: &SessionSummary) -> rusqlite::Result<bool> {
        let (ws_id, members) = workspace_columns(&session.workspace);
        Ok(self.connection.execute(
            "UPDATE sessions SET name=?1, state=?2, agent=?3, workspace_id=?4, workspace_members=?5, security_profile=?6
             WHERE id=?7 AND state != 'deleted'",
            params![
                session.name,
                state_name(&session.state),
                session.agent.as_str(),
                ws_id,
                members,
                session.security.profile,
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
        Ok(self.connection.execute(
            "UPDATE sessions SET state='deleted' WHERE id=?1 AND state != 'deleted'",
            [id.as_str()],
        )? == 1)
    }

    /// Fold the pre-v3 single-repository columns into the JSON member list.
    fn backfill_workspace_members(&self) -> rusqlite::Result<()> {
        let rows: Vec<(String, String, Option<String>)> = {
            let mut statement = self.connection.prepare(
                "SELECT id, workspace_repository, workspace_branch FROM sessions
                 WHERE workspace_id IS NOT NULL AND workspace_repository IS NOT NULL",
            )?;
            let mapped = statement.query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get::<_, Option<String>>(2)?))
            })?;
            mapped.collect::<rusqlite::Result<Vec<_>>>()?
        };

        for (id, repository, branch) in rows {
            let member = anclave_protocol::WorkspaceMember {
                repository,
                branch,
                base: None,
                access: anclave_protocol::MemberAccess::ReadWrite,
            };
            let encoded = serde_json::to_string(&vec![member]).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?;
            self.connection.execute(
                "UPDATE sessions SET workspace_members=?1 WHERE id=?2",
                params![encoded, id],
            )?;
        }
        Ok(())
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

        // v2 -> v3: the three per-member columns become one JSON list, so a
        // workspace can hold more than one repository. Existing rows are
        // folded into a single-member list rather than dropped; the old
        // columns are left in place because SQLite cannot drop a column on
        // every version we support, and an unread column costs nothing.
        // v3 -> v4: remember which profile a session was created under, so a
        // restart cannot silently upgrade an uncontained session's posture or
        // downgrade a contained one.
        if current_version < 4 {
            self.connection.execute_batch(
                "ALTER TABLE sessions ADD COLUMN security_profile TEXT NOT NULL DEFAULT 'default';",
            )?;
        }
        if current_version < 3 {
            self.connection
                .execute_batch("ALTER TABLE sessions ADD COLUMN workspace_members TEXT;")?;
            self.backfill_workspace_members()?;
        }

        self.connection.execute(
            "INSERT INTO schema_meta (key,value) VALUES ('schema_version',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [SCHEMA_VERSION.to_string()],
        )?;

        Ok(())
    }
}
/// A workspace is stored as its id plus a JSON array of members.
///
/// The members were four flat columns while a workspace held exactly one
/// repository. They cannot describe N of them, and widening the session table
/// per member is what the plan's "keep the central session table small" rules
/// out — so the list travels as one opaque value the session table does not
/// need to understand.
fn workspace_columns(workspace: &Option<WorkspaceSpec>) -> (Option<String>, Option<String>) {
    match workspace {
        Some(ws) => (
            Some(ws.id.as_str().to_owned()),
            serde_json::to_string(&ws.members).ok(),
        ),
        None => (None, None),
    }
}

fn workspace_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Option<WorkspaceSpec>> {
    let ws_id: Option<String> = row.get(4)?;
    let members: Option<String> = row.get(5)?;
    let (Some(id), Some(members)) = (ws_id, members) else {
        return Ok(None);
    };
    let members = serde_json::from_str(&members).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(5, rusqlite::types::Type::Text, Box::new(e))
    })?;
    Ok(Some(WorkspaceSpec {
        id: anclave_protocol::WorkspaceId::new(id).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Text, Box::new(e))
        })?,
        members,
    }))
}

/// Rebuild a client-visible posture from the stored profile name.
///
/// The name is stored rather than the resolved profile: a profile the
/// operator has since tightened should apply on the next launch, and a bad
/// one must be fixable without rewriting rows. What was *in force* for a
/// given action belongs in the audit log, not here.
fn posture_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SecurityPosture> {
    let profile: Option<String> = row.get(6)?;
    Ok(match profile {
        Some(profile) => SecurityPosture {
            profile,
            ..SecurityPosture::default()
        },
        None => SecurityPosture::default(),
    })
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
    use std::time::{SystemTime, UNIX_EPOCH};

    fn session(id: &str, name: &str) -> SessionSummary {
        SessionSummary {
            id: SessionId::new(id).unwrap(),
            name: name.to_owned(),
            state: SessionState::Creating,
            agent: AgentId::new("default").unwrap(),
            workspace: None,
            security: Default::default(),
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
            members: vec![
                anclave_protocol::WorkspaceMember {
                    repository: "/repo".to_owned(),
                    branch: Some("feat/test".to_owned()),
                    base: Some("main".to_owned()),
                    access: anclave_protocol::MemberAccess::ReadWrite,
                },
                anclave_protocol::WorkspaceMember {
                    repository: "/reference".to_owned(),
                    branch: None,
                    base: None,
                    access: anclave_protocol::MemberAccess::ReadOnly,
                },
            ],
        };
        let mut v = session("session-1", "demo");
        v.workspace = Some(ws.clone());
        s.insert_session(&v).unwrap();
        let retrieved = s.get_session(&v.id).unwrap().unwrap();
        // Every member survives the round trip, including the access
        // declaration a sandbox will later read.
        assert_eq!(retrieved.workspace.as_ref().unwrap(), &ws);
    }

    /// A database written before the member list existed must keep its
    /// workspace, folded into a one-member list rather than dropped.
    #[test]
    fn a_pre_v3_workspace_row_is_migrated_into_a_member_list() {
        use anclave_protocol::{MemberAccess, WorkspaceMember};

        let path = std::env::temp_dir().join(format!(
            "anclave-migrate-{}-{}.db",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&path);

        // Build a v2 database by hand: the old flat columns, no member list.
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE schema_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 CREATE TABLE sessions (
                     id TEXT PRIMARY KEY,
                     name TEXT NOT NULL UNIQUE,
                     state TEXT NOT NULL,
                     agent TEXT NOT NULL DEFAULT 'default',
                     workspace_id TEXT,
                     workspace_repository TEXT,
                     workspace_branch TEXT,
                     workspace_base TEXT
                 );
                 INSERT INTO schema_meta (key,value) VALUES ('schema_version','2');
                 INSERT INTO sessions VALUES
                     ('session-1','demo','running','claude','ws-1','/repo','feat/old',NULL);",
            )
            .unwrap();
        }

        let storage = Storage::open(&path).unwrap();
        let session = storage
            .get_session(&SessionId::new("session-1").unwrap())
            .unwrap()
            .unwrap();
        let workspace = session.workspace.expect("the workspace must survive");
        assert_eq!(
            workspace.members,
            vec![WorkspaceMember {
                repository: "/repo".to_owned(),
                branch: Some("feat/old".to_owned()),
                base: None,
                access: MemberAccess::ReadWrite,
            }]
        );
        let _ = std::fs::remove_file(path);
    }
}
