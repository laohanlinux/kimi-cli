use crate::store::Store;

#[derive(Clone)]
pub struct SubagentStore {
    store: Store,
}

impl SubagentStore {
    pub fn new(store: Store) -> Self {
        Self { store }
    }

    pub async fn create(
        &self,
        id: &str,
        session_id: &str,
        system_prompt: &str,
        prompt: &str,
    ) -> anyhow::Result<()> {
        self.store.create_subagent(crate::store::CreateSubagentParams {
            id,
            session_id,
            parent_tool_call_id: None,
            agent_type: Some("subagent"),
            system_prompt: Some(system_prompt),
            prompt: Some(prompt),
            parent_session_id: Some(session_id),
        })?;
        Ok(())
    }

    pub async fn append_wire(
        &self,
        agent_id: &str,
        event: &crate::wire::WireEvent,
    ) -> anyhow::Result<()> {
        let envelope = crate::wire::WireEnvelope::new(event.clone());
        self.append_wire_envelope(agent_id, &envelope).await
    }

    /// Persist a full [`crate::wire::WireEnvelope`] (§6.5): replay keeps `EventSource` / `event_id` / timestamps.
    pub async fn append_wire_envelope(
        &self,
        agent_id: &str,
        envelope: &crate::wire::WireEnvelope,
    ) -> anyhow::Result<()> {
        let Some((session_id, _, _, _, _)) = self.store.get_subagent(agent_id)? else {
            tracing::debug!("append_wire_envelope: unknown subagent {}", agent_id);
            return Ok(());
        };
        let payload = serde_json::to_string(envelope)?;
        let event_type = envelope.event.serde_type_key();
        self.store
            .append_wire_event(&session_id, &event_type, &payload)?;
        Ok(())
    }

    pub async fn read_output(&self, _agent_id: &str) -> anyhow::Result<String> {
        // Subagent output is forwarded to parent wire; persisted there.
        Ok(String::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_subagent_store_create_and_persist() {
        let store = Store::open(std::path::Path::new(":memory:")).unwrap();
        store.create_session("s1", "/tmp").unwrap();

        let sa_store = SubagentStore::new(store.clone());
        sa_store
            .create("sa1", "s1", "You are a helper.", "Do thing")
            .await
            .unwrap();

        let sa = store.get_subagent("sa1").unwrap().unwrap();
        assert_eq!(sa.0, "s1");
        assert_eq!(sa.3.as_deref(), Some("You are a helper."));
        assert_eq!(sa.4.as_deref(), Some("s1"));
    }

    #[tokio::test]
    async fn test_append_wire_unknown_agent_is_ok() {
        let store = Store::open(std::path::Path::new(":memory:")).unwrap();
        let sa_store = SubagentStore::new(store);
        sa_store
            .append_wire("missing", &crate::wire::WireEvent::TurnEnd)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_read_output_empty() {
        let store = Store::open(std::path::Path::new(":memory:")).unwrap();
        let sa_store = SubagentStore::new(store);
        let out = sa_store.read_output("sa1").await.unwrap();
        assert_eq!(out, "");
    }

    #[tokio::test]
    async fn test_append_wire_persists_to_parent_session() {
        let store = Store::open(std::path::Path::new(":memory:")).unwrap();
        store.create_session("s1", "/tmp").unwrap();
        let sa_store = SubagentStore::new(store.clone());
        sa_store.create("sa1", "s1", "sys", "go").await.unwrap();
        sa_store
            .append_wire(
                "sa1",
                &crate::wire::WireEvent::TextPart {
                    text: "hello".to_string(),
                },
            )
            .await
            .unwrap();
        sa_store
            .append_wire("sa1", &crate::wire::WireEvent::TurnEnd)
            .await
            .unwrap();
        let wire = store.get_wire_events("s1").unwrap();
        assert_eq!(wire.len(), 2);
        assert_eq!(wire[0].1, "text_part");
        assert_eq!(wire[1].1, "turn_end");
        let env0: crate::wire::WireEnvelope = serde_json::from_str(&wire[0].2).unwrap();
        assert!(matches!(
            env0.event,
            crate::wire::WireEvent::TextPart { .. }
        ));
        assert_eq!(env0.source.source_type, crate::wire::SourceType::Root);
    }

    #[tokio::test]
    async fn test_create_multiple_subagents() {
        let store = Store::open(std::path::Path::new(":memory:")).unwrap();
        store.create_session("s1", "/tmp").unwrap();
        let sa_store = SubagentStore::new(store.clone());

        sa_store
            .create("sa1", "s1", "sys1", "prompt1")
            .await
            .unwrap();
        sa_store
            .create("sa2", "s1", "sys2", "prompt2")
            .await
            .unwrap();

        let sa1 = store.get_subagent("sa1").unwrap().unwrap();
        let sa2 = store.get_subagent("sa2").unwrap().unwrap();
        assert_eq!(sa1.3.as_deref(), Some("sys1"));
        assert_eq!(sa2.3.as_deref(), Some("sys2"));
    }
}
