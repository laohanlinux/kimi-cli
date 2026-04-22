//! SQLite persistence layer for sessions, context, wire events, and tasks.
//!
//! Single `Store` per process; all operations are synchronised via a
//! mutex around the SQLite connection.

use rusqlite::{Connection, Result as SqlResult, params};
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;

#[derive(Clone)]
pub struct Store {
    pub(crate) conn: Arc<Mutex<Connection>>,
}

#[derive(Debug)]
#[allow(dead_code)]
pub struct ContextRow {
    pub id: i64,
    pub role: String,
    pub content: Option<String>,
    pub metadata: Option<String>,
    pub checkpoint_id: Option<i64>,
    pub token_count: Option<i64>,
}

/// Tuple returned by session list queries: (id, work_dir, created_at, title).
pub type SessionRow = (String, String, String, Option<String>);

/// Parameters for [`Store::create_subagent`].
pub struct CreateSubagentParams<'a> {
    pub id: &'a str,
    pub session_id: &'a str,
    pub parent_tool_call_id: Option<&'a str>,
    pub agent_type: Option<&'a str>,
    pub system_prompt: Option<&'a str>,
    pub prompt: Option<&'a str>,
    pub parent_session_id: Option<&'a str>,
}

/// One row from [`Store::list_unified_session_events`] (§8.6 read-side unified stream).
#[derive(Debug, Clone, serde::Serialize)]
pub struct UnifiedSessionEvent {
    pub stream: String,
    pub source_id: i64,
    pub kind: String,
    pub body: String,
    pub created_at: String,
}

/// Full notification row from SQLite (offset tail, claims, exports).
#[derive(Debug, Clone)]
pub struct NotificationRecord {
    pub id: String,
    pub category: String,
    pub kind: String,
    pub severity: String,
    pub payload: String,
    pub created_at: String,
    pub title: String,
    pub body: String,
    pub source_kind: String,
    pub source_id: String,
}

fn notification_record_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<NotificationRecord> {
    Ok(NotificationRecord {
        id: row.get(0)?,
        category: row.get(1)?,
        kind: row.get(2)?,
        severity: row.get(3)?,
        payload: row.get(4)?,
        created_at: row.get(5)?,
        title: row.get(6)?,
        body: row.get(7)?,
        source_kind: row.get(8)?,
        source_id: row.get(9)?,
    })
}

