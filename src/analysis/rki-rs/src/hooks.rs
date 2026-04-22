//! Structured side-effect hooks (PreToolUse, PostToolUse, Stop).
//!
//! `SideEffectEngine` runs ordered hooks with error boundaries.

use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;

/// Ordered stages for structured side effects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HookStage {
    PreValidate,
    PreExecute,
    PostExecute,
    PostExecuteFailure,
    Audit,
    Stop,
}

/// Decision returned by a side effect.
#[derive(Debug, Clone, PartialEq)]
pub enum EffectDecision {
    Allow,
    Block { reason: String },
}

/// Result of running a side effect.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct SideEffectResult {
    pub decision: EffectDecision,
    pub message: Option<String>,
}

impl SideEffectResult {
    pub fn allow() -> Self {
        Self {
            decision: EffectDecision::Allow,
            message: None,
        }
    }

    pub fn block(reason: impl Into<String>) -> Self {
        Self {
            decision: EffectDecision::Block {
                reason: reason.into(),
            },
            message: None,
        }
    }
}

/// A single side effect (hook) that runs at a specific stage.
#[async_trait]
pub trait SideEffect: Send + Sync {
    fn name(&self) -> &str;
    fn stage(&self) -> HookStage;
    /// If true, failure of this effect stops the turn.
    fn is_critical(&self) -> bool;
    async fn execute(&self, event: &str, payload: &Value) -> anyhow::Result<SideEffectResult>;
}

/// Engine that runs ordered side-effect stages with error boundaries.
#[derive(Clone)]
#[allow(clippy::type_complexity)]
pub struct SideEffectEngine {
    effects:
        std::sync::Arc<std::sync::Mutex<HashMap<HookStage, Vec<std::sync::Arc<dyn SideEffect>>>>>,
}

impl SideEffectEngine {
    pub fn new() -> Self {
        Self {
            effects: std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
        }
    }

    pub fn register(&self, effect: std::sync::Arc<dyn SideEffect>) {
        let mut effects = self.effects.lock().unwrap();
        effects.entry(effect.stage()).or_default().push(effect);
    }

    /// Run all effects for the given stage. Returns Block if ANY effect returns Block.
    /// Critical failures are propagated as errors.
    pub async fn run(
        &self,
        stage: HookStage,
        event: &str,
        payload: &Value,
    ) -> anyhow::Result<SideEffectResult> {
        let effects: Vec<std::sync::Arc<dyn SideEffect>> = {
            let map = self.effects.lock().unwrap();
            map.get(&stage).cloned().unwrap_or_default()
        };
        if effects.is_empty() {
            return Ok(SideEffectResult::allow());
        }

        let mut handles = Vec::new();
        for effect in &effects {
            handles.push(effect.execute(event, payload));
        }

        let results = futures::future::join_all(handles).await;
        let aggregate = SideEffectResult::allow();

        for (effect, result) in effects.iter().zip(results) {
            match result {
                Ok(res) => {
                    if let EffectDecision::Block { reason } = &res.decision {
                        return Ok(SideEffectResult::block(format!(
                            "{} blocked: {}",
                            effect.name(),
                            reason
                        )));
                    }
                }
                Err(e) => {
                    if effect.is_critical() {
                        return Err(anyhow::anyhow!(
                            "Critical side effect {} failed: {}",
                            effect.name(),
                            e
                        ));
                    } else {
                        tracing::warn!("Non-critical side effect {} failed: {}", effect.name(), e);
                    }
                }
            }
        }

        Ok(aggregate)
    }
}

/// Built-in audit logger side effect.
#[allow(dead_code)]
pub struct AuditLogger;

#[async_trait]
impl SideEffect for AuditLogger {
    fn name(&self) -> &str {
        "audit_logger"
    }
    fn stage(&self) -> HookStage {
        HookStage::Audit
    }
    fn is_critical(&self) -> bool {
        false
    }

    async fn execute(&self, event: &str, payload: &Value) -> anyhow::Result<SideEffectResult> {
        tracing::info!(target: "audit", "event={} payload={}", event, payload);
        Ok(SideEffectResult::allow())
    }
}

/// Built-in pre-validate guard that blocks destructive commands.
pub struct DestructiveGuard;

#[async_trait]
impl SideEffect for DestructiveGuard {
    fn name(&self) -> &str {
        "destructive_guard"
    }
    fn stage(&self) -> HookStage {
        HookStage::PreValidate
    }
    fn is_critical(&self) -> bool {
        true
    }

