use crate::capability::TrustProfile;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub default_model: String,
    pub max_steps_per_turn: Option<usize>,
    pub max_context_size: Option<usize>,
    pub models: Option<serde_json::Value>,
    pub providers: Option<serde_json::Value>,
    pub trust_profile: Option<TrustProfile>,
    #[serde(default = "default_orchestrator")]
    pub default_orchestrator: String,
    #[serde(default)]
    pub ralph_max_iterations: usize,
    /// Compaction threshold as fraction of max_context_size (0.0–1.0).
    #[serde(default = "default_compaction_threshold_percent")]
    pub compaction_threshold_percent: f64,
    /// Absolute token buffer before compaction triggers.
    #[serde(default = "default_compaction_threshold_absolute")]
    pub compaction_threshold_absolute: usize,
    /// Minimum messages retained after compaction.
    #[serde(default = "default_compaction_min_messages")]
    pub compaction_min_messages: usize,
    /// JSON fingerprint of `[mcp]` from [`crate::config_registry::ConfigRegistry`] (hot-reload diff only).
    #[serde(default)]
    pub mcp_fingerprint: String,
    /// When `false`, reject user text that looks like embedded images (§1.2 L16 text-only models).
    #[serde(default = "default_supports_vision")]
    pub supports_vision: bool,
    /// When `true`, skip [`crate::user_input::model_supports_vision_hint`] so only `supports_vision` applies.
    #[serde(default)]
    pub ignore_vision_model_hint: bool,
    /// Per-model multimodal gate from registry `[models.vision_by_model]` (ASCII keys; lookup is case-insensitive).
    #[serde(default)]
    pub vision_by_model: HashMap<String, bool>,
}

fn default_orchestrator() -> String {
    "react".to_string()
}
fn default_compaction_threshold_percent() -> f64 {
    0.85
}
fn default_compaction_threshold_absolute() -> usize {
    50_000
}
fn default_compaction_min_messages() -> usize {
    4
}