impl Store {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let conn = Connection::open(path)?;
        let store = Self {
            conn: Arc::new(Mutex::new(conn)),
        };
        store.init_schema()?;
        Ok(store)
    }

    fn init_schema(&self) -> anyhow::Result<()> {
        {
            let conn = self.conn.lock().unwrap();
            // Helper: check if a column exists on a table
            let column_exists = |table: &str, col: &str| -> bool {
                let mut stmt = match conn.prepare(&format!("PRAGMA table_info({})", table)) {
                    Ok(s) => s,
                    Err(_) => return false,
                };
                let rows = stmt.query_map([], |row| {
                    let name: String = row.get(1)?;
                    Ok(name)
                });
                match rows {
                    Ok(names) => names.filter_map(|n| n.ok()).any(|n| n == col),
                    Err(_) => false,
                }
            };
            // Migration: add dedupe_key to existing notifications tables
            if !column_exists("notifications", "dedupe_key") {
                let _ = conn.execute("ALTER TABLE notifications ADD COLUMN dedupe_key TEXT", []);
            }
            for col in ["title", "body", "source_kind", "source_id"] {
                if !column_exists("notifications", col) {
                    let _ = conn.execute(
                        &format!("ALTER TABLE notifications ADD COLUMN {col} TEXT DEFAULT ''"),
                        [],
                    );
                }
            }
            // Migration: add archived to existing sessions table
            if !column_exists("sessions", "archived") {
                let _ = conn.execute(
                    "ALTER TABLE sessions ADD COLUMN archived INTEGER NOT NULL DEFAULT 0",
                    [],
                );
            }
            if !column_exists("sessions", "parent_session_id") {
                let _ = conn.execute("ALTER TABLE sessions ADD COLUMN parent_session_id TEXT", []);
            }
            if !column_exists("sessions", "fork_parent_context_rowid") {
                let _ = conn.execute(
                    "ALTER TABLE sessions ADD COLUMN fork_parent_context_rowid INTEGER",
                    [],
                );
            }
            if !column_exists("subagents", "parent_session_id") {
                let _ = conn.execute(
                    "ALTER TABLE subagents ADD COLUMN parent_session_id TEXT",
                    [],
                );
            }
        }
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY,
                work_dir TEXT NOT NULL,
                archived INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL,
                parent_session_id TEXT,
                fork_parent_context_rowid INTEGER
            );
            CREATE TABLE IF NOT EXISTS context_entries (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                role TEXT NOT NULL,
                content TEXT,
                metadata TEXT,
                checkpoint_id INTEGER,
                token_count INTEGER,
                created_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS wire_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                event_type TEXT NOT NULL,
                payload TEXT NOT NULL,
                created_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS state (
                session_id TEXT PRIMARY KEY,
                data TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS subagents (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                parent_tool_call_id TEXT,
                agent_type TEXT,
                system_prompt TEXT,
                prompt TEXT,
                created_at TEXT NOT NULL,
                parent_session_id TEXT
            );
            CREATE TABLE IF NOT EXISTS notifications (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                category TEXT,
                kind TEXT,
                severity TEXT,
                payload TEXT,
                dedupe_key TEXT,
                title TEXT NOT NULL DEFAULT '',
                body TEXT NOT NULL DEFAULT '',
                source_kind TEXT NOT NULL DEFAULT '',
                source_id TEXT NOT NULL DEFAULT '',
                created_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS tasks (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                kind TEXT,
                spec TEXT,
                status TEXT,
                output TEXT,
                heartbeat_at TEXT,
                created_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS notification_offsets (
                consumer_id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                offset_id TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS notification_claims (
                claim_id TEXT PRIMARY KEY,
                notification_id TEXT NOT NULL,
                consumer_id TEXT NOT NULL,
                claimed_at TEXT NOT NULL,
                acked_at TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_notification_claims_consumer ON notification_claims(consumer_id);
            CREATE INDEX IF NOT EXISTS idx_notification_claims_notification ON notification_claims(notification_id);
            CREATE INDEX IF NOT EXISTS idx_context_session ON context_entries(session_id);
            CREATE INDEX IF NOT EXISTS idx_wire_session ON wire_events(session_id);
            "#,
        )?;
        Ok(())
    }

    pub fn create_session(&self, id: &str, work_dir: &str) -> SqlResult<()> {
        self.create_session_with_parent(id, work_dir, None, None)
    }

    /// Create a session row; `parent_session_id` / `fork_parent_context_rowid` record fork lineage (§8.6).
    pub fn create_session_with_parent(
        &self,
        id: &str,
        work_dir: &str,
        parent_session_id: Option<&str>,
        fork_parent_context_rowid: Option<i64>,
    ) -> SqlResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO sessions (id, work_dir, created_at, parent_session_id, fork_parent_context_rowid) VALUES (?1, ?2, datetime('now'), ?3, ?4)",
            params![id, work_dir, parent_session_id, fork_parent_context_rowid],
        )?;
        Ok(())
    }

    pub fn get_session(&self, id: &str) -> SqlResult<Option<(String, String)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT id, work_dir FROM sessions WHERE id = ?1")?;
        let mut rows = stmt.query(params![id])?;
        if let Some(row) = rows.next()? {
            Ok(Some((row.get(0)?, row.get(1)?)))
        } else {
            Ok(None)
        }
    }

    /// Parent session when this row was created via [`crate::session::Session::fork`].
    pub fn get_parent_session_id(&self, session_id: &str) -> SqlResult<Option<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT parent_session_id FROM sessions WHERE id = ?1")?;
        let mut rows = stmt.query(params![session_id])?;
        if let Some(row) = rows.next()? {
            Ok(row.get::<_, Option<String>>(0)?)
        } else {
            Ok(None)
        }
    }

    /// Direct forks of `parent_session_id` (newest child first).
    pub fn list_child_session_ids(&self, parent_session_id: &str) -> SqlResult<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT id FROM sessions WHERE parent_session_id = ?1 ORDER BY rowid DESC")?;
        let rows = stmt.query_map(params![parent_session_id], |row| row.get(0))?;
        rows.collect()
    }

    /// Max `context_entries.id` from the parent that was included when this session was forked (`None` if not a fork or full copy).
    pub fn get_fork_parent_context_rowid(&self, session_id: &str) -> SqlResult<Option<i64>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT fork_parent_context_rowid FROM sessions WHERE id = ?1")?;
        let mut rows = stmt.query(params![session_id])?;
        if let Some(row) = rows.next()? {
            Ok(row.get::<_, Option<i64>>(0)?)
        } else {
            Ok(None)
        }
    }

    /// Read-side union of context / wire / notification / task / state for `session_id` (§8.6 observability).
    pub fn list_unified_session_events(
        &self,
        session_id: &str,
        limit: usize,
    ) -> SqlResult<Vec<UnifiedSessionEvent>> {
        self.list_unified_session_events_filtered(session_id, None, limit)
    }

    /// Same as [`Self::list_unified_session_events`], but only rows strictly after `after_created_at` (SQLite `datetime` compare).
    /// Pass `None` for no lower bound. Used for tailing / pagination in CLI.
    pub fn list_unified_session_events_filtered(
        &self,
        session_id: &str,
        after_created_at: Option<&str>,
        limit: usize,
    ) -> SqlResult<Vec<UnifiedSessionEvent>> {
        let conn = self.conn.lock().unwrap();
        let cap = limit.clamp(1, 10_000);
        let mut stmt = conn.prepare(
            "SELECT stream, source_id, kind, body, created_at FROM (\
                SELECT 'context' AS stream, id AS source_id, role AS kind, \
                       substr(coalesce(content, '') || char(10) || coalesce(metadata, ''), 1, 8000) AS body, \
                       created_at FROM context_entries WHERE session_id = ?1 \
                UNION ALL \
                SELECT 'wire', id, event_type, substr(payload, 1, 8000), created_at \
                FROM wire_events WHERE session_id = ?1 \
                UNION ALL \
                SELECT 'notification', rowid, coalesce(kind, ''), \
                       substr(coalesce(title, '') || char(10) || coalesce(body, '') || char(10) || coalesce(payload, ''), 1, 8000), \
                       created_at \
                FROM notifications WHERE session_id = ?1 \
                UNION ALL \
                SELECT 'task', rowid, coalesce(status, ''), \
                       substr(coalesce(spec, '') || char(10) || coalesce(output, ''), 1, 8000), created_at \
                FROM tasks WHERE session_id = ?1 \
                UNION ALL \
                SELECT 'state', 0, 'snapshot', substr(data, 1, 8000), updated_at \
                FROM state WHERE session_id = ?1 \
            ) AS merged \
            WHERE (?2 IS NULL OR datetime(merged.created_at) > datetime(?2)) \
            ORDER BY datetime(merged.created_at), \
                CASE merged.stream \
                    WHEN 'context' THEN 0 \
                    WHEN 'wire' THEN 1 \
                    WHEN 'notification' THEN 2 \
                    WHEN 'task' THEN 3 \
                    WHEN 'state' THEN 4 \
                    ELSE 9 END, \
                merged.source_id \
            LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![session_id, after_created_at, cap], |row| {
            Ok(UnifiedSessionEvent {
                stream: row.get(0)?,
                source_id: row.get(1)?,
                kind: row.get(2)?,
                body: row.get(3)?,
                created_at: row.get(4)?,
            })
        })?;
        rows.collect()
    }

    pub fn list_sessions(&self) -> SqlResult<Vec<SessionRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT s.id, s.work_dir, s.created_at, st.data FROM sessions s LEFT JOIN state st ON s.id = st.session_id ORDER BY datetime(s.created_at) DESC, s.rowid DESC"
        )?;
        let rows = stmt.query_map([], |row| {
            let state_json: Option<String> = row.get(3)?;
            let title = state_json.and_then(|j| {
                serde_json::from_str::<serde_json::Value>(&j).ok()
                    .and_then(|v| v.get("title").and_then(|t| t.as_str()).map(|s| s.to_string()))
            });
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, title))
        })?;
        rows.collect()
    }

    pub fn list_unarchived_sessions(&self) -> SqlResult<Vec<SessionRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT s.id, s.work_dir, s.created_at, st.data FROM sessions s LEFT JOIN state st ON s.id = st.session_id WHERE s.archived = 0 ORDER BY datetime(s.created_at) DESC, s.rowid DESC"
        )?;
        let rows = stmt.query_map([], |row| {
            let state_json: Option<String> = row.get(3)?;
            let title = state_json.and_then(|j| {
                serde_json::from_str::<serde_json::Value>(&j).ok()
                    .and_then(|v| v.get("title").and_then(|t| t.as_str()).map(|s| s.to_string()))
            });
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, title))
        })?;
        rows.collect()
    }

    pub fn archive_session(&self, session_id: &str) -> SqlResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE sessions SET archived = 1 WHERE id = ?1",
            params![session_id],
        )?;
        Ok(())
    }

    pub fn append_context(
        &self,
        session_id: &str,
        role: &str,
        content: Option<&str>,
        metadata: Option<&str>,
        checkpoint_id: Option<i64>,
        token_count: Option<i64>,
    ) -> SqlResult<i64> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO context_entries (session_id, role, content, metadata, checkpoint_id, token_count, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, datetime('now'))",
            params![session_id, role, content, metadata, checkpoint_id, token_count],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn get_context(&self, session_id: &str) -> SqlResult<Vec<ContextRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, role, content, metadata, checkpoint_id, token_count FROM context_entries WHERE session_id = ?1 ORDER BY id"
        )?;
        let rows = stmt.query_map(params![session_id], |row| {
            Ok(ContextRow {
                id: row.get(0)?,
                role: row.get(1)?,
                content: row.get(2)?,
                metadata: row.get(3)?,
                checkpoint_id: row.get(4)?,
                token_count: row.get(5)?,
            })
        })?;
        rows.collect()
    }

    pub fn clear_context(&self, session_id: &str) -> SqlResult<usize> {
        let conn = self.conn.lock().unwrap();
        let affected = conn.execute(
            "DELETE FROM context_entries WHERE session_id = ?1",
            params![session_id],
        )?;
        Ok(affected)
    }

    pub fn revert_context_to_checkpoint(
        &self,
        session_id: &str,
        checkpoint_id: i64,
    ) -> SqlResult<usize> {
        let conn = self.conn.lock().unwrap();
        let affected = conn.execute(
            "DELETE FROM context_entries WHERE session_id = ?1 AND id > (SELECT COALESCE(MAX(id), 0) FROM context_entries WHERE session_id = ?1 AND checkpoint_id = ?2)",
            params![session_id, checkpoint_id],
        )?;
        Ok(affected)
    }

    pub fn set_state(&self, session_id: &str, data: &str) -> SqlResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO state (session_id, data, updated_at) VALUES (?1, ?2, datetime('now')) ON CONFLICT(session_id) DO UPDATE SET data = excluded.data, updated_at = excluded.updated_at",
            params![session_id, data],
        )?;
        Ok(())
    }

    pub fn get_state(&self, session_id: &str) -> SqlResult<Option<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT data FROM state WHERE session_id = ?1")?;
        let mut rows = stmt.query(params![session_id])?;
        if let Some(row) = rows.next()? {
            Ok(Some(row.get(0)?))
        } else {
            Ok(None)
        }
    }

    /// Maximum wire events retained per session before tail-compaction triggers.
    const WIRE_EVENTS_TRIM_THRESHOLD: usize = 10_000;
    /// Target count after tail-compaction.
    const WIRE_EVENTS_TRIM_TARGET: usize = 8_000;

    pub fn append_wire_event(
        &self,
        session_id: &str,
        event_type: &str,
        payload: &str,
    ) -> SqlResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO wire_events (session_id, event_type, payload, created_at) VALUES (?1, ?2, ?3, datetime('now'))",
            params![session_id, event_type, payload],
        )?;
        // Opportunistic tail compaction (§6.5): keep recent events, discard oldest.
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM wire_events WHERE session_id = ?1",
            params![session_id],
            |row| row.get(0),
        )?;
        if count > Self::WIRE_EVENTS_TRIM_THRESHOLD as i64 {
            conn.execute(
                "DELETE FROM wire_events WHERE session_id = ?1 AND id <= (
                    SELECT id FROM wire_events WHERE session_id = ?1 ORDER BY id DESC LIMIT 1 OFFSET ?2
                )",
                params![session_id, Self::WIRE_EVENTS_TRIM_TARGET],
            )?;
        }
        Ok(())
    }

    pub fn get_wire_events(&self, session_id: &str) -> SqlResult<Vec<(i64, String, String)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, event_type, payload FROM wire_events WHERE session_id = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map(params![session_id], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
        rows.collect()
    }

    pub fn create_subagent(&self, params: CreateSubagentParams<'_>) -> SqlResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO subagents (id, session_id, parent_tool_call_id, agent_type, system_prompt, prompt, created_at, parent_session_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6, datetime('now'), ?7)",
            params![
                params.id,
                params.session_id,
                params.parent_tool_call_id,
                params.agent_type,
                params.system_prompt,
                params.prompt,
                params.parent_session_id
            ],
        )?;
        Ok(())
    }

    #[allow(clippy::type_complexity)]
    pub fn get_subagent(
        &self,
        id: &str,
    ) -> SqlResult<
        Option<(
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
        )>,
    > {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT session_id, parent_tool_call_id, agent_type, system_prompt, parent_session_id FROM subagents WHERE id = ?1",
        )?;
        let mut rows = stmt.query(params![id])?;
        if let Some(row) = rows.next()? {
            Ok(Some((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            )))
        } else {
            Ok(None)
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn append_notification(
        &self,
        id: &str,
        session_id: &str,
        category: &str,
        kind: &str,
        severity: &str,
        payload: &str,
        dedupe_key: Option<&str>,
        title: &str,
        body: &str,
        source_kind: &str,
        source_id: &str,
    ) -> SqlResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO notifications (id, session_id, category, kind, severity, payload, dedupe_key, title, body, source_kind, source_id, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, datetime('now'))",
            params![
                id,
                session_id,
                category,
                kind,
                severity,
                payload,
                dedupe_key,
                title,
                body,
                source_kind,
                source_id
            ],
        )?;
        Ok(())
    }

    pub fn get_notifications(&self, session_id: &str) -> SqlResult<Vec<NotificationRecord>> {
        const SEL: &str = "SELECT id, category, kind, severity, payload, created_at, \
            title, body, source_kind, source_id FROM notifications WHERE session_id = ?1 ORDER BY created_at";
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(SEL)?;
        let rows = stmt.query_map(params![session_id], notification_record_from_row)?;
        rows.collect()
    }

    /// List notifications in insertion order, optionally only those after `after_notification_id` (§8.4 rowid tail).
    pub fn list_notifications_after(
        &self,
        session_id: &str,
        after_notification_id: Option<&str>,
        limit: usize,
    ) -> SqlResult<Vec<NotificationRecord>> {
        const SEL_TAIL: &str = "SELECT id, category, kind, severity, payload, created_at, \
            title, body, source_kind, source_id FROM notifications \
             WHERE session_id = ?1 ORDER BY rowid ASC LIMIT ?2";
        const SEL_AFTER: &str = "SELECT id, category, kind, severity, payload, created_at, \
            title, body, source_kind, source_id FROM notifications \
             WHERE session_id = ?1 \
               AND rowid > COALESCE(\
                 (SELECT rowid FROM notifications WHERE id = ?2 AND session_id = ?1),\
                 0\
               ) \
             ORDER BY rowid ASC LIMIT ?3";
        let conn = self.conn.lock().unwrap();
        let cap = limit.clamp(1, 500);
        match after_notification_id {
            None => {
                let mut stmt = conn.prepare(SEL_TAIL)?;
                let rows =
                    stmt.query_map(params![session_id, cap], notification_record_from_row)?;
                rows.collect()
            }
            Some(after) => {
                let mut stmt = conn.prepare(SEL_AFTER)?;
                let rows = stmt.query_map(
                    params![session_id, after, cap],
                    notification_record_from_row,
                )?;
                rows.collect()
            }
        }
    }

    /// Claim unclaimed notifications for a consumer, creating claim records.
    /// Returns the claimed notification rows.
    pub fn claim_notifications(
        &self,
        session_id: &str,
        consumer_id: &str,
        limit: usize,
    ) -> SqlResult<Vec<NotificationRecord>> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;

        // Select notifications that have never been claimed by this consumer
        let mut stmt = tx.prepare(
            "SELECT n.id, n.category, n.kind, n.severity, n.payload, n.created_at, \
                n.title, n.body, n.source_kind, n.source_id
             FROM notifications n
             WHERE n.session_id = ?1
               AND n.id NOT IN (
                   SELECT notification_id FROM notification_claims
                   WHERE consumer_id = ?2
               )
             ORDER BY n.created_at
             LIMIT ?3",
        )?;
        let rows: Vec<NotificationRecord> = stmt
            .query_map(
                params![session_id, consumer_id, limit],
                notification_record_from_row,
            )?
            .collect::<SqlResult<Vec<_>>>()?;
        stmt.finalize()?;

        // Insert claims
        for r in &rows {
            let claim_id = uuid::Uuid::new_v4().to_string();
            tx.execute(
                "INSERT INTO notification_claims (claim_id, notification_id, consumer_id, claimed_at) VALUES (?1, ?2, ?3, datetime('now'))",
                params![claim_id, &r.id, consumer_id],
            )?;
        }

        tx.commit()?;
        Ok(rows)
    }

    /// Acknowledge a specific notification claim for a consumer.
    pub fn ack_notification_claim(
        &self,
        notification_id: &str,
        consumer_id: &str,
    ) -> SqlResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE notification_claims SET acked_at = datetime('now') WHERE notification_id = ?1 AND consumer_id = ?2 AND acked_at IS NULL",
            params![notification_id, consumer_id],
        )?;
        Ok(())
    }

    /// Recover stale claims older than the given duration (in milliseconds).
    /// Deletes the stale claim records and returns the notification IDs for redelivery.
    pub fn recover_stale_claims(&self, stale_after_ms: i64) -> SqlResult<Vec<String>> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;

        let cutoff = format!("-{:.3} seconds", stale_after_ms as f64 / 1000.0);
        let mut stmt = tx.prepare(
            "SELECT notification_id FROM notification_claims
             WHERE acked_at IS NULL
               AND claimed_at < strftime('%Y-%m-%d %H:%M:%f', 'now', ?1)",
        )?;
        let ids: Vec<String> = stmt
            .query_map(params![cutoff], |row| row.get(0))?
            .collect::<SqlResult<Vec<_>>>()?;
        stmt.finalize()?;

        tx.execute(
            "DELETE FROM notification_claims
             WHERE acked_at IS NULL
               AND claimed_at < strftime('%Y-%m-%d %H:%M:%f', 'now', ?1)",
            params![&cutoff],
        )?;

        tx.commit()?;
        Ok(ids)
    }

    /// Check if a dedupe key already exists for this session.
    pub fn has_notification_dedupe(&self, session_id: &str, dedupe_key: &str) -> SqlResult<bool> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT 1 FROM notifications WHERE session_id = ?1 AND dedupe_key = ?2 LIMIT 1",
        )?;
        let mut rows = stmt.query(params![session_id, dedupe_key])?;
        Ok(rows.next()?.is_some())
    }

    pub fn create_task(
        &self,
        id: &str,
        session_id: &str,
        kind: &str,
        spec: &str,
        status: &str,
    ) -> SqlResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO tasks (id, session_id, kind, spec, status, created_at) VALUES (?1, ?2, ?3, ?4, ?5, datetime('now'))",
            params![id, session_id, kind, spec, status],
        )?;
        Ok(())
    }

    pub fn update_task_status(
        &self,
        id: &str,
        status: &str,
        output: Option<&str>,
    ) -> SqlResult<()> {
        let conn = self.conn.lock().unwrap();
        if let Some(out) = output {
            conn.execute(
                "UPDATE tasks SET status = ?1, output = ?2 WHERE id = ?3",
                params![status, out, id],
            )?;
        } else {
            conn.execute(
                "UPDATE tasks SET status = ?1 WHERE id = ?2",
                params![status, id],
            )?;
        }
        Ok(())
    }

    pub fn heartbeat_task(&self, id: &str) -> SqlResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE tasks SET heartbeat_at = datetime('now') WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    }

    #[allow(clippy::type_complexity)]
    pub fn get_task(
        &self,
        id: &str,
    ) -> SqlResult<Option<(String, String, String, Option<String>)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT session_id, kind, spec, status, output FROM tasks WHERE id = ?1")?;
        let mut rows = stmt.query(params![id])?;
        if let Some(row) = rows.next()? {
            Ok(Some((row.get(0)?, row.get(1)?, row.get(2)?, row.get(4)?)))
        } else {
            Ok(None)
        }
    }

    #[allow(clippy::type_complexity)]
    pub fn list_tasks(
        &self,
        session_id: &str,
    ) -> SqlResult<Vec<(String, String, String, String, Option<String>)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, kind, spec, status, output FROM tasks WHERE session_id = ?1 ORDER BY created_at"
        )?;
        let rows = stmt.query_map(params![session_id], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })?;
        rows.collect()
    }

    /// Minimum query length (trimmed) to include `state` / `notifications` in [`Self::search_sessions`].
    pub const SEARCH_MIN_LEN_STATE_NOTIFICATION: usize = 3;

    /// Cross-session search (§8.6 / §8.4): distinct `session_id` where context, wire, state, or notifications match `query`.
    /// Shorter queries only scan context + wire (avoids broad `state` / `notifications` LIKE).
    pub fn search_sessions(&self, query: &str) -> SqlResult<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let q = query.trim();
        let pattern = format!("%{q}%");
        let narrow = q.len() < Self::SEARCH_MIN_LEN_STATE_NOTIFICATION;
        let mut stmt = if narrow {
            conn.prepare(
                "SELECT session_id FROM (\
                    SELECT session_id FROM context_entries \
                    WHERE content LIKE ?1 OR metadata LIKE ?1 OR role LIKE ?1 \
                    UNION \
                    SELECT session_id FROM wire_events WHERE payload LIKE ?1 \
                ) ORDER BY session_id",
            )?
        } else {
            conn.prepare(
                "SELECT session_id FROM (\
                    SELECT session_id FROM context_entries \
                    WHERE content LIKE ?1 OR metadata LIKE ?1 OR role LIKE ?1 \
                    UNION \
                    SELECT session_id FROM wire_events WHERE payload LIKE ?1 \
                    UNION \
                    SELECT session_id FROM state WHERE data LIKE ?1 \
                    UNION \
                    SELECT session_id FROM notifications \
                    WHERE payload LIKE ?1 OR category LIKE ?1 OR kind LIKE ?1 OR severity LIKE ?1 \
                       OR coalesce(title, '') LIKE ?1 OR coalesce(body, '') LIKE ?1 \
                       OR coalesce(source_kind, '') LIKE ?1 OR coalesce(source_id, '') LIKE ?1 \
                ) ORDER BY session_id",
            )?
        };
        let rows = stmt.query_map(params![pattern], |row| row.get(0))?;
        rows.collect()
    }

    pub fn get_notification_offset(
        &self,
        consumer_id: &str,
        session_id: &str,
    ) -> SqlResult<Option<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT offset_id FROM notification_offsets WHERE consumer_id = ?1 AND session_id = ?2",
        )?;
        let mut rows = stmt.query(params![consumer_id, session_id])?;
        if let Some(row) = rows.next()? {
            Ok(Some(row.get(0)?))
        } else {
            Ok(None)
        }
    }

    pub fn set_notification_offset(
        &self,
        consumer_id: &str,
        session_id: &str,
        offset_id: &str,
    ) -> SqlResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO notification_offsets (consumer_id, session_id, offset_id, updated_at) VALUES (?1, ?2, ?3, datetime('now')) ON CONFLICT(consumer_id) DO UPDATE SET offset_id = excluded.offset_id, updated_at = excluded.updated_at",
            params![consumer_id, session_id, offset_id],
        )?;
        Ok(())
    }

    /// Export all sessions to JSONL.
    pub fn export_sessions_to_jsonl(&self, writer: &mut dyn std::io::Write) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT id, work_dir, created_at FROM sessions")?;
        let rows = stmt.query_map([], |row| {
            Ok(serde_json::json!({
                "table": "sessions",
                "id": row.get::<_, String>(0)?,
                "work_dir": row.get::<_, String>(1)?,
                "created_at": row.get::<_, String>(2)?,
            }))
        })?;
        for row in rows {
            writeln!(writer, "{}", row?)?;
        }
        Ok(())
    }

    /// Export context entries for a session to JSONL.
    pub fn export_context_to_jsonl(
        &self,
        session_id: &str,
        writer: &mut dyn std::io::Write,
    ) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, role, content, metadata, checkpoint_id, token_count, created_at FROM context_entries WHERE session_id = ?1"
        )?;
        let rows = stmt.query_map(params![session_id], |row| {
            Ok(serde_json::json!({
                "table": "context_entries",
                "id": row.get::<_, i64>(0)?,
                "role": row.get::<_, String>(1)?,
                "content": row.get::<_, Option<String>>(2)?,
                "metadata": row.get::<_, Option<String>>(3)?,
                "checkpoint_id": row.get::<_, Option<i64>>(4)?,
                "token_count": row.get::<_, Option<i64>>(5)?,
                "created_at": row.get::<_, String>(6)?,
            }))
        })?;
        for row in rows {
            writeln!(writer, "{}", row?)?;
        }
        Ok(())
    }

    /// Export wire events for a session to JSONL.
    pub fn export_wire_events_to_jsonl(
        &self,
        session_id: &str,
        writer: &mut dyn std::io::Write,
    ) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, event_type, payload, created_at FROM wire_events WHERE session_id = ?1",
        )?;
        let rows = stmt.query_map(params![session_id], |row| {
            Ok(serde_json::json!({
                "table": "wire_events",
                "id": row.get::<_, i64>(0)?,
                "event_type": row.get::<_, String>(1)?,
                "payload": row.get::<_, String>(2)?,
                "created_at": row.get::<_, String>(3)?,
            }))
        })?;
        for row in rows {
            writeln!(writer, "{}", row?)?;
        }
        Ok(())
    }

    /// Export tasks for a session to JSONL.
    pub fn export_tasks_to_jsonl(
        &self,
        session_id: &str,
        writer: &mut dyn std::io::Write,
    ) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, kind, spec, status, output, created_at FROM tasks WHERE session_id = ?1",
        )?;
        let rows = stmt.query_map(params![session_id], |row| {
            Ok(serde_json::json!({
                "table": "tasks",
                "id": row.get::<_, String>(0)?,
                "kind": row.get::<_, String>(1)?,
                "spec": row.get::<_, String>(2)?,
                "status": row.get::<_, String>(3)?,
                "output": row.get::<_, Option<String>>(4)?,
                "created_at": row.get::<_, String>(5)?,
            }))
        })?;
        for row in rows {
            writeln!(writer, "{}", row?)?;
        }
        Ok(())
    }

    /// Export notifications for a session to JSONL.
    pub fn export_notifications_to_jsonl(
        &self,
        session_id: &str,
        writer: &mut dyn std::io::Write,
    ) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, category, kind, severity, payload, created_at, title, body, source_kind, source_id \
             FROM notifications WHERE session_id = ?1"
        )?;
        let rows = stmt.query_map(params![session_id], |row| {
            Ok(serde_json::json!({
                "table": "notifications",
                "id": row.get::<_, String>(0)?,
                "category": row.get::<_, Option<String>>(1)?,
                "kind": row.get::<_, Option<String>>(2)?,
                "severity": row.get::<_, Option<String>>(3)?,
                "payload": row.get::<_, Option<String>>(4)?,
                "created_at": row.get::<_, String>(5)?,
                "title": row.get::<_, Option<String>>(6)?,
                "body": row.get::<_, Option<String>>(7)?,
                "source_kind": row.get::<_, Option<String>>(8)?,
                "source_id": row.get::<_, Option<String>>(9)?,
            }))
        })?;
        for row in rows {
            writeln!(writer, "{}", row?)?;
        }
        Ok(())
    }

    /// Export all data for a session to JSONL.
    pub fn export_session_to_jsonl(
        &self,
        session_id: &str,
        writer: &mut dyn std::io::Write,
    ) -> anyhow::Result<()> {
        self.export_context_to_jsonl(session_id, writer)?;
        self.export_wire_events_to_jsonl(session_id, writer)?;
        self.export_tasks_to_jsonl(session_id, writer)?;
        self.export_notifications_to_jsonl(session_id, writer)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_store() -> Store {
        Store::open(Path::new(":memory:")).unwrap()
    }

    #[test]
    fn test_session_crud() {
        let store = test_store();
        store.create_session("s1", "/tmp/wd").unwrap();

        let s = store.get_session("s1").unwrap().unwrap();
        assert_eq!(s.0, "s1");
        assert_eq!(s.1, "/tmp/wd");

        let missing = store.get_session("s2").unwrap();
        assert!(missing.is_none());

        let all = store.list_sessions().unwrap();
        assert_eq!(all.len(), 1);
    }

    #[test]
    fn test_list_sessions_reads_title_from_state() {
        let store = test_store();
        store.create_session("s1", "/tmp/wd").unwrap();
        store.set_state("s1", r#"{"title":"My Session"}"#).unwrap();

        let all = store.list_sessions().unwrap();
        assert_eq!(all.len(), 1);
        let (_, _, _, title) = &all[0];
        assert_eq!(title.as_deref(), Some("My Session"));
    }

    #[test]
    fn test_list_sessions_untitled_when_no_state() {
        let store = test_store();
        store.create_session("s1", "/tmp/wd").unwrap();

        let all = store.list_sessions().unwrap();
        assert_eq!(all.len(), 1);
        let (_, _, _, title) = &all[0];
        assert_eq!(title.as_deref(), None);
    }

    #[test]
    fn test_archive_session_excludes_from_unarchived_list() {
        let store = test_store();
        store.create_session("s1", "/tmp").unwrap();
        store.archive_session("s1").unwrap();

        let unarchived = store.list_unarchived_sessions().unwrap();
        assert!(unarchived.iter().all(|(id, _, _, _)| id != "s1"));

        let all = store.list_sessions().unwrap();
        assert!(all.iter().any(|(id, _, _, _)| id == "s1"));
    }

    #[test]
    fn test_context_entries_roundtrip() {
        let store = test_store();
        store.create_session("s1", "/tmp").unwrap();

        store
            .append_context("s1", "user", Some("hello"), None, None, None)
            .unwrap();
        store
            .append_context("s1", "assistant", Some("hi"), None, None, Some(42))
            .unwrap();

        let rows = store.get_context("s1").unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].role, "user");
        assert_eq!(rows[0].content.as_deref(), Some("hello"));
        assert_eq!(rows[1].role, "assistant");
        assert_eq!(rows[1].token_count, Some(42));
    }

    #[test]
    fn test_context_clear() {
        let store = test_store();
        store.create_session("s1", "/tmp").unwrap();
        store
            .append_context("s1", "user", Some("a"), None, None, None)
            .unwrap();
        store
            .append_context("s1", "user", Some("b"), None, None, None)
            .unwrap();

        let deleted = store.clear_context("s1").unwrap();
        assert_eq!(deleted, 2);
        assert!(store.get_context("s1").unwrap().is_empty());
    }

    #[test]
    fn test_context_revert_to_checkpoint() {
        let store = test_store();
        store.create_session("s1", "/tmp").unwrap();
        store
            .append_context("s1", "_checkpoint", None, None, Some(1), None)
            .unwrap();
        store
            .append_context("s1", "user", Some("after ck1"), None, None, None)
            .unwrap();
        store
            .append_context("s1", "_checkpoint", None, None, Some(2), None)
            .unwrap();
        store
            .append_context("s1", "user", Some("after ck2"), None, None, None)
            .unwrap();

        let deleted = store.revert_context_to_checkpoint("s1", 1).unwrap();
        assert_eq!(deleted, 3); // ck2 + user after ck2 + user after ck1 (all after checkpoint 1)

        let rows = store.get_context("s1").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].role, "_checkpoint");
    }

    #[test]
    fn test_state_roundtrip() {
        let store = test_store();
        store.create_session("s1", "/tmp").unwrap();

        let empty = store.get_state("s1").unwrap();
        assert!(empty.is_none());

        store.set_state("s1", r#"{"yolo":true}"#).unwrap();
        let data = store.get_state("s1").unwrap().unwrap();
        assert_eq!(data, r#"{"yolo":true}"#);

        store.set_state("s1", r#"{"yolo":false}"#).unwrap();
        let updated = store.get_state("s1").unwrap().unwrap();
        assert_eq!(updated, r#"{"yolo":false}"#);
    }

    #[test]
    fn test_wire_events_roundtrip() {
        let store = test_store();
        store.create_session("s1", "/tmp").unwrap();

        store
            .append_wire_event("s1", "TurnBegin", r#"{"text":"hello"}"#)
            .unwrap();
        store
            .append_wire_event("s1", "TextPart", r#"{"text":"world"}"#)
            .unwrap();

        let events = store.get_wire_events("s1").unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].1, "TurnBegin");
        assert_eq!(events[1].2, r#"{"text":"world"}"#);
    }

    #[test]
    fn test_wire_events_tail_compaction() {
        let store = test_store();
        store.create_session("s1", "/tmp").unwrap();

        let threshold = Store::WIRE_EVENTS_TRIM_THRESHOLD;
        let target = Store::WIRE_EVENTS_TRIM_TARGET;

        // Insert enough events to trigger compaction on the last append.
        for i in 0..=threshold {
            store
                .append_wire_event("s1", "TextPart", &format!(r#"{{"n":{i}}}"#))
                .unwrap();
        }

        let events = store.get_wire_events("s1").unwrap();
        // After trimming, we should have roughly (target + 1) events left
        // (target from the subquery offset + the row that triggered the delete).
        assert!(
            events.len() <= target + 100,
            "expected <= {} events after trim, got {}",
            target + 100,
            events.len()
        );
        assert!(
            events.len() >= target - 100,
            "expected >= {} events after trim, got {}",
            target - 100,
            events.len()
        );

        // The oldest remaining event should be near the trim boundary.
        let first_n: i64 = serde_json::from_str::<serde_json::Value>(&events[0].2)
            .unwrap()["n"]
            .as_i64()
            .unwrap();
        assert!(
            first_n >= (threshold - target - 100) as i64,
            "oldest remaining event n={first_n} should be near boundary"
        );

        // The newest event should always be preserved.
        let last_n: i64 = serde_json::from_str::<serde_json::Value>(&events.last().unwrap().2)
            .unwrap()["n"]
            .as_i64()
            .unwrap();
        assert_eq!(last_n, threshold as i64);
    }

    #[test]
    fn test_subagent_crud() {
        let store = test_store();
        store.create_session("s1", "/tmp").unwrap();

        store
            .create_subagent(CreateSubagentParams {
                id: "sa1",
                session_id: "s1",
                parent_tool_call_id: Some("tc-1"),
                agent_type: Some("coder"),
                system_prompt: Some("sys"),
                prompt: Some("do x"),
                parent_session_id: Some("s1"),
            })
            .unwrap();
        let sa = store.get_subagent("sa1").unwrap().unwrap();
        assert_eq!(sa.0, "s1");
        assert_eq!(sa.1.as_deref(), Some("tc-1"));
        assert_eq!(sa.2.as_deref(), Some("coder"));
        assert_eq!(sa.3.as_deref(), Some("sys"));
        assert_eq!(sa.4.as_deref(), Some("s1"));

        assert!(store.get_subagent("missing").unwrap().is_none());
    }

    #[test]
    fn test_notification_crud() {
        let store = test_store();
        store.create_session("s1", "/tmp").unwrap();

        store
            .append_notification(
                "n1",
                "s1",
                "task",
                "done",
                "info",
                r#"{"x":1}"#,
                None,
                "",
                "",
                "",
                "",
            )
            .unwrap();
        store
            .append_notification(
                "n2",
                "s1",
                "agent",
                "error",
                "warn",
                r#"{"y":2}"#,
                None,
                "",
                "",
                "",
                "",
            )
            .unwrap();

        let notifs = store.get_notifications("s1").unwrap();
        assert_eq!(notifs.len(), 2);
        assert_eq!(notifs[0].category, "task");
        assert_eq!(notifs[1].severity, "warn");
    }

    #[test]
    fn test_task_crud() {
        let store = test_store();
        store.create_session("s1", "/tmp").unwrap();

        store
            .create_task("t1", "s1", "bash", r#"{"cmd":"echo hi"}"#, "pending")
            .unwrap();
        let t = store.get_task("t1").unwrap().unwrap();
        assert_eq!(t.0, "s1");
        assert_eq!(t.1, "bash");
        assert_eq!(t.3, None); // output is initially None

        store
            .update_task_status("t1", "running", Some("output"))
            .unwrap();
        let updated = store.get_task("t1").unwrap().unwrap();
        assert_eq!(updated.3.as_deref(), Some("output"));

        store.heartbeat_task("t1").unwrap();

        let list = store.list_tasks("s1").unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].0, "t1");
        assert_eq!(list[0].3, "running");
        assert_eq!(list[0].4.as_deref(), Some("output"));
    }

    #[test]
    fn test_search_sessions() {
        let store = test_store();
        store.create_session("s1", "/tmp/a").unwrap();
        store.create_session("s2", "/tmp/b").unwrap();
        store
            .append_context("s1", "user", Some("hello world"), None, None, None)
            .unwrap();
        store
            .append_context("s2", "user", Some("goodbye"), None, None, None)
            .unwrap();

        let results = store.search_sessions("hello").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0], "s1");

        store
            .append_wire_event("s2", "TextPart", r#"{"text":"hello wire"}"#)
            .unwrap();
        let wire_hits = store.search_sessions("hello wire").unwrap();
        assert_eq!(wire_hits, vec!["s2".to_string()]);

        store
            .set_state("s1", r#"{"title":"hello state snapshot"}"#)
            .unwrap();
        let state_hits = store.search_sessions("hello state").unwrap();
        assert!(state_hits.contains(&"s1".to_string()));

        store
            .append_notification(
                "n1",
                "s2",
                "hello_cat",
                "ping",
                "info",
                r#"{"msg":"hello notify"}"#,
                None,
                "",
                "",
                "",
                "",
            )
            .unwrap();
        let notif_hits = store.search_sessions("hello notify").unwrap();
        assert!(notif_hits.contains(&"s2".to_string()));

        let none = store.search_sessions("xyz").unwrap();
        assert!(none.is_empty());
    }

    #[test]
    fn test_list_unified_session_events_interleaves_streams() {
        let store = test_store();
        store.create_session("s1", "/tmp").unwrap();
        store
            .append_context("s1", "user", Some("ctx-a"), None, None, None)
            .unwrap();
        store
            .append_wire_event("s1", "TextPart", r#"{"text":"wire-b"}"#)
            .unwrap();
        store
            .append_notification(
                "n1",
                "s1",
                "c",
                "k",
                "info",
                r#"{"x":1}"#,
                None,
                "",
                "",
                "",
                "",
            )
            .unwrap();
        store
            .create_task("t1", "s1", "bash", r#"{"cmd":"echo"}"#, "running")
            .unwrap();
        store.set_state("s1", r#"{"k":1}"#).unwrap();

        let rows = store.list_unified_session_events("s1", 50).unwrap();
        assert!(rows.iter().any(|r| r.stream == "context"));
        assert!(rows.iter().any(|r| r.stream == "wire"));
        assert!(rows.iter().any(|r| r.stream == "notification"));
        assert!(rows.iter().any(|r| r.stream == "task"));
        assert!(rows.iter().any(|r| r.stream == "state"));
        let streams: Vec<_> = rows.iter().map(|r| r.stream.as_str()).collect();
        let first_wire = streams.iter().position(|s| *s == "wire").unwrap();
        let first_ctx = streams.iter().position(|s| *s == "context").unwrap();
        assert!(
            first_ctx < first_wire,
            "same-second ordering should list context before wire, got {:?}",
            streams
        );
        let first_notif = streams.iter().position(|s| *s == "notification").unwrap();
        let first_task = streams.iter().position(|s| *s == "task").unwrap();
        let first_state = streams.iter().position(|s| *s == "state").unwrap();
        assert!(first_wire < first_notif && first_notif < first_task && first_task < first_state);
    }

    #[test]
    fn test_list_unified_session_events_filtered_after_cursor() {
        let store = test_store();
        store.create_session("s1", "/tmp").unwrap();
        store
            .append_context("s1", "user", Some("hello"), None, None, None)
            .unwrap();

        let all = store
            .list_unified_session_events_filtered("s1", Some("1970-01-01 00:00:00"), 50)
            .unwrap();
        assert!(!all.is_empty());

        let none = store
            .list_unified_session_events_filtered("s1", Some("9999-12-31 23:59:59"), 50)
            .unwrap();
        assert!(none.is_empty());
    }

    #[test]
    fn test_search_sessions_short_query_skips_state_and_notifications() {
        let store = test_store();
        store.create_session("s1", "/tmp").unwrap();
        store.set_state("s1", r#"{"secret":"xyzzy"}"#).unwrap();
        store
            .append_notification(
                "n1",
                "s1",
                "x",
                "y",
                "info",
                r#"{"z":1}"#,
                None,
                "",
                "",
                "",
                "",
            )
            .unwrap();

        let short = store.search_sessions("xy").unwrap();
        assert!(
            !short.contains(&"s1".to_string()),
            "2-char query should not scan state/notifications"
        );

        let long = store.search_sessions("xyz").unwrap();
        assert!(long.contains(&"s1".to_string()));
    }

    #[test]
    fn test_export_sessions_to_jsonl() {
        let store = test_store();
        store.create_session("s1", "/tmp/a").unwrap();
        store.create_session("s2", "/tmp/b").unwrap();

        let mut buf = Vec::new();
        store.export_sessions_to_jsonl(&mut buf).unwrap();
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("s1"));
        assert!(output.contains("s2"));
        assert!(output.contains("sessions"));
    }

    #[test]
    fn test_export_context_to_jsonl() {
        let store = test_store();
        store.create_session("s1", "/tmp").unwrap();
        store
            .append_context("s1", "user", Some("hello"), None, None, None)
            .unwrap();

        let mut buf = Vec::new();
        store.export_context_to_jsonl("s1", &mut buf).unwrap();
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("context_entries"));
        assert!(output.contains("hello"));
    }

    #[test]
    fn test_export_wire_events_to_jsonl() {
        let store = test_store();
        store.create_session("s1", "/tmp").unwrap();
        store
            .append_wire_event("s1", "TurnBegin", r#"{"text":"hi"}"#)
            .unwrap();

        let mut buf = Vec::new();
        store.export_wire_events_to_jsonl("s1", &mut buf).unwrap();
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("wire_events"));
        assert!(output.contains("TurnBegin"));
    }

    #[test]
    fn test_export_tasks_to_jsonl() {
        let store = test_store();
        store.create_session("s1", "/tmp").unwrap();
        store
            .create_task("t1", "s1", "bash", r#"{"cmd":"echo"}"#, "done")
            .unwrap();

        let mut buf = Vec::new();
        store.export_tasks_to_jsonl("s1", &mut buf).unwrap();
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("tasks"));
        assert!(output.contains("t1"));
    }

    #[test]
    fn test_export_notifications_to_jsonl() {
        let store = test_store();
        store.create_session("s1", "/tmp").unwrap();
        store
            .append_notification(
                "n1", "s1", "task", "done", "info", r#"{}"#, None, "", "", "", "",
            )
            .unwrap();

        let mut buf = Vec::new();
        store.export_notifications_to_jsonl("s1", &mut buf).unwrap();
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("notifications"));
        assert!(output.contains("n1"));
    }

    #[test]
    fn test_export_session_to_jsonl() {
        let store = test_store();
        store.create_session("s1", "/tmp").unwrap();
        store
            .append_context("s1", "user", Some("hello"), None, None, None)
            .unwrap();
        store.append_wire_event("s1", "TurnBegin", r#"{}"#).unwrap();

        let mut buf = Vec::new();
        store.export_session_to_jsonl("s1", &mut buf).unwrap();
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("context_entries"));
        assert!(output.contains("wire_events"));
    }

    #[test]
    fn test_notification_offset_roundtrip() {
        let store = test_store();
        store
            .set_notification_offset("consumer-a", "s1", "off-1")
            .unwrap();
        let off = store.get_notification_offset("consumer-a", "s1").unwrap();
        assert_eq!(off.as_deref(), Some("off-1"));

        store
            .set_notification_offset("consumer-a", "s1", "off-2")
            .unwrap();
        let updated = store.get_notification_offset("consumer-a", "s1").unwrap();
        assert_eq!(updated.as_deref(), Some("off-2"));

        assert!(
            store
                .get_notification_offset("missing", "s1")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn test_list_notifications_after_rowid() {
        let store = test_store();
        store.create_session("s1", "/tmp").unwrap();
        store
            .append_notification("n1", "s1", "a", "k1", "info", r#"{}"#, None, "", "", "", "")
            .unwrap();
        store
            .append_notification("n2", "s1", "a", "k2", "info", r#"{}"#, None, "", "", "", "")
            .unwrap();
        let head = store.list_notifications_after("s1", None, 10).unwrap();
        assert_eq!(head.len(), 2);
        assert_eq!(head[0].id, "n1");
        let tail = store
            .list_notifications_after("s1", Some("n1"), 10)
            .unwrap();
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].id, "n2");
        let empty = store
            .list_notifications_after("s1", Some("n2"), 10)
            .unwrap();
        assert!(empty.is_empty());
    }
}
