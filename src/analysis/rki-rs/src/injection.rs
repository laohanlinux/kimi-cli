//! Dynamic system-message injection based on runtime state.
//!
//! `InjectionEngine` collects reminders from registered providers.

use crate::message::Message;
use crate::runtime::Runtime;
use async_trait::async_trait;
use std::sync::Arc;

/// Dynamic system reminder injected into context based on runtime state.
#[async_trait]
pub trait InjectionProvider: Send + Sync {
    async fn inject(&self, runtime: &Runtime) -> Vec<Message>;
}

/// Adds a reminder when plan mode is active.
pub struct PlanModeInjectionProvider;

#[async_trait]
impl InjectionProvider for PlanModeInjectionProvider {
    async fn inject(&self, runtime: &Runtime) -> Vec<Message> {
        if runtime.is_plan_mode().await {
            vec![Message::System {
                content: "[PLAN MODE] You are in read-only research mode. Do not use tools. Present a step-by-step plan.".to_string(),
            }]
        } else {
            vec![]
        }
    }
}

/// Adds a reminder when YOLO mode is active.
#[allow(dead_code)]
pub struct YoloModeInjectionProvider;

#[async_trait]
impl InjectionProvider for YoloModeInjectionProvider {
    async fn inject(&self, runtime: &Runtime) -> Vec<Message> {
        if runtime.is_yolo() {
            vec![Message::System {
                content: "[YOLO MODE] Auto-approve is active. All destructive operations will be executed without confirmation. Use with caution.".to_string(),
            }]
        } else {
            vec![]
        }
    }
}

/// Registry of injection providers.
#[derive(Clone)]
pub struct InjectionEngine {
    providers: Vec<Arc<dyn InjectionProvider>>,
}

impl InjectionEngine {
    pub fn new() -> Self {
        Self {
            providers: Vec::new(),
        }
    }

    pub fn register(&mut self, provider: Arc<dyn InjectionProvider>) {
        self.providers.push(provider);
    }

    pub async fn collect(&self, runtime: &Runtime) -> Vec<Message> {
        let mut msgs = Vec::new();
        for p in &self.providers {
            msgs.extend(p.inject(runtime).await);
        }
        msgs
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn test_runtime() -> Runtime {
        let hub = crate::wire::RootWireHub::new();
        let approval = Arc::new(crate::approval::ApprovalRuntime::new(
            hub.clone(),
            true,
            vec![],
        ));
        let store = crate::store::Store::open(std::path::Path::new(":memory:")).unwrap();
        Runtime::new(
            crate::config::Config::default(),
            crate::session::Session::create(&store, std::env::current_dir().unwrap()).unwrap(),
            approval,
            hub,
            store,
        )
    }

    #[tokio::test]
    async fn test_plan_mode_injection() {
        let rt = test_runtime();
        let provider = PlanModeInjectionProvider;

        // Not in plan mode
        assert!(provider.inject(&rt).await.is_empty());

        // Enter plan mode
        rt.enter_plan_mode().await;
        let msgs = provider.inject(&rt).await;
        assert_eq!(msgs.len(), 1);
        assert!(matches!(&msgs[0], Message::System { content } if content.contains("PLAN MODE")));
    }

    #[tokio::test]
    async fn test_injection_engine_collects() {
        let rt = test_runtime();
        let mut engine = InjectionEngine::new();
        engine.register(Arc::new(PlanModeInjectionProvider));

        rt.enter_plan_mode().await;
        let msgs = engine.collect(&rt).await;
        assert_eq!(msgs.len(), 1);
    }

    #[tokio::test]
    async fn test_injection_engine_empty_when_no_providers() {
        let rt = test_runtime();
        let engine = InjectionEngine::new();
        let msgs = engine.collect(&rt).await;
        assert!(msgs.is_empty());
    }

    #[tokio::test]
    async fn test_yolo_injection_active() {
        let rt = test_runtime(); // yolo=true by default
        let provider = YoloModeInjectionProvider;
        let msgs = provider.inject(&rt).await;
        assert_eq!(msgs.len(), 1);
        assert!(matches!(&msgs[0], Message::System { content } if content.contains("YOLO MODE")));
    }

    #[tokio::test]
    async fn test_yolo_injection_inactive() {
        let hub = crate::wire::RootWireHub::new();
        let approval = Arc::new(crate::approval::ApprovalRuntime::new(
            hub.clone(),
            false,
            vec![],
        ));
        let store = crate::store::Store::open(std::path::Path::new(":memory:")).unwrap();
        let rt = Runtime::new(
            crate::config::Config::default(),
            crate::session::Session::create(&store, std::env::current_dir().unwrap()).unwrap(),
            approval,
            hub,
            store,
        );
        let provider = YoloModeInjectionProvider;
        let msgs = provider.inject(&rt).await;
        assert!(msgs.is_empty());
    }

    #[tokio::test]
    async fn test_multiple_providers_aggregate() {
        let rt = test_runtime(); // yolo=true by default
        let mut engine = InjectionEngine::new();
        engine.register(Arc::new(PlanModeInjectionProvider));
        engine.register(Arc::new(YoloModeInjectionProvider));
        rt.enter_plan_mode().await;
        let msgs = engine.collect(&rt).await;
        assert_eq!(msgs.len(), 2);
        assert!(msgs.iter().any(|m| match m {
            Message::System { content } => content.contains("PLAN MODE"),
            _ => false,
        }));
        assert!(msgs.iter().any(|m| match m {
            Message::System { content } => content.contains("YOLO MODE"),
            _ => false,
        }));
    }
}