    async fn execute(&self, _event: &str, payload: &Value) -> anyhow::Result<SideEffectResult> {
        if let Some(cmd) = payload.get("command").and_then(|v| v.as_str()) {
            let destructive = ["rm -rf /", "mkfs", "dd if=/dev/zero"];
            for pattern in &destructive {
                if cmd.contains(pattern) {
                    return Ok(SideEffectResult::block(format!(
                        "Destructive command detected: {}",
                        pattern
                    )));
                }
            }
        }
        Ok(SideEffectResult::allow())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_empty_engine_allows() {
        let engine = SideEffectEngine::new();
        let result = engine
            .run(HookStage::PreExecute, "test", &serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(result.decision, EffectDecision::Allow);
    }

    #[tokio::test]
    async fn test_destructive_guard_blocks() {
        let engine = SideEffectEngine::new();
        engine.register(std::sync::Arc::new(DestructiveGuard));
        let result = engine
            .run(
                HookStage::PreValidate,
                "shell",
                &serde_json::json!({"command": "rm -rf /home"}),
            )
            .await
            .unwrap();
        assert!(matches!(result.decision, EffectDecision::Block { .. }));
    }

    #[tokio::test]
    async fn test_destructive_guard_allows_safe() {
        let engine = SideEffectEngine::new();
        engine.register(std::sync::Arc::new(DestructiveGuard));
        let result = engine
            .run(
                HookStage::PreValidate,
                "shell",
                &serde_json::json!({"command": "echo hello"}),
            )
            .await
            .unwrap();
        assert_eq!(result.decision, EffectDecision::Allow);
    }

    #[tokio::test]
    async fn test_multiple_hooks_aggregate_block() {
        let engine = SideEffectEngine::new();
        engine.register(std::sync::Arc::new(DestructiveGuard));
        engine.register(std::sync::Arc::new(DestructiveGuard));
        let result = engine
            .run(
                HookStage::PreValidate,
                "shell",
                &serde_json::json!({"command": "rm -rf /"}),
            )
            .await
            .unwrap();
        assert!(matches!(result.decision, EffectDecision::Block { .. }));
    }

    #[tokio::test]
    async fn test_hook_allows_non_destructive_tools() {
        let engine = SideEffectEngine::new();
        engine.register(std::sync::Arc::new(DestructiveGuard));
        let result = engine
            .run(
                HookStage::PreValidate,
                "read_file",
                &serde_json::json!({"path": "/etc/passwd"}),
            )
            .await
            .unwrap();
        assert_eq!(result.decision, EffectDecision::Allow);
    }

    /// Non-critical hook failure logs a warning and returns Allow.
    struct FailingNonCriticalEffect {
        name: String,
    }

    #[async_trait]
    impl SideEffect for FailingNonCriticalEffect {
        fn name(&self) -> &str {
            &self.name
        }
        fn stage(&self) -> HookStage {
            HookStage::PreValidate
        }
        fn is_critical(&self) -> bool {
            false
        }
        async fn execute(
            &self,
            _event: &str,
            _payload: &serde_json::Value,
        ) -> anyhow::Result<SideEffectResult> {
            anyhow::bail!("non-critical oops")
        }
    }

    #[tokio::test]
    async fn test_non_critical_hook_failure_allows() {
        let engine = SideEffectEngine::new();
        engine.register(std::sync::Arc::new(FailingNonCriticalEffect {
            name: "flaky".into(),
        }));
        let result = engine
            .run(HookStage::PreValidate, "tool_call", &serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(result.decision, EffectDecision::Allow);
    }

    /// First Allow, then Block — Block should win with the blocking hook's name in the reason.
    struct NamedBlockEffect {
        name: String,
        reason: String,
    }

    #[async_trait]
    impl SideEffect for NamedBlockEffect {
        fn name(&self) -> &str {
            &self.name
        }
        fn stage(&self) -> HookStage {
            HookStage::PreValidate
        }
        fn is_critical(&self) -> bool {
            false
        }
        async fn execute(
            &self,
            _event: &str,
            _payload: &serde_json::Value,
        ) -> anyhow::Result<SideEffectResult> {
            Ok(SideEffectResult::block(&self.reason))
        }
    }

    #[tokio::test]
    async fn test_mixed_allow_block_first_block_wins() {
        let engine = SideEffectEngine::new();
        engine.register(std::sync::Arc::new(FailingNonCriticalEffect {
            name: "allower".into(),
        })); // returns Allow (via Err, which for non-critical becomes Allow)
        engine.register(std::sync::Arc::new(NamedBlockEffect {
            name: "blocker".into(),
            reason: "no way".into(),
        }));
        let result = engine
            .run(HookStage::PreValidate, "tool_call", &serde_json::json!({}))
            .await
            .unwrap();
        assert!(matches!(result.decision, EffectDecision::Block { .. }));
        let reason = match result.decision {
            EffectDecision::Block { reason } => reason,
            _ => panic!("expected Block"),
        };
        assert!(reason.contains("blocker"), "reason should name blocking hook: {}", reason);
        assert!(reason.contains("no way"), "reason should contain hook reason: {}", reason);
    }
}
