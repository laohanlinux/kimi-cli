//! Session lifecycle: create, resume, fork, and auto-title.
//!
//! Sessions are backed by a per-session directory under `~/.kimi/sessions/`
//! and tracked in the SQLite store.

use std::path::{Path, PathBuf};

use crate::store::Store;

/// True if `stored` (from DB) refers to the same work directory as `requested` (§8.6 resume).
fn work_dir_matches(stored: &str, requested: &Path) -> bool {
    let req_lossy = requested.to_string_lossy();
    if stored == req_lossy.as_ref() {
        return true;
    }
    let stored_path = Path::new(stored);
    match (stored_path.canonicalize(), requested.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Returns the base directory for session storage.
/// Respects `KIMI_SESSION_DIR` env var for testing/sandbox environments.
/// Falls back to a temp directory when the home directory is not writable.
fn session_base_dir() -> anyhow::Result<PathBuf> {
    if let Ok(env_dir) = std::env::var("KIMI_SESSION_DIR") {
        let path = PathBuf::from(env_dir);
        std::fs::create_dir_all(&path)?;
        return Ok(path);
    }
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("No home directory found"))?;
    let base = home.join(".kimi").join("sessions");
    match std::fs::create_dir_all(&base) {
        Ok(()) => return Ok(base),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            let tmp = std::env::temp_dir().join("kimi-sessions");
            std::fs::create_dir_all(&tmp)?;
            return Ok(tmp);
        }
        Err(e) => return Err(e.into()),
    }
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Session {
    pub id: String,
    pub dir: PathBuf,
    pub work_dir: PathBuf,
}

impl Session {
    pub fn create(store: &Store, work_dir: PathBuf) -> anyhow::Result<Self> {
        Self::create_with_parent(store, work_dir, None, None)
    }

    fn create_with_parent(
        store: &Store,
        work_dir: PathBuf,
        parent_session_id: Option<&str>,
        fork_parent_context_rowid: Option<i64>,
    ) -> anyhow::Result<Self> {
        let id = uuid::Uuid::new_v4().to_string();
        let base = session_base_dir()?;
        let dir = base.join(&id);
        if let Err(e) = std::fs::create_dir_all(&dir) {
            if e.kind() == std::io::ErrorKind::PermissionDenied && base != std::env::temp_dir().join("kimi-sessions") {
                let tmp_base = std::env::temp_dir().join("kimi-sessions");
                std::fs::create_dir_all(&tmp_base)?;
                let dir = tmp_base.join(&id);
                std::fs::create_dir_all(&dir)?;
                return Self::finish_create(store, id, dir, work_dir, parent_session_id, fork_parent_context_rowid);
            }
            return Err(e.into());
        }
        Self::finish_create(store, id, dir, work_dir, parent_session_id, fork_parent_context_rowid)
    }

    fn finish_create(
        store: &Store,
        id: String,
        dir: PathBuf,
        work_dir: PathBuf,
        parent_session_id: Option<&str>,
        fork_parent_context_rowid: Option<i64>,
    ) -> anyhow::Result<Self> {
        store.create_session_with_parent(
            &id,
            work_dir.to_string_lossy().as_ref(),
            parent_session_id,
            fork_parent_context_rowid,
        )?;
        Ok(Self { id, dir, work_dir })
    }

    pub fn discover_latest(store: &Store, work_dir: PathBuf) -> anyhow::Result<Self> {
        let sessions = store.list_unarchived_sessions()?;
        for (id, wd, _) in sessions {
            if work_dir_matches(&wd, &work_dir) {
                let home =
                    dirs::home_dir().ok_or_else(|| anyhow::anyhow!("No home directory found"))?;
                let dir = home.join(".kimi").join("sessions").join(&id);
                return Ok(Self { id, dir, work_dir });
            }
        }
        Self::create(store, work_dir)
    }

    /// Load an existing session by exact ID.
    pub fn load_by_id(store: &Store, id: &str) -> anyhow::Result<Self> {
        let row = store.get_session(id)?;
        if let Some((id, work_dir)) = row {
            let dir = session_base_dir()?.join(&id);
            return Ok(Self { id, dir, work_dir: PathBuf::from(work_dir) });
        }
        anyhow::bail!("Session not found: {}", id)
    }

    /// Fork a new session from an existing one, copying all context rows (§8.6 lineage).
    pub fn fork(store: &Store, parent_id: &str, work_dir: PathBuf) -> anyhow::Result<Self> {
        Self::fork_with_context_cursor(store, parent_id, work_dir, None)
    }

    /// Fork from `parent_id`, copying only context rows with `id <= max_context_entry_id`.
    /// When `max_context_entry_id` is `None`, copies the full history (same as [`Self::fork`]).
    pub fn fork_with_context_cursor(
        store: &Store,
        parent_id: &str,
        work_dir: PathBuf,
        max_context_entry_id: Option<i64>,
    ) -> anyhow::Result<Self> {
        let new_session = Self::create_with_parent(store, work_dir, Some(parent_id), max_context_entry_id)?;
        let entries = store.get_context(parent_id)?;
        for row in entries {
            if let Some(max_id) = max_context_entry_id {
                if row.id > max_id {
                    continue;
                }
            }
            store.append_context(
                &new_session.id,
                &row.role,
                row.content.as_deref(),
                row.metadata.as_deref(),
                row.checkpoint_id,
                row.token_count,
            )?;
        }
        Ok(new_session)
    }

    /// Auto-set session title from the first user message, if not already set.
    pub fn auto_title(&self, store: &Store) -> anyhow::Result<()> {
        let state = store.get_state(&self.id)?;
        let mut data = match state {
            Some(s) => serde_json::from_str::<serde_json::Value>(&s).unwrap_or_else(|_| serde_json::json!({})),
            None => serde_json::json!({}),
        };
        if data.get("title").and_then(|v| v.as_str()).is_some() {
            return Ok(()); // already set
        }
        // Find first user message
        let entries = store.get_context(&self.id)?;
        for row in entries {
            if row.role == "user" {
                let raw = row.content.as_deref().unwrap_or("Untitled");
                let title_source = match crate::message::UserMessage::from_persistent_string(raw) {
                    Ok(u) => {
                        let s = u.flatten_for_recall();
                        if s.is_empty() {
                            raw.to_string()
                        } else {
                            s
                        }
                    }
                    Err(_) => raw.to_string(),
                };
                let title = if title_source.len() > 50 {
                    format!("{}...", &title_source[..50])
                } else {
                    title_source
                };
                data["title"] = serde_json::Value::String(title);
                store.set_state(&self.id, &serde_json::to_string(&data)?)?;
                break;
            }
        }
        Ok(())
    }

    /// Archive this session so it is skipped by `discover_latest`.
    pub fn archive(&self, store: &Store) -> anyhow::Result<()> {
        store.archive_session(&self.id)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_session_create_and_fork() {
        let store = Store::open(std::path::Path::new(":memory:")).unwrap();
        let work = std::env::current_dir().unwrap();

        let parent = Session::create(&store, work.clone()).unwrap();
        store.append_context(&parent.id, "user", Some("hello"), None, None, None).unwrap();
        store.append_context(&parent.id, "assistant", Some("hi"), None, None, None).unwrap();

        let child = Session::fork(&store, &parent.id, work.clone()).unwrap();
        assert_ne!(child.id, parent.id);

        let child_ctx = store.get_context(&child.id).unwrap();
        assert_eq!(child_ctx.len(), 2);
        assert_eq!(child_ctx[0].role, "user");
        assert_eq!(child_ctx[1].role, "assistant");
    }

    #[test]
    fn test_fork_records_parent_session_id() {
        let store = Store::open(std::path::Path::new(":memory:")).unwrap();
        let work = std::env::current_dir().unwrap();
        let parent = Session::create(&store, work.clone()).unwrap();
        let child = Session::fork(&store, &parent.id, work).unwrap();
        assert_eq!(
            store.get_parent_session_id(&child.id).unwrap().as_deref(),
            Some(parent.id.as_str())
        );
        assert!(store.get_parent_session_id(&parent.id).unwrap().is_none());
    }

    #[test]
    fn test_fork_with_context_cursor() {
        let store = Store::open(std::path::Path::new(":memory:")).unwrap();
        let work = std::env::current_dir().unwrap();
        let parent = Session::create(&store, work.clone()).unwrap();
        store
            .append_context(&parent.id, "user", Some("first"), None, None, None)
            .unwrap();
        let after_second = store
            .append_context(
                &parent.id,
                "assistant",
                Some("second"),
                None,
                None,
                None,
            )
            .unwrap();
        store
            .append_context(&parent.id, "user", Some("third"), None, None, None)
            .unwrap();

        let child =
            Session::fork_with_context_cursor(&store, &parent.id, work.clone(), Some(after_second))
                .unwrap();
        assert_eq!(
            store.get_fork_parent_context_rowid(&child.id).unwrap(),
            Some(after_second)
        );
        let ctx = store.get_context(&child.id).unwrap();
        assert_eq!(ctx.len(), 2);
        assert_eq!(ctx[0].content.as_deref(), Some("first"));
        assert_eq!(ctx[1].content.as_deref(), Some("second"));

        let full = Session::fork_with_context_cursor(&store, &parent.id, work, None).unwrap();
        assert_eq!(store.get_context(&full.id).unwrap().len(), 3);
        assert_eq!(store.get_fork_parent_context_rowid(&full.id).unwrap(), None);
    }

    #[test]
    fn test_list_child_session_ids() {
        let store = Store::open(std::path::Path::new(":memory:")).unwrap();
        let work = std::env::current_dir().unwrap();
        let parent = Session::create(&store, work.clone()).unwrap();
        let c1 = Session::fork(&store, &parent.id, work.clone()).unwrap();
        let c2 = Session::fork(&store, &parent.id, work).unwrap();
        let kids = store.list_child_session_ids(&parent.id).unwrap();
        assert_eq!(kids.len(), 2);
        assert_eq!(kids[0], c2.id);
        assert_eq!(kids[1], c1.id);
    }

    #[test]
    fn test_session_load_by_id() {
        let store = Store::open(std::path::Path::new(":memory:")).unwrap();
        let work = std::env::current_dir().unwrap();
        let s1 = Session::create(&store, work.clone()).unwrap();

        let loaded = Session::load_by_id(&store, &s1.id).unwrap();
        assert_eq!(loaded.id, s1.id);
        assert_eq!(loaded.work_dir, work);
    }

    #[test]
    fn test_session_load_by_id_missing() {
        let store = Store::open(std::path::Path::new(":memory:")).unwrap();
        let result = Session::load_by_id(&store, "nonexistent-id");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("nonexistent-id"));
    }

    #[test]
    fn test_search_sessions() {
        let store = Store::open(std::path::Path::new(":memory:")).unwrap();
        let work = std::env::current_dir().unwrap();

        let s1 = Session::create(&store, work.clone()).unwrap();
        store.append_context(&s1.id, "user", Some("rust code here"), None, None, None).unwrap();

        let s2 = Session::create(&store, work.clone()).unwrap();
        store.append_context(&s2.id, "user", Some("python code here"), None, None, None).unwrap();

        let results = store.search_sessions("rust").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0], s1.id);

        let results = store.search_sessions("code").unwrap();
        assert_eq!(results.len(), 2);
        assert!(results.contains(&s1.id));
        assert!(results.contains(&s2.id));
    }

    #[test]
    fn test_discover_latest_picks_latest_for_same_work_dir() {
        let store = Store::open(std::path::Path::new(":memory:")).unwrap();
        let work = std::env::current_dir().unwrap();

        let first = Session::create(&store, work.clone()).unwrap();
        let second = Session::create(&store, work.clone()).unwrap();
        assert_ne!(first.id, second.id);

        let resumed = Session::discover_latest(&store, work.clone()).unwrap();
        assert_eq!(resumed.id, second.id);
    }

    #[test]
    fn test_discover_latest_ignores_other_work_dir() {
        let store = Store::open(std::path::Path::new(":memory:")).unwrap();
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();

        let _other = Session::create(&store, dir_a.path().to_path_buf()).unwrap();
        let mine = Session::create(&store, dir_b.path().to_path_buf()).unwrap();

        let resumed = Session::discover_latest(&store, dir_b.path().to_path_buf()).unwrap();
        assert_eq!(resumed.id, mine.id);
    }

    #[test]
    fn test_auto_title_from_first_user_message() {
        let store = Store::open(std::path::Path::new(":memory:")).unwrap();
        let work = std::env::current_dir().unwrap();

        let session = Session::create(&store, work.clone()).unwrap();
        store.append_context(&session.id, "system", Some("system prompt"), None, None, None).unwrap();
        store.append_context(&session.id, "user", Some("how do I refactor this code?"), None, None, None).unwrap();

        session.auto_title(&store).unwrap();

        let state = store.get_state(&session.id).unwrap().unwrap();
        let data: serde_json::Value = serde_json::from_str(&state).unwrap();
        assert_eq!(data["title"].as_str().unwrap(), "how do I refactor this code?");
    }

    #[test]
    fn test_auto_title_truncates_long_messages() {
        let store = Store::open(std::path::Path::new(":memory:")).unwrap();
        let work = std::env::current_dir().unwrap();

        let session = Session::create(&store, work.clone()).unwrap();
        let long_msg = "a".repeat(100);
        store.append_context(&session.id, "user", Some(&long_msg), None, None, None).unwrap();

        session.auto_title(&store).unwrap();

        let state = store.get_state(&session.id).unwrap().unwrap();
        let data: serde_json::Value = serde_json::from_str(&state).unwrap();
        let title = data["title"].as_str().unwrap();
        assert!(title.len() <= 53); // 50 + "..."
        assert!(title.ends_with("..."));
    }

    #[test]
    fn test_session_fork_copies_context() {
        let store = Store::open(std::path::Path::new(":memory:")).unwrap();
        let work = std::env::current_dir().unwrap();

        let parent = Session::create(&store, work.clone()).unwrap();
        store.append_context(&parent.id, "user", Some("hello"), None, None, None).unwrap();
        store.append_context(&parent.id, "assistant", Some("hi"), None, None, None).unwrap();

        let child = Session::fork(&store, &parent.id, work.clone()).unwrap();
        let child_context = store.get_context(&child.id).unwrap();
        assert_eq!(child_context.len(), 2);
    }

    #[test]
    fn test_discover_latest_creates_when_empty() {
        let store = Store::open(std::path::Path::new(":memory:")).unwrap();
        let work = std::env::current_dir().unwrap();

        // No sessions exist yet
        let session = Session::discover_latest(&store, work.clone()).unwrap();
        assert!(!session.id.is_empty());
    }

    #[test]
    fn test_auto_title_skips_when_already_set() {
        let store = Store::open(std::path::Path::new(":memory:")).unwrap();
        let work = std::env::current_dir().unwrap();

        let session = Session::create(&store, work.clone()).unwrap();
        store.set_state(&session.id, r#"{"title": "Existing"}"#).unwrap();

        store.append_context(&session.id, "user", Some("new message"), None, None, None).unwrap();
        session.auto_title(&store).unwrap();

        let state = store.get_state(&session.id).unwrap().unwrap();
        let data: serde_json::Value = serde_json::from_str(&state).unwrap();
        assert_eq!(data["title"].as_str().unwrap(), "Existing");
    }
}
