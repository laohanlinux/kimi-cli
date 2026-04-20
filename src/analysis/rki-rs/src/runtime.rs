//! Dependency container and cross-cutting runtime state.
//!
//! `Runtime` holds all shared state (config, session, approval, wire hub,
//! toolset, background manager, etc.) and is cloned into tools and async tasks.

use crate::agent::{AgentSpec, LaborMarket};
use crate::approval::ApprovalRuntime;
use crate::background::BackgroundTaskManager;
use crate::capability_registry::CapabilityRegistry;
use crate::config::Config;
use crate::feature_flags::{ExperimentalFeature, FeatureFlags};
use crate::hooks::SideEffectEngine;
use crate::identity::IdentityManager;
use crate::injection::InjectionEngine;
use crate::notification::NotificationManager;
use crate::orchestrator::{
    PlanModeOrchestrator, RalphOrchestrator, ReActOrchestrator, TurnOrchestrator,
};
use crate::question::QuestionManager;
use crate::session::Session;
use crate::slash::SlashRegistry;
use crate::soul::denwa_renji::DenwaRenji;
use crate::steer::SteerQueue;
use crate::store::Store;
use crate::stream::SessionStream;
use crate::token::ContextToken;
use crate::toolset::Toolset;
use crate::wire::RootWireHub;
use std::sync::Arc;

/// Resolve the initial orchestrator, applying A/B testing when the feature flag is enabled.
fn resolve_orchestrator<'a>(
    config: &'a Config,
    session_id: &str,
    features: &FeatureFlags,
) -> &'a str {
    use crate::feature_flags::ExperimentalFeature;

    // If explicitly set to something other than react, respect it
    if config.default_orchestrator != "react" {
        return &config.default_orchestrator;
    }

    // A/B test: deterministic split based on session ID hash
    if features.is_enabled(ExperimentalFeature::OrchestratorAbTest) {
        let hash = session_id
            .bytes()
            .fold(0u64, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u64));
        if hash % 2 == 0 { "plan" } else { "react" }
    } else {
        "react"
    }
}

#[derive(Clone)]
pub struct Runtime {
    pub config: Arc<tokio::sync::RwLock<Config>>,
    pub session: Session,
    pub approval: Arc<ApprovalRuntime>,
    pub hub: RootWireHub,
    pub toolset: Arc<tokio::sync::Mutex<Toolset>>,
    pub environment: Environment,
    pub bg_manager: BackgroundTaskManager,
    pub question: Arc<QuestionManager>,
    pub denwa_renji: Arc<DenwaRenji>,
    pub notifications: NotificationManager,
    pub store: Store,
    pub hooks: SideEffectEngine,
    pub identity: Arc<IdentityManager>,
    pub slash_registry: SlashRegistry,
    pub steer_queue: Arc<SteerQueue>,
    pub injection: InjectionEngine,
    pub features: FeatureFlags,
    /// When `KIMI_EXPERIMENTAL_UNIFIED_STREAM` is set, persists hub traffic for replay (§5.2).
    pub session_stream: Option<Arc<SessionStream>>,
    pub capabilities: Option<CapabilityRegistry>,
    /// Builtin subagent type registry (§1.2 LaborMarket).
    pub labor_market: LaborMarket,
    orchestrator: Arc<tokio::sync::RwLock<Arc<dyn TurnOrchestrator>>>,
}

#[derive(Debug, Clone)]
pub struct Environment {
    pub os: String,
    pub shell: String,
    pub cwd: String,
}

impl Runtime {
    pub fn new(
        config: Config,
        session: Session,
        approval: Arc<ApprovalRuntime>,
        hub: RootWireHub,
        store: Store,
    ) -> Self {
        Self::with_features(
            config,
            session,
            approval,
            hub,
            store,
            FeatureFlags::default(),
        )
    }

