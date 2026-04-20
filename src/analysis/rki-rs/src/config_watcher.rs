//! Hot-reloading config watcher with selective subscriber propagation.
//!
//! `ConfigWatcher` watches the config file and notifies per-section subscribers
//! only when their section actually changed.

use notify::{Config as NotifyConfig, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::config::Config;

/// Subscriber notified when a specific config section changes.
pub trait ConfigSubscriber: Send + Sync {
    fn on_change(&self, section: &str, old: &Config, new: &Config);
}

impl<F> ConfigSubscriber for F
where
    F: Fn(&str, &Config, &Config) + Send + Sync,
{
    fn on_change(&self, section: &str, old: &Config, new: &Config) {
        self(section, old, new);
    }
}

impl<T: ConfigSubscriber> ConfigSubscriber for Arc<T> {
    fn on_change(&self, section: &str, old: &Config, new: &Config) {
        (**self).on_change(section, old, new);
    }
}

/// Propagates config changes to registered subscribers per section (§8.7 deviation).
/// Only notifies subscribers whose section actually changed.
pub struct ConfigChangePropagator {
    path: std::path::PathBuf,
    subscribers: Mutex<HashMap<String, Vec<Box<dyn ConfigSubscriber>>>>,
    last_config: Mutex<Option<Config>>,
}

impl ConfigChangePropagator {
    pub fn new(path: std::path::PathBuf, initial: Option<Config>) -> Arc<Self> {
        Arc::new(Self {
            path,
            subscribers: Mutex::new(HashMap::new()),
            last_config: Mutex::new(initial),
        })
    }

    /// Register a subscriber for a specific config section.
    /// Sections: `model`, `loop_control`, `trust_profile`, `orchestrator`, `providers`, `compaction`, `mcp` (§8.7).
    pub fn subscribe(&self, section: &str, subscriber: Box<dyn ConfigSubscriber>) {
        let mut subs = self.subscribers.lock().unwrap();
        subs.entry(section.to_string())
            .or_default()
            .push(subscriber);
    }

    /// Load current config, diff against last known, and notify subscribers.
    pub fn check_and_notify(&self) -> anyhow::Result<()> {
        let registry = crate::config_registry::parse_config_file(&self.path)?;
        let new_config = registry.to_legacy_config();
        let mut last_lock = self.last_config.lock().unwrap();

        if let Some(ref old) = *last_lock {
            let changed = Self::diff_sections(old, &new_config);
            if !changed.is_empty() {
                let subs = self.subscribers.lock().unwrap();
                for section in changed {
                    if let Some(subscribers) = subs.get(&section) {
                        for sub in subscribers {
                            sub.on_change(&section, old, &new_config);
                        }
                    }
                }
            }
        }

        *last_lock = Some(new_config);
        Ok(())
    }

    /// Determine which config sections changed between old and new.
    fn diff_sections(old: &Config, new: &Config) -> Vec<String> {
        let mut changed = BTreeSet::new();

        if old.default_model != new.default_model
            || old.supports_vision != new.supports_vision
            || old.ignore_vision_model_hint != new.ignore_vision_model_hint
            || old.vision_by_model != new.vision_by_model
        {
            changed.insert("model".to_string());
        }
        if old.max_steps_per_turn != new.max_steps_per_turn
            || old.max_context_size != new.max_context_size
        {
            changed.insert("loop_control".to_string());
        }
        if old.trust_profile != new.trust_profile {
            changed.insert("trust_profile".to_string());
        }
        if old.default_orchestrator != new.default_orchestrator {
            changed.insert("orchestrator".to_string());
        }
        if old.ralph_max_iterations != new.ralph_max_iterations {
            changed.insert("orchestrator".to_string());
        }
        if (old.compaction_threshold_percent - new.compaction_threshold_percent).abs()
            > f64::EPSILON
            || old.compaction_threshold_absolute != new.compaction_threshold_absolute
            || old.compaction_min_messages != new.compaction_min_messages
        {
            changed.insert("compaction".to_string());
        }
        // models and providers are nested JSON; compare serialized form
        let old_models = serde_json::to_string(&old.models).unwrap_or_default();
        let new_models = serde_json::to_string(&new.models).unwrap_or_default();
        if old_models != new_models {
            changed.insert("model".to_string());
        }
        let old_providers = serde_json::to_string(&old.providers).unwrap_or_default();
        let new_providers = serde_json::to_string(&new.providers).unwrap_or_default();
        if old_providers != new_providers {
            changed.insert("providers".to_string());
        }
        if old.mcp_fingerprint != new.mcp_fingerprint {
            changed.insert("mcp".to_string());
        }

        changed.into_iter().collect()
    }
}

/// Watches a config file and propagates changes to subscribers.
pub struct ConfigWatcher {
    _watcher: RecommendedWatcher,
}

impl ConfigWatcher {
    pub fn new(path: &Path, propagator: Arc<ConfigChangePropagator>) -> anyhow::Result<Self> {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut watcher = RecommendedWatcher::new(tx, NotifyConfig::default())?;
        watcher.watch(path, RecursiveMode::NonRecursive)?;

        std::thread::spawn(move || {
            for res in rx {
                if let Ok(event) = res
                    && matches!(
                        event.kind,
                        notify::EventKind::Modify(_) | notify::EventKind::Create(_)
                    )
                    && let Err(e) = propagator.check_and_notify()
                {
                    tracing::warn!("Config change propagation failed: {}", e);
                }
            }
        });
        Ok(Self { _watcher: watcher })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[derive(Debug)]
    struct MockSubscriber {
        triggered: AtomicBool,
    }

    impl MockSubscriber {
        fn new() -> Self {
            Self {
                triggered: AtomicBool::new(false),
            }
        }
        fn was_triggered(&self) -> bool {
            self.triggered.load(Ordering::SeqCst)
        }
    }

    impl ConfigSubscriber for MockSubscriber {
        fn on_change(&self, _section: &str, _old: &Config, _new: &Config) {
            self.triggered.store(true, Ordering::SeqCst);
        }
    }

    #[test]
    fn test_diff_sections_detects_model_change() {
        let old = Config {
            default_model: "gpt-4".to_string(),
            max_steps_per_turn: Some(10),
            max_context_size: Some(128_000),
            ..Config::default()
        };
        let new = Config {
            default_model: "claude".to_string(),
            max_steps_per_turn: Some(10),
            max_context_size: Some(128_000),
            ..Config::default()
        };
        let changed = ConfigChangePropagator::diff_sections(&old, &new);
        assert!(changed.contains(&"model".to_string()));
        assert!(!changed.contains(&"loop_control".to_string()));
    }

    #[test]
    fn test_diff_sections_detects_supports_vision_change() {
        let mut old = Config {
            default_model: "gpt-4".to_string(),
            max_steps_per_turn: Some(10),
            max_context_size: Some(128_000),
            ..Config::default()
        };
        old.supports_vision = true;
        let mut new = old.clone();
        new.supports_vision = false;
        let changed = ConfigChangePropagator::diff_sections(&old, &new);
        assert!(changed.contains(&"model".to_string()));
    }

    #[test]
    fn test_diff_sections_detects_ignore_vision_hint_change() {
        let base = Config {
            default_model: "echo".to_string(),
            max_steps_per_turn: Some(10),
            max_context_size: Some(128_000),
            ..Config::default()
        };
        let mut a = base.clone();
        a.ignore_vision_model_hint = false;
        let mut b = base.clone();
        b.ignore_vision_model_hint = true;
        let changed = ConfigChangePropagator::diff_sections(&a, &b);
        assert!(changed.contains(&"model".to_string()));
    }

    #[test]
    fn test_diff_sections_detects_vision_by_model_change() {
        let base = Config {
            default_model: "echo".to_string(),
            max_steps_per_turn: Some(10),
            max_context_size: Some(128_000),
            ..Config::default()
        };
        let mut a = base.clone();
        a.vision_by_model.insert("echo".to_string(), false);
        let mut b = base.clone();
        b.vision_by_model.insert("echo".to_string(), true);
        let changed = ConfigChangePropagator::diff_sections(&a, &b);
        assert!(changed.contains(&"model".to_string()));
    }

    #[test]
    fn test_diff_sections_detects_loop_control_change() {
        let old = Config {
            default_model: "gpt-4".to_string(),
            max_steps_per_turn: Some(10),
            max_context_size: Some(128_000),
            ..Config::default()
        };
        let new = Config {
            default_model: "gpt-4".to_string(),
            max_steps_per_turn: Some(50),
            max_context_size: Some(256_000),
            ..Config::default()
        };
        let changed = ConfigChangePropagator::diff_sections(&old, &new);
        assert!(changed.contains(&"loop_control".to_string()));
        assert!(!changed.contains(&"model".to_string()));
    }

    #[test]
    fn test_diff_sections_no_change() {
        let config = Config {
            default_model: "gpt-4".to_string(),
            max_steps_per_turn: Some(10),
            max_context_size: Some(128_000),
            ..Config::default()
        };
        let changed = ConfigChangePropagator::diff_sections(&config, &config);
        assert!(changed.is_empty());
    }

    #[test]
    fn test_diff_sections_detects_compaction_change() {
        let old = Config {
            default_model: "gpt-4".to_string(),
            max_steps_per_turn: Some(10),
            max_context_size: Some(128_000),
            compaction_threshold_percent: 0.85,
            compaction_threshold_absolute: 50_000,
            compaction_min_messages: 4,
            ..Config::default()
        };
        let mut new = old.clone();
        new.compaction_min_messages = 8;
        let changed = ConfigChangePropagator::diff_sections(&old, &new);
        assert!(changed.contains(&"compaction".to_string()));
        assert!(!changed.contains(&"model".to_string()));
    }

    #[test]
    fn test_propagator_notifies_subscriber() {
        let temp_dir = tempfile::tempdir().unwrap();
        let config_path = temp_dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            r#"
[models]
default_model = "gpt-4"

[loop_control]
max_steps_per_turn = 10
max_context_size = 128000
"#,
        )
        .unwrap();

        let initial = crate::config_registry::parse_config_file(&config_path)
            .unwrap()
            .to_legacy_config();
        let propagator = ConfigChangePropagator::new(config_path.clone(), Some(initial));

        let subscriber = Arc::new(MockSubscriber::new());
        propagator.subscribe("model", Box::new(Arc::clone(&subscriber)));

        // Modify config
        std::fs::write(
            &config_path,
            r#"
[models]
default_model = "claude"

[loop_control]
max_steps_per_turn = 10
max_context_size = 128000
"#,
        )
        .unwrap();

        propagator.check_and_notify().unwrap();
        assert!(
            subscriber.was_triggered(),
            "Subscriber should have been notified"
        );
    }

    #[test]
    fn test_propagator_skips_unchanged_section() {
        let temp_dir = tempfile::tempdir().unwrap();
        let config_path = temp_dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            r#"
[models]
default_model = "gpt-4"

[loop_control]
max_steps_per_turn = 10
max_context_size = 128000
"#,
        )
        .unwrap();

        let initial = crate::config_registry::parse_config_file(&config_path)
            .unwrap()
            .to_legacy_config();
        let propagator = ConfigChangePropagator::new(config_path.clone(), Some(initial));

        let model_sub = Arc::new(MockSubscriber::new());
        propagator.subscribe("model", Box::new(Arc::clone(&model_sub)));

        // Modify only loop_control
        std::fs::write(
            &config_path,
            r#"
[models]
default_model = "gpt-4"

[loop_control]
max_steps_per_turn = 50
max_context_size = 128000
"#,
        )
        .unwrap();

        propagator.check_and_notify().unwrap();
        assert!(
            !model_sub.was_triggered(),
            "Model subscriber should NOT have been notified"
        );
    }

    #[test]
    fn test_propagator_notifies_only_once_per_change() {
        let temp_dir = tempfile::tempdir().unwrap();
        let config_path = temp_dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            r#"
[models]
default_model = "gpt-4"

[loop_control]
max_steps_per_turn = 10
max_context_size = 128000
"#,
        )
        .unwrap();

        let initial = crate::config_registry::parse_config_file(&config_path)
            .unwrap()
            .to_legacy_config();
        let propagator = ConfigChangePropagator::new(config_path.clone(), Some(initial));

        let subscriber = Arc::new(MockSubscriber::new());
        propagator.subscribe("model", Box::new(Arc::clone(&subscriber)));

        // No change
        propagator.check_and_notify().unwrap();
        assert!(
            !subscriber.was_triggered(),
            "Should not trigger when config unchanged"
        );
    }

    #[test]
    fn test_diff_sections_detects_providers_change() {
        let old = Config {
            default_model: "gpt-4".to_string(),
            max_steps_per_turn: Some(10),
            max_context_size: Some(128_000),
            providers: Some(serde_json::json!({"openai": "key1"})),
            ..Config::default()
        };
        let new = Config {
            default_model: "gpt-4".to_string(),
            max_steps_per_turn: Some(10),
            max_context_size: Some(128_000),
            providers: Some(serde_json::json!({"openai": "key2"})),
            ..Config::default()
        };
        let changed = ConfigChangePropagator::diff_sections(&old, &new);
        assert!(changed.contains(&"providers".to_string()));
    }

    #[test]
    fn test_diff_sections_detects_mcp_fingerprint_change() {
        let mut old = Config::default();
        old.mcp_fingerprint = r#"{"servers":{}}"#.to_string();
        let mut new = old.clone();
        new.mcp_fingerprint =
            r#"{"servers":{"fs":{"command":"npx","args":["-y","mcp"],"env":{}}}}"#.to_string();
        let changed = ConfigChangePropagator::diff_sections(&old, &new);
        assert!(changed.contains(&"mcp".to_string()));
    }

    #[test]
    fn test_propagator_notifies_mcp_subscriber() {
        let temp_dir = tempfile::tempdir().unwrap();
        let config_path = temp_dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            r#"
[models]
default_model = "echo"

[loop_control]
max_steps_per_turn = 10
max_context_size = 128000

[mcp.servers.fs]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem"]
"#,
        )
        .unwrap();

        let initial = crate::config_registry::parse_config_file(&config_path)
            .unwrap()
            .to_legacy_config();
        let propagator = ConfigChangePropagator::new(config_path.clone(), Some(initial));

        let subscriber = Arc::new(MockSubscriber::new());
        propagator.subscribe("mcp", Box::new(Arc::clone(&subscriber)));

        std::fs::write(
            &config_path,
            r#"
[models]
default_model = "echo"

[loop_control]
max_steps_per_turn = 10
max_context_size = 128000

[mcp.servers.fs]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
"#,
        )
        .unwrap();

        propagator.check_and_notify().unwrap();
        assert!(
            subscriber.was_triggered(),
            "mcp subscriber should run when MCP server args change"
        );
    }
}
