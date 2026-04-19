use crate::agent::{Agent, AgentSpec};
use crate::approval::ApprovalRuntime;
use crate::context::Context;
use crate::feature_flags::ExperimentalFeature;
use crate::llm::EchoProvider;
use crate::runtime::Runtime;
use crate::subagents::store::SubagentStore;
use crate::tools::{ReadFileTool, ShellTool, WriteFileTool};
use crate::wire::{EventSource, RootWireHub};
use std::sync::Arc;
use tokio::sync::Mutex;

pub struct ForegroundSubagentRunner;

impl ForegroundSubagentRunner {
    pub async fn run(
        parent_runtime: &Runtime,
        parent_hub: &RootWireHub,
        parent_tool_call_id: String,
        agent_spec: AgentSpec,
        prompt: String,
    ) -> anyhow::Result<String> {
        let agent_id = uuid::Uuid::new_v4().to_string();
        let store = SubagentStore::new(parent_runtime.store.clone());
        store
            .create(&agent_id, &parent_runtime.session.id, &agent_spec.system_prompt, &prompt)
            .await?;

        let hub = RootWireHub::new();
        let approval = Arc::new(ApprovalRuntime::new(hub.clone(), false, vec![]));
        let config = {
            let cfg = parent_runtime.config.read().await;
            cfg.clone()
        };
        let subagent_runtime = Runtime::new(
            config,
            parent_runtime.session.clone(),
            approval,
            hub,
            parent_runtime.store.clone(),
        );

        {
            let mut ts = subagent_runtime.toolset.lock().await;
            ts.register(Box::new(ShellTool));
            ts.register(Box::new(ReadFileTool));
            ts.register(Box::new(WriteFileTool));
        }

        let context = Arc::new(Mutex::new(
            Context::load(&subagent_runtime.store, &subagent_runtime.session.id).await?
        ));
        let agent = Agent {
            spec: agent_spec.clone(),
            system_prompt: agent_spec.system_prompt,
        };
        let llm: Arc<dyn crate::llm::ChatProvider> = Arc::new(EchoProvider);
        let soul = crate::soul::KimiSoul::new(agent, context, llm, subagent_runtime.clone());

        // One hub for the subagent soul and the forwarder (fixes accidental second hub).
        let hub = subagent_runtime.hub.clone();
        let mut rx = hub.subscribe();
        let stamp_subagent = parent_runtime
            .features
            .is_enabled(ExperimentalFeature::SubagentEventSource);
        let persist_subagent_wire = parent_runtime
            .features
            .is_enabled(ExperimentalFeature::SubagentWirePersistence);

        let parent_tool_call_id_fwd = parent_tool_call_id.clone();
        let parent_hub_fwd = parent_hub.clone();
        let agent_id_fwd = agent_id.clone();
        let sa_store_fwd = store.clone();
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let forward_task = tokio::spawn(async move {
            use tokio::sync::broadcast::error::RecvError;
            loop {
                tokio::select! {
                    recv = rx.recv() => {
                        match recv {
                            Ok(envelope) => {
                                let envelope = if stamp_subagent {
                                    envelope.with_source(EventSource::subagent(
                                        agent_id_fwd.clone(),
                                        Some(parent_tool_call_id_fwd.clone()),
                                    ))
                                } else {
                                    envelope
                                };
                                if persist_subagent_wire {
                                    if let Err(e) = sa_store_fwd
                                        .append_wire_envelope(&agent_id_fwd, &envelope)
                                        .await
                                    {
                                        tracing::warn!(
                                            error = %e,
                                            "SubagentStore::append_wire_envelope failed"
                                        );
                                    }
                                }
                                parent_hub_fwd.broadcast_envelope(envelope);
                            }
                            Err(RecvError::Lagged(_)) => continue,
                            Err(RecvError::Closed) => break,
                        }
                    }
                    _ = &mut shutdown_rx => {
                        while let Ok(envelope) = rx.try_recv() {
                            let envelope = if stamp_subagent {
                                envelope.with_source(EventSource::subagent(
                                    agent_id_fwd.clone(),
                                    Some(parent_tool_call_id_fwd.clone()),
                                ))
                            } else {
                                envelope
                            };
                            if persist_subagent_wire {
                                if let Err(e) = sa_store_fwd
                                    .append_wire_envelope(&agent_id_fwd, &envelope)
                                    .await
                                {
                                    tracing::warn!(
                                        error = %e,
                                        "SubagentStore::append_wire_envelope failed (drain)"
                                    );
                                }
                            }
                            parent_hub_fwd.broadcast_envelope(envelope);
                        }
                        break;
                    }
                }
            }
        });

        let result = soul.run(&prompt, &hub).await;
        let _ = shutdown_tx.send(());
        let _ = forward_task.await;

        match result {
            Ok(_) => Ok(format!("Subagent {} completed", agent_id)),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval::ApprovalRuntime;
    use crate::config::Config;
    use crate::feature_flags::{ExperimentalFeature, FeatureFlags};
    use crate::session::Session;
    use crate::store::Store;
    use crate::wire::{RootWireHub, SourceType};
    use std::sync::Arc;

    fn test_runtime() -> Runtime {
        test_runtime_with_features(FeatureFlags::default())
    }

    fn test_runtime_with_features(features: FeatureFlags) -> Runtime {
        let hub = RootWireHub::new();
        let approval = Arc::new(ApprovalRuntime::new(hub.clone(), true, vec![]));
        let store = Store::open(std::path::Path::new(":memory:")).unwrap();
        Runtime::with_features(
            Config::default(),
            Session::create(&store, std::env::current_dir().unwrap()).unwrap(),
            approval,
            hub,
            store,
            features,
        )
    }

    #[tokio::test]
    async fn test_subagent_runner_completes() {
        let rt = test_runtime();
        let parent_hub = RootWireHub::new();
        let spec = AgentSpec {
            name: "test".to_string(),
            system_prompt: "You are a test subagent.".to_string(),
            tools: vec![],
            capabilities: vec![],
            ..Default::default()
        };
        let result = ForegroundSubagentRunner::run(&rt, &parent_hub, "tc-1".to_string(), spec, "hello".to_string()).await;
        assert!(result.is_ok());
        assert!(result.unwrap().contains("completed"));
    }

    #[tokio::test]
    async fn test_subagent_wire_persistence_writes_parent_wire_events() {
        let mut features = FeatureFlags::default();
        features.enable(ExperimentalFeature::SubagentWirePersistence);
        let rt = test_runtime_with_features(features);
        let parent_hub = RootWireHub::new();
        let spec = AgentSpec {
            name: "test".to_string(),
            system_prompt: "You are a test subagent.".to_string(),
            tools: vec![],
            capabilities: vec![],
            ..Default::default()
        };
        let sid = rt.session.id.clone();
        ForegroundSubagentRunner::run(&rt, &parent_hub, "tc-persist".to_string(), spec, "hi".to_string())
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        let rows = rt.store.get_wire_events(&sid).unwrap();
        assert!(
            !rows.is_empty(),
            "expected wire_events on parent session when SubagentWirePersistence is on"
        );
    }

    #[tokio::test]
    async fn test_subagent_wire_persistence_stores_wire_envelope_with_source() {
        use crate::wire::WireEnvelope;
        let mut features = FeatureFlags::default();
        features.enable(ExperimentalFeature::SubagentWirePersistence);
        features.enable(ExperimentalFeature::SubagentEventSource);
        let rt = test_runtime_with_features(features);
        let parent_hub = RootWireHub::new();
        let spec = AgentSpec {
            name: "test".to_string(),
            system_prompt: "You are a test subagent.".to_string(),
            tools: vec![],
            capabilities: vec![],
            ..Default::default()
        };
        let sid = rt.session.id.clone();
        ForegroundSubagentRunner::run(&rt, &parent_hub, "tc-env-src".to_string(), spec, "persist-env".to_string())
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let rows = rt.store.get_wire_events(&sid).unwrap();
        let mut saw_subagent_envelope = false;
        for (_, _etype, payload) in rows {
            if let Ok(env) = serde_json::from_str::<WireEnvelope>(&payload) {
                if matches!(env.source.source_type, SourceType::Subagent) {
                    saw_subagent_envelope = true;
                    assert!(!env.event_id.is_empty());
                    break;
                }
            }
        }
        assert!(
            saw_subagent_envelope,
            "expected persisted JSON to be a WireEnvelope with Subagent EventSource"
        );
    }

    #[tokio::test]
    async fn test_subagent_runner_empty_prompt() {
        let rt = test_runtime();
        let parent_hub = RootWireHub::new();
        let spec = AgentSpec {
            name: "empty".to_string(),
            system_prompt: "You are a test subagent.".to_string(),
            tools: vec![],
            capabilities: vec![],
            ..Default::default()
        };
        let result = ForegroundSubagentRunner::run(&rt, &parent_hub, "tc-2".to_string(), spec, "".to_string()).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_subagent_runner_different_spec_name() {
        let rt = test_runtime();
        let parent_hub = RootWireHub::new();
        let spec = AgentSpec {
            name: "researcher".to_string(),
            system_prompt: "You are a researcher.".to_string(),
            tools: vec![],
            capabilities: vec![],
            ..Default::default()
        };
        let result = ForegroundSubagentRunner::run(&rt, &parent_hub, "tc-3".to_string(), spec, "find docs".to_string()).await;
        assert!(result.is_ok());
        assert!(result.unwrap().contains("completed"));
    }

    #[tokio::test]
    async fn test_subagent_parent_sees_subagent_source_when_flag_on() {
        let mut features = FeatureFlags::default();
        features.enable(ExperimentalFeature::SubagentEventSource);
        let rt = test_runtime_with_features(features);
        let parent_hub = RootWireHub::new();
        let mut rx = parent_hub.subscribe();
        let spec = AgentSpec {
            name: "test".to_string(),
            system_prompt: "You are a test subagent.".to_string(),
            tools: vec![],
            capabilities: vec![],
            ..Default::default()
        };
        ForegroundSubagentRunner::run(&rt, &parent_hub, "tc-src-on".to_string(), spec, "hello".to_string())
            .await
            .unwrap();
        let mut saw_subagent = false;
        while let Ok(env) = rx.try_recv() {
            if env.source.source_type == SourceType::Subagent {
                saw_subagent = true;
                break;
            }
        }
        assert!(saw_subagent, "expected at least one Subagent-stamped envelope");
    }

    #[tokio::test]
    async fn test_subagent_parent_sees_root_only_when_flag_off() {
        let rt = test_runtime(); // SubagentEventSource off by default
        let parent_hub = RootWireHub::new();
        let mut rx = parent_hub.subscribe();
        let spec = AgentSpec {
            name: "test".to_string(),
            system_prompt: "You are a test subagent.".to_string(),
            tools: vec![],
            capabilities: vec![],
            ..Default::default()
        };
        ForegroundSubagentRunner::run(&rt, &parent_hub, "tc-src-off".to_string(), spec, "hello".to_string())
            .await
            .unwrap();
        let mut n = 0usize;
        while let Ok(env) = rx.try_recv() {
            n += 1;
            assert_ne!(
                env.source.source_type,
                SourceType::Subagent,
                "forwarded subagent wire should stay Root when SubagentEventSource is off"
            );
        }
        assert!(n > 0, "expected forwarded wire events on parent hub");
    }
}