    pub fn with_features(
        config: Config,
        session: Session,
        approval: Arc<ApprovalRuntime>,
        hub: RootWireHub,
        store: Store,
        features: FeatureFlags,
    ) -> Self {
        let env = Environment {
            os: std::env::consts::OS.to_string(),
            shell: std::env::var("SHELL").unwrap_or_else(|_| "sh".to_string()),
            cwd: std::env::current_dir()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string(),
        };
        let max_bg = if features.is_enabled(ExperimentalFeature::DistributedQueue) {
            8
        } else {
            4
        };
        let bg_manager = if features.is_enabled(ExperimentalFeature::DistributedQueue) {
            // §8.3: per-executor caps (bash-heavy vs in-process agents).
            BackgroundTaskManager::with_executor_caps(session.id.clone(), store.clone(), 6, 2)
        } else {
            BackgroundTaskManager::with_max_running(session.id.clone(), store.clone(), max_bg)
        };
        let question = Arc::new(QuestionManager::new(hub.clone()));
        let denwa_renji = Arc::new(DenwaRenji::new());
        let notifications = NotificationManager::new(session.id.clone(), store.clone());
        let hooks = SideEffectEngine::new();
        let identity = Arc::new(IdentityManager::default_for_kimi().unwrap_or_else(|_| {
            IdentityManager::new(Box::new(crate::identity::EnvCredentialStore::new("")))
        }));
        let slash_registry = SlashRegistry::default();
        let steer_queue = Arc::new(SteerQueue::new());
        let mut injection = InjectionEngine::new();
        injection.register(Arc::new(crate::injection::PlanModeInjectionProvider));
        injection.register(Arc::new(crate::injection::YoloModeInjectionProvider));
        let orchestrator_name = resolve_orchestrator(&config, &session.id, &features);
        let orchestrator: Arc<dyn TurnOrchestrator> = match orchestrator_name {
            "plan" => Arc::new(PlanModeOrchestrator),
            "ralph" => Arc::new(RalphOrchestrator::new(config.ralph_max_iterations.max(1))),
            _ => Arc::new(ReActOrchestrator),
        };
        let session_stream = features
            .is_enabled(ExperimentalFeature::UnifiedStream)
            .then(|| {
                Arc::new(SessionStream::new(
                    session.id.clone(),
                    store.clone(),
                    hub.clone(),
                ))
            });
        let capabilities = if features.is_enabled(ExperimentalFeature::CapabilityServices) {
            let mut reg = CapabilityRegistry::new();
            reg.register(store.clone());
            reg.register(hub.clone());
            reg.register(session.clone());
            reg.register(approval.clone());
            Some(reg)
        } else {
            None
        };
        let mut labor_market = LaborMarket::new();
        let work_dir = session.work_dir.as_path();
        let agent_yaml_candidates = [
            work_dir.join("agent.yaml"),
            work_dir.join("agent.yml"),
            work_dir.join(".kimi").join("agent.yaml"),
            work_dir.join(".kimi").join("agent.yml"),
        ];
        for candidate in &agent_yaml_candidates {
            if candidate.exists() {
                if let Ok(spec) = AgentSpec::from_yaml(candidate) {
                    let spec_dir = candidate.parent().unwrap_or(work_dir);
                    let _ = labor_market.register_subagents_from_spec(&spec, spec_dir);
                }
                break;
            }
        }
        let runtime = Self {
            config: Arc::new(tokio::sync::RwLock::new(config)),
            session,
            approval,
            hub,
            toolset: Arc::new(tokio::sync::Mutex::new(Toolset::new())),
            environment: env,
            bg_manager: bg_manager.clone(),
            question,
            denwa_renji,
            notifications,
            store,
            hooks,
            identity,
            slash_registry,
            steer_queue,
            injection,
            features,
            session_stream,
            capabilities,
            labor_market,
            orchestrator: Arc::new(tokio::sync::RwLock::new(orchestrator)),
        };
        runtime.bg_manager.set_runtime(runtime.clone());
        runtime
    }

    pub async fn get_orchestrator(&self) -> Arc<dyn TurnOrchestrator> {
        self.orchestrator.read().await.clone()
    }

    pub async fn set_orchestrator(&self, orch: Arc<dyn TurnOrchestrator>) {
        let mut lock = self.orchestrator.write().await;
        *lock = orch;
    }

    pub async fn enter_plan_mode(&self) {
        self.set_orchestrator(Arc::new(PlanModeOrchestrator)).await;
    }

    pub async fn exit_plan_mode(&self) {
        self.set_orchestrator(Arc::new(ReActOrchestrator)).await;
    }

