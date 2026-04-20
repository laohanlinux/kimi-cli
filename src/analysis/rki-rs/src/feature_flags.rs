//! Feature flags for experimental deviations (§9.3 risk mitigation).
//!
//! Flags are read from `KIMI_EXPERIMENTAL_*` environment variables at startup.
//! Each deviation can be enabled independently for canary testing.

use std::collections::HashSet;

/// Known experimental features.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExperimentalFeature {
    /// Capability-based service decomposition (§5.1)
    CapabilityServices,
    /// Unified `SessionStream`: persist hub events to SQLite when enabled (`KIMI_EXPERIMENTAL_UNIFIED_STREAM`, §5.2).
    UnifiedStream,
    /// Cooperative backpressure while streaming LLM chunks (`KIMI_EXPERIMENTAL_PULL_GENERATION`, §6.1).
    PullGeneration,
    /// Structured side-effect engine: `PreExecute` hook `Block` is honored (stops tool before `handle`), §6.2 + §9.3.
    StructuredEffects,
    /// When set, subagent→parent forwarding rewrites `EventSource` to `subagent` (§6.5). When unset, envelopes pass through as emitted by the subagent hub.
    SubagentEventSource,
    /// Persist forwarded subagent `WireEvent`s to the parent session `wire_events` table (§6.5 event sourcing).
    SubagentWirePersistence,
    /// Plugin registry with dynamic manifests (§7.1)
    PluginRegistry,
    /// Stateless function tools (§7.2): tags each tool JSON schema with `x-rki-tool-contract` for providers that support function-style tools.
    FunctionTools,
    /// Native MCP integration (§7.5): forward MCP progress and resource updates to the hub; route sampling to the LLM.
    /// MCP tools are still registered from `mcp.json` when this is off; only the background bridges are skipped.
    NativeMcp,
    /// Hierarchical memory: when enabled, `ReActOrchestrator` prepends `history_with_recall` to the LLM (§8.5 + §9.3).
    MemoryHierarchy,
    /// When set with `MemoryHierarchy`, CLI attaches an embedding provider: `KIMI_EMBEDDING_URL` → HTTP (else hash) (§8.5).
    SemanticEmbeddings,
    /// Distributed task queue (§8.3): durable `recover()`, bash **auto-resubmit** on non-zero exit when `TaskSpec.max_retries > 0`,
    /// and **per-executor** background caps (6 bash / 2 agent) instead of a single global pool.
    DistributedQueue,
    /// A/B testing per-session orchestrator selection
    OrchestratorAbTest,
    /// Hot-reload config watcher
    ConfigHotReload,
}

impl ExperimentalFeature {
    fn env_name(&self) -> &'static str {
        match self {
            ExperimentalFeature::CapabilityServices => "KIMI_EXPERIMENTAL_CAPABILITY_SERVICES",
            ExperimentalFeature::UnifiedStream => "KIMI_EXPERIMENTAL_UNIFIED_STREAM",
            ExperimentalFeature::PullGeneration => "KIMI_EXPERIMENTAL_PULL_GENERATION",
            ExperimentalFeature::StructuredEffects => "KIMI_EXPERIMENTAL_STRUCTURED_EFFECTS",
            ExperimentalFeature::SubagentEventSource => "KIMI_EXPERIMENTAL_SUBAGENT_EVENT_SOURCE",
            ExperimentalFeature::SubagentWirePersistence => {
                "KIMI_EXPERIMENTAL_SUBAGENT_WIRE_PERSISTENCE"
            }
            ExperimentalFeature::PluginRegistry => "KIMI_EXPERIMENTAL_PLUGIN_REGISTRY",
            ExperimentalFeature::FunctionTools => "KIMI_EXPERIMENTAL_FUNCTION_TOOLS",
            ExperimentalFeature::NativeMcp => "KIMI_EXPERIMENTAL_NATIVE_MCP",
            ExperimentalFeature::MemoryHierarchy => "KIMI_EXPERIMENTAL_MEMORY_HIERARCHY",
            ExperimentalFeature::SemanticEmbeddings => "KIMI_EXPERIMENTAL_SEMANTIC_EMBEDDINGS",
            ExperimentalFeature::DistributedQueue => "KIMI_EXPERIMENTAL_DISTRIBUTED_QUEUE",
            ExperimentalFeature::OrchestratorAbTest => "KIMI_EXPERIMENTAL_ORCHESTRATOR_AB_TEST",
            ExperimentalFeature::ConfigHotReload => "KIMI_EXPERIMENTAL_CONFIG_HOT_RELOAD",
        }
    }
}