fn default_supports_vision() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        Self {
            default_model: "echo".to_string(),
            max_steps_per_turn: None,
            max_context_size: None,
            models: None,
            providers: None,
            trust_profile: None,
            default_orchestrator: default_orchestrator(),
            ralph_max_iterations: 5,
            compaction_threshold_percent: default_compaction_threshold_percent(),
            compaction_threshold_absolute: default_compaction_threshold_absolute(),
            compaction_min_messages: default_compaction_min_messages(),
            mcp_fingerprint: String::new(),
            supports_vision: default_supports_vision(),
            ignore_vision_model_hint: false,
            vision_by_model: HashMap::new(),
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        if path.exists() {
            let content = std::fs::read_to_string(path)?;
            let config: Config = toml::from_str(&content)?;
            Ok(config)
        } else {
            Ok(Config::default())
        }
    }

    /// Apply environment variable overrides (§4.1 precedence level 2).
    /// Overrides are applied in-place; higher precedence than config file.
    pub fn apply_env_overrides(&mut self) {
        if let Ok(val) = std::env::var("KIMI_MODEL")
            && !val.is_empty()
        {
            self.default_model = val;
        }
        if let Ok(val) = std::env::var("KIMI_MAX_STEPS") {
            if let Ok(n) = val.parse::<usize>() {
                self.max_steps_per_turn = Some(n);
            }
        }
        if let Ok(val) = std::env::var("KIMI_MAX_CONTEXT_SIZE") {
            if let Ok(n) = val.parse::<usize>() {
                self.max_context_size = Some(n);
            }
        }
        if let Ok(val) = std::env::var("KIMI_ORCHESTRATOR")
            && !val.is_empty()
        {
            self.default_orchestrator = val;
        }
        if let Ok(val) = std::env::var("KIMI_RALPH_MAX_ITERATIONS") {
            if let Ok(n) = val.parse::<usize>() {
                self.ralph_max_iterations = n;
            }
        }
        if let Ok(val) = std::env::var("KIMI_COMPACTION_THRESHOLD_PERCENT") {
            if let Ok(f) = val.parse::<f64>() {
                self.compaction_threshold_percent = f.clamp(0.0, 1.0);
            }
        }
        if let Ok(val) = std::env::var("KIMI_COMPACTION_THRESHOLD_ABSOLUTE") {
            if let Ok(n) = val.parse::<usize>() {
                self.compaction_threshold_absolute = n;
            }
        }
        if let Ok(val) = std::env::var("KIMI_COMPACTION_MIN_MESSAGES") {
            if let Ok(n) = val.parse::<usize>() {
                self.compaction_min_messages = n;
            }
        }
        if let Ok(val) = std::env::var("KIMI_SUPPORTS_VISION") {
            let v = val.trim().to_ascii_lowercase();
            if v == "0" || v == "false" || v == "no" {
                self.supports_vision = false;
            } else if v == "1" || v == "true" || v == "yes" {
                self.supports_vision = true;
            }
        }
        if let Ok(val) = std::env::var("KIMI_IGNORE_VISION_MODEL_HINT") {
            let v = val.trim().to_ascii_lowercase();
            if v == "1" || v == "true" || v == "yes" {
                self.ignore_vision_model_hint = true;
            } else if v == "0" || v == "false" || v == "no" {
                self.ignore_vision_model_hint = false;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Process environment is global; parallel tests that set `KIMI_*` vars must not interleave.
    static ENV_OVERRIDE_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn test_config_load_defaults_when_missing() {
        let cfg = Config::load(Path::new("/nonexistent/path")).unwrap();
        assert_eq!(cfg.default_model, "echo");
        assert_eq!(cfg.max_steps_per_turn, None);
        assert_eq!(cfg.max_context_size, None);
        assert_eq!(cfg.compaction_threshold_percent, 0.85);
        assert_eq!(cfg.compaction_threshold_absolute, 50_000);
        assert_eq!(cfg.compaction_min_messages, 4);
        assert!(cfg.supports_vision);
    }

    #[test]
    fn test_config_load_from_toml() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
default_model = "kimi-k2"
max_steps_per_turn = 50
max_context_size = 64000
"#,
        )
        .unwrap();

        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.default_model, "kimi-k2");
        assert_eq!(cfg.max_steps_per_turn, Some(50));
        assert_eq!(cfg.max_context_size, Some(64000));
    }

    #[test]
    fn test_config_load_partial_toml() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        std::fs::write(&path, r#"default_model = "gpt-4""#).unwrap();

        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.default_model, "gpt-4");
        assert!(cfg.max_steps_per_turn.is_none());
    }

    #[test]
    fn test_config_clone() {
        let cfg = Config::load(Path::new("/nonexistent")).unwrap();
        let cfg2 = cfg.clone();
        assert_eq!(cfg2.default_model, "echo");
    }

    #[test]
    fn test_config_debug_format() {
        let cfg = Config::load(Path::new("/nonexistent")).unwrap();
        let s = format!("{:?}", cfg);
        assert!(s.contains("echo"));
    }

    #[test]
    fn test_env_override_model() {
        let _g = ENV_OVERRIDE_LOCK.lock().unwrap();
        unsafe {
            std::env::set_var("KIMI_MODEL", "gpt-4o");
        }
        let mut cfg = Config::default();
        cfg.apply_env_overrides();
        assert_eq!(cfg.default_model, "gpt-4o");
        unsafe {
            std::env::remove_var("KIMI_MODEL");
        }
    }

    #[test]
    fn test_env_override_max_steps_and_context() {
        let _g = ENV_OVERRIDE_LOCK.lock().unwrap();
        unsafe {
            std::env::set_var("KIMI_MAX_STEPS", "25");
            std::env::set_var("KIMI_MAX_CONTEXT_SIZE", "32000");
        }
        let mut cfg = Config::default();
        cfg.apply_env_overrides();
        assert_eq!(cfg.max_steps_per_turn, Some(25));
        assert_eq!(cfg.max_context_size, Some(32000));
        unsafe {
            std::env::remove_var("KIMI_MAX_STEPS");
            std::env::remove_var("KIMI_MAX_CONTEXT_SIZE");
        }
    }

    #[test]
    fn test_env_override_orchestrator_and_ralph() {
        let _g = ENV_OVERRIDE_LOCK.lock().unwrap();
        unsafe {
            std::env::set_var("KIMI_ORCHESTRATOR", "ralph");
            std::env::set_var("KIMI_RALPH_MAX_ITERATIONS", "10");
        }
        let mut cfg = Config::default();
        cfg.apply_env_overrides();
        assert_eq!(cfg.default_orchestrator, "ralph");
        assert_eq!(cfg.ralph_max_iterations, 10);
        unsafe {
            std::env::remove_var("KIMI_ORCHESTRATOR");
            std::env::remove_var("KIMI_RALPH_MAX_ITERATIONS");
        }
    }

    #[test]
    fn test_env_override_compaction() {
        let _g = ENV_OVERRIDE_LOCK.lock().unwrap();
        unsafe {
            std::env::set_var("KIMI_COMPACTION_THRESHOLD_PERCENT", "0.75");
            std::env::set_var("KIMI_COMPACTION_THRESHOLD_ABSOLUTE", "30000");
            std::env::set_var("KIMI_COMPACTION_MIN_MESSAGES", "8");
        }
        let mut cfg = Config::default();
        cfg.apply_env_overrides();
        assert_eq!(cfg.compaction_threshold_percent, 0.75);
        assert_eq!(cfg.compaction_threshold_absolute, 30000);
        assert_eq!(cfg.compaction_min_messages, 8);
        unsafe {
            std::env::remove_var("KIMI_COMPACTION_THRESHOLD_PERCENT");
            std::env::remove_var("KIMI_COMPACTION_THRESHOLD_ABSOLUTE");
            std::env::remove_var("KIMI_COMPACTION_MIN_MESSAGES");
        }
    }

    #[test]
    fn test_env_override_supports_vision() {
        let _g = ENV_OVERRIDE_LOCK.lock().unwrap();
        unsafe {
            std::env::set_var("KIMI_SUPPORTS_VISION", "0");
        }
        let mut cfg = Config::default();
        cfg.apply_env_overrides();
        assert!(!cfg.supports_vision);
        unsafe {
            std::env::set_var("KIMI_SUPPORTS_VISION", "true");
        }
        let mut cfg2 = Config::default();
        cfg2.apply_env_overrides();
        assert!(cfg2.supports_vision);
        unsafe {
            std::env::remove_var("KIMI_SUPPORTS_VISION");
        }
    }

    #[test]
    fn test_env_override_ignore_vision_model_hint() {
        let _g = ENV_OVERRIDE_LOCK.lock().unwrap();
        unsafe {
            std::env::set_var("KIMI_IGNORE_VISION_MODEL_HINT", "1");
        }
        let mut cfg = Config::default();
        cfg.apply_env_overrides();
        assert!(cfg.ignore_vision_model_hint);
        unsafe {
            std::env::remove_var("KIMI_IGNORE_VISION_MODEL_HINT");
        }
    }

    #[test]
    fn test_env_override_ignores_empty() {
        let _g = ENV_OVERRIDE_LOCK.lock().unwrap();
        unsafe {
            std::env::set_var("KIMI_MODEL", "");
        }
        let mut cfg = Config::default();
        cfg.apply_env_overrides();
        assert_eq!(cfg.default_model, "echo"); // unchanged
        unsafe {
            std::env::remove_var("KIMI_MODEL");
        }
    }
}