    pub async fn enter_ralph_mode(&self, max_iterations: usize) {
        self.set_orchestrator(Arc::new(RalphOrchestrator::new(max_iterations)))
            .await;
    }

    pub async fn is_plan_mode(&self) -> bool {
        self.get_orchestrator().await.name() == "plan"
    }

    pub fn is_yolo(&self) -> bool {
        self.approval.is_yolo()
    }

    pub fn set_yolo(&self, enabled: bool) {
        self.approval.set_yolo(enabled);
    }

    /// Build a turn-scoped token with optional unified `SessionStream` attached (§5.3).
    pub fn context_token_for_turn(&self, turn_id: impl Into<String>) -> ContextToken {
        let mut t = ContextToken::new(self.session.id.clone(), turn_id);
        if let Some(ref stream) = self.session_stream {
            t = t.with_stream(Arc::clone(stream));
        }
        t
    }

    pub async fn reload_config(&self, path: &std::path::Path) -> anyhow::Result<()> {
        let registry = crate::config_registry::parse_config_file(path)?;
        let new_config = registry.to_legacy_config();
        let mut lock = self.config.write().await;
        *lock = new_config;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feature_flags::{ExperimentalFeature, FeatureFlags};
    use std::sync::Arc;

    fn test_runtime() -> Runtime {
        let hub = RootWireHub::new();
        let approval = Arc::new(ApprovalRuntime::new(hub.clone(), true, vec![]));
        let store = Store::open(std::path::Path::new(":memory:")).unwrap();
        Runtime::new(
            Config::default(),
            Session::create(&store, std::env::current_dir().unwrap()).unwrap(),
            approval,
            hub,
            store,
        )
    }

    #[tokio::test]
    async fn test_runtime_creation() {
        let rt = test_runtime();
        assert_eq!(rt.environment.os, std::env::consts::OS);
        assert!(!rt.session.id.is_empty());
        assert_eq!(rt.bg_manager.max_concurrent_tasks(), 4);
    }

    #[tokio::test]
    async fn test_orchestrator_switching() {
        let rt = test_runtime();

        // Default is ReAct
        let orch = rt.get_orchestrator().await;
        assert_eq!(orch.name(), "react");

        rt.enter_plan_mode().await;
        assert!(rt.is_plan_mode().await);

        rt.exit_plan_mode().await;
        assert!(!rt.is_plan_mode().await);

        rt.enter_ralph_mode(5).await;
        let orch = rt.get_orchestrator().await;
        assert_eq!(orch.name(), "ralph");
    }

    #[tokio::test]
    async fn test_orchestrator_selected_from_config() {
        let hub = RootWireHub::new();
        let approval = Arc::new(ApprovalRuntime::new(hub.clone(), true, vec![]));
        let store = Store::open(std::path::Path::new(":memory:")).unwrap();

        let rt = Runtime::new(
            Config {
                default_orchestrator: "plan".to_string(),
                ..Config::default()
            },
            Session::create(&store, std::env::current_dir().unwrap()).unwrap(),
            approval,
            hub,
            store,
        );

        let orch = rt.get_orchestrator().await;
        assert_eq!(orch.name(), "plan");
    }

    #[tokio::test]
    async fn test_reload_config() {
        let rt = test_runtime();

        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join("config.toml");
        std::fs::write(
            &config_path,
            r#"
[models]
default_model = "gpt-4"
"#,
        )
        .unwrap();

        rt.reload_config(&config_path).await.unwrap();
        let cfg = rt.config.read().await;
        assert_eq!(cfg.default_model, "gpt-4");
    }

    #[tokio::test]
    async fn test_runtime_session_dir_exists() {
        let rt = test_runtime();
        assert!(rt.session.dir.exists());
    }

    #[tokio::test]
    async fn test_runtime_environment_fields() {
        let rt = test_runtime();
        assert!(!rt.environment.shell.is_empty());
        assert!(!rt.environment.cwd.is_empty());
    }

    #[tokio::test]
    async fn test_capability_registry_when_feature_enabled() {
        let hub = RootWireHub::new();
        let approval = Arc::new(ApprovalRuntime::new(hub.clone(), true, vec![]));
        let store = Store::open(std::path::Path::new(":memory:")).unwrap();
        let mut features = FeatureFlags::default();
        features.enable(ExperimentalFeature::CapabilityServices);
        let rt = Runtime::with_features(
            Config::default(),
            Session::create(&store, std::env::current_dir().unwrap()).unwrap(),
            approval,
            hub,
            store,
            features,
        );
        assert!(rt.capabilities.is_some());
        let reg = rt.capabilities.unwrap();
        assert!(reg.has::<Store>());
        assert!(reg.has::<RootWireHub>());
    }

    #[tokio::test]
    async fn test_capability_registry_absent_when_feature_disabled() {
        let rt = test_runtime();
        assert!(rt.capabilities.is_none());
    }

    #[tokio::test]
    async fn test_runtime_session_stream_none_without_unified_stream_flag() {
        let rt = test_runtime();
        assert!(rt.session_stream.is_none());
    }

    #[tokio::test]
    async fn test_distributed_queue_raises_background_concurrency_cap() {
        let hub = RootWireHub::new();
        let approval = Arc::new(ApprovalRuntime::new(hub.clone(), true, vec![]));
        let store = Store::open(std::path::Path::new(":memory:")).unwrap();
        let mut features = FeatureFlags::default();
        features.enable(ExperimentalFeature::DistributedQueue);
        let rt = Runtime::with_features(
            Config::default(),
            Session::create(&store, std::env::current_dir().unwrap()).unwrap(),
            approval,
            hub,
            store,
            features,
        );
        assert_eq!(rt.bg_manager.max_concurrent_tasks(), 8);
    }

    #[tokio::test]
    async fn test_context_token_for_turn_attaches_session_stream_when_unified() {
        let hub = RootWireHub::new();
        let approval = Arc::new(ApprovalRuntime::new(hub.clone(), true, vec![]));
        let store = Store::open(std::path::Path::new(":memory:")).unwrap();
        let mut features = FeatureFlags::default();
        features.enable(ExperimentalFeature::UnifiedStream);
        let rt = Runtime::with_features(
            Config::default(),
            Session::create(&store, std::env::current_dir().unwrap()).unwrap(),
            approval,
            hub,
            store,
            features,
        );
        let token = rt.context_token_for_turn("turn-1");
        assert!(token.stream.is_some());
    }

    #[tokio::test]
    async fn test_runtime_loads_labor_market_from_work_dir_agent_yaml() {
        let hub = RootWireHub::new();
        let approval = Arc::new(ApprovalRuntime::new(hub.clone(), true, vec![]));
        let store = Store::open(std::path::Path::new(":memory:")).unwrap();
        let work = tempfile::tempdir().unwrap();
        std::fs::write(
            work.path().join("agent.yaml"),
            b"agent:
  name: root
  subagents:
    helper:
      path: ./helper.yaml
      description: h
",
        )
        .unwrap();
        std::fs::write(
            work.path().join("helper.yaml"),
            b"agent:
  name: helper
  system_prompt_path: ./h.md
",
        )
        .unwrap();
        std::fs::write(work.path().join("h.md"), b"help").unwrap();

        let rt = Runtime::new(
            Config::default(),
            Session::create(&store, work.path().to_path_buf()).unwrap(),
            approval,
            hub,
            store,
        );
        assert_eq!(rt.labor_market.get("helper").unwrap().name, "helper");
    }

    #[tokio::test]
    async fn test_runtime_has_session_stream_when_unified_stream_enabled() {
        let hub = RootWireHub::new();
        let approval = Arc::new(ApprovalRuntime::new(hub.clone(), true, vec![]));
        let store = Store::open(std::path::Path::new(":memory:")).unwrap();
        let mut features = FeatureFlags::default();
        features.enable(ExperimentalFeature::UnifiedStream);
        let rt = Runtime::with_features(
            Config::default(),
            Session::create(&store, std::env::current_dir().unwrap()).unwrap(),
            approval,
            hub.clone(),
            store,
            features,
        );
        let stream = rt.session_stream.as_ref().expect("unified stream");
        rt.hub.broadcast(crate::wire::WireEvent::TurnEnd);
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        let replay = stream.replay(0).unwrap();
        assert!(
            !replay.is_empty(),
            "SessionStream should have persisted the TurnEnd event"
        );
    }
}