/// Runtime feature flag state.
#[derive(Debug, Clone, Default)]
pub struct FeatureFlags {
    enabled: HashSet<ExperimentalFeature>,
}

impl FeatureFlags {
    /// Load all feature flags from the environment.
    pub fn from_env() -> Self {
        let mut enabled = HashSet::new();
        for feature in [
            ExperimentalFeature::CapabilityServices,
            ExperimentalFeature::UnifiedStream,
            ExperimentalFeature::PullGeneration,
            ExperimentalFeature::StructuredEffects,
            ExperimentalFeature::SubagentEventSource,
            ExperimentalFeature::SubagentWirePersistence,
            ExperimentalFeature::PluginRegistry,
            ExperimentalFeature::FunctionTools,
            ExperimentalFeature::NativeMcp,
            ExperimentalFeature::MemoryHierarchy,
            ExperimentalFeature::SemanticEmbeddings,
            ExperimentalFeature::DistributedQueue,
            ExperimentalFeature::OrchestratorAbTest,
            ExperimentalFeature::ConfigHotReload,
        ] {
            if let Ok(val) = std::env::var(feature.env_name())
                && is_truthy(&val)
            {
                enabled.insert(feature);
            }
        }
        Self { enabled }
    }

    /// Check if a feature is enabled.
    pub fn is_enabled(&self, feature: ExperimentalFeature) -> bool {
        self.enabled.contains(&feature)
    }

    /// Enable a feature (for testing).
    pub fn enable(&mut self, feature: ExperimentalFeature) {
        self.enabled.insert(feature);
    }

    /// Disable a feature.
    pub fn disable(&mut self, feature: ExperimentalFeature) {
        self.enabled.remove(&feature);
    }

    /// Total number of enabled features.
    pub fn count_enabled(&self) -> usize {
        self.enabled.len()
    }
}

fn is_truthy(s: &str) -> bool {
    matches!(
        s.trim().to_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_feature_flags_empty_by_default() {
        let flags = FeatureFlags::default();
        assert!(!flags.is_enabled(ExperimentalFeature::PullGeneration));
        assert_eq!(flags.count_enabled(), 0);
    }

    #[test]
    fn test_feature_flags_enable_disable() {
        let mut flags = FeatureFlags::default();
        flags.enable(ExperimentalFeature::PullGeneration);
        assert!(flags.is_enabled(ExperimentalFeature::PullGeneration));
        assert_eq!(flags.count_enabled(), 1);

        flags.disable(ExperimentalFeature::PullGeneration);
        assert!(!flags.is_enabled(ExperimentalFeature::PullGeneration));
    }

    #[test]
    fn test_is_truthy() {
        assert!(is_truthy("1"));
        assert!(is_truthy("true"));
        assert!(is_truthy("TRUE"));
        assert!(is_truthy("yes"));
        assert!(is_truthy("on"));
        assert!(!is_truthy("0"));
        assert!(!is_truthy("false"));
        assert!(!is_truthy("no"));
        assert!(!is_truthy(""));
    }

    #[test]
    fn test_feature_flags_from_env() {
        // Set a feature flag
        unsafe {
            std::env::set_var("KIMI_EXPERIMENTAL_PULL_GENERATION", "1");
        }
        let flags = FeatureFlags::from_env();
        assert!(flags.is_enabled(ExperimentalFeature::PullGeneration));

        // Clean up
        unsafe {
            std::env::remove_var("KIMI_EXPERIMENTAL_PULL_GENERATION");
        }
    }
}
