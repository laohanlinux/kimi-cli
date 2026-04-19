//! Typed config registry with per-section parsing and validation.
//!
//! Plugins can register their own `ConfigSection` types and cross-validators.
//! `parse_config_file` is the single entry point for config loading.

use serde::de::DeserializeOwned;
use std::any::Any;
use std::collections::HashMap;

/// Error type for config validation failures.
#[derive(Debug, Clone)]
pub struct ConfigError {
    pub section: Option<String>,
    pub message: String,
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(section) = &self.section {
            write!(f, "config[{}]: {}", section, self.message)
        } else {
            write!(f, "config: {}", self.message)
        }
    }
}

impl std::error::Error for ConfigError {}

/// A typed config section that can be parsed and validated independently.
pub trait ConfigSection: DeserializeOwned + Clone + Send + Sync + 'static {
    fn section_name() -> &'static str;
    fn validate(&self) -> Result<(), ConfigError>;
    fn default() -> Self;
}

/// Cross-section validator function.
pub type CrossValidator = Box<dyn Fn(&toml::map::Map<String, toml::Value>) -> Result<(), ConfigError> + Send + Sync>;

/// Plugin-extensible config registry (§8.1 deviation).
///
/// Each section is independently parsed and validated. Plugins can register
/// their own sections and cross-validators at load time.
#[allow(clippy::type_complexity)]
pub struct ConfigRegistry {
    sections: HashMap<String, Box<dyn Any + Send + Sync>>,
    parsers: HashMap<String, Box<dyn Fn(&toml::Value) -> Result<Box<dyn Any + Send + Sync>, ConfigError> + Send + Sync>>,
    validators: Vec<CrossValidator>,
}

impl ConfigRegistry {
    pub fn new() -> Self {
        Self {
            sections: HashMap::new(),
            parsers: HashMap::new(),
            validators: Vec::new(),
        }
    }

    /// Register a typed config section parser.
    pub fn register_section<T: ConfigSection>(&mut self) {
        let name = T::section_name().to_string();
        self.parsers.insert(name, Box::new(|value| {
            let section: T = value.clone().try_into()
                .map_err(|e| ConfigError {
                    section: Some(T::section_name().to_string()),
                    message: format!("parse error: {}", e),
                })?;
            section.validate()?;
            Ok(Box::new(section) as Box<dyn Any + Send + Sync>)
        }));
    }

    /// Register a cross-section validator.
    pub fn register_validator(&mut self, validator: CrossValidator) {
        self.validators.push(validator);
    }

    /// Get raw toml table from a parsed value.
    #[allow(dead_code)]
    fn raw_table(raw: &toml::Value) -> Option<&toml::map::Map<String, toml::Value>> {
        match raw {
            toml::Value::Table(t) => Some(t),
            _ => None,
        }
    }

    /// Parse raw TOML value and validate all registered sections.
    pub fn parse(&mut self, raw: &toml::Value) -> Result<(), Vec<ConfigError>> {
        let mut errors = Vec::new();

        // Parse each registered section
        for (name, parser) in &self.parsers {
            let section_value = raw.get(name).cloned().unwrap_or(toml::Value::Table(toml::Table::new()));
            match parser(&section_value) {
                Ok(section) => {
                    self.sections.insert(name.clone(), section);
                }
                Err(e) => errors.push(e),
            }
        }

        // Run cross-section validators
        if let toml::Value::Table(table) = raw {
            for validator in &self.validators {
                if let Err(e) = validator(table) {
                    errors.push(e);
                }
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    /// Get a parsed section by type.
    pub fn get_section<T: ConfigSection>(&self) -> Option<&T> {
        self.sections.get(T::section_name())?
            .downcast_ref::<T>()
    }

    /// Get a parsed section or return its default.
    pub fn get_section_or_default<T: ConfigSection>(&self) -> T {
        self.get_section::<T>().cloned().unwrap_or_else(T::default)
    }
}

// --- Built-in config sections ---

/// Core loop control settings.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct LoopControlSection {
    #[serde(default = "default_max_steps")]
    pub max_steps_per_turn: usize,
    #[serde(default = "default_max_context")]
    pub max_context_size: usize,
}

fn default_max_steps() -> usize { 100 }
fn default_max_context() -> usize { 128_000 }

impl ConfigSection for LoopControlSection {
    fn section_name() -> &'static str { "loop_control" }

    fn validate(&self) -> Result<(), ConfigError> {
        if self.max_steps_per_turn == 0 {
            return Err(ConfigError {
                section: Some("loop_control".to_string()),
                message: "max_steps_per_turn must be > 0".to_string(),
            });
        }
        if self.max_context_size < 1000 {
            return Err(ConfigError {
                section: Some("loop_control".to_string()),
                message: "max_context_size must be >= 1000".to_string(),
            });
        }
        Ok(())
    }

    fn default() -> Self {
        Self {
            max_steps_per_turn: default_max_steps(),
            max_context_size: default_max_context(),
        }
    }
}

/// Model and provider configuration.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct ModelsSection {
    #[serde(default)]
    pub default_model: String,
    #[serde(default)]
    pub models: HashMap<String, toml::Value>,
    #[serde(default)]
    pub providers: HashMap<String, toml::Value>,
    /// When `false`, multimodal-looking user input is rejected (after env overrides on legacy `Config`).
    #[serde(default = "default_models_supports_vision")]
    pub supports_vision: bool,
    /// Skip `user_input::model_supports_vision_hint` for the configured `default_model`.
    #[serde(default)]
    pub ignore_vision_model_hint: bool,
    /// Optional per-model overrides for multimodal input (merged into legacy [`crate::config::Config::vision_by_model`]).
    #[serde(default)]
    pub vision_by_model: HashMap<String, bool>,
}

fn default_models_supports_vision() -> bool {
    true
}

impl ConfigSection for ModelsSection {
    fn section_name() -> &'static str { "models" }

    fn validate(&self) -> Result<(), ConfigError> {
        if !self.default_model.is_empty() && !self.models.contains_key(&self.default_model) {
            // Default model not in models map is a warning, not an error
            tracing::warn!(
                "default_model '{}' not found in models map",
                self.default_model
            );
        }
        Ok(())
    }

    fn default() -> Self {
        Self {
            default_model: "echo".to_string(),
            models: HashMap::new(),
            providers: HashMap::new(),
            supports_vision: true,
            ignore_vision_model_hint: false,
            vision_by_model: HashMap::new(),
        }
    }
}

/// MCP server configuration.
#[derive(Debug, Clone, Default, serde::Deserialize, serde::Serialize)]
pub struct MCPSection {
    #[serde(default)]
    pub servers: HashMap<String, MCPServerConfig>,
}

#[derive(Debug, Clone, Default, serde::Deserialize, serde::Serialize)]
pub struct MCPServerConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
}

impl ConfigSection for MCPSection {
    fn section_name() -> &'static str { "mcp" }

    fn validate(&self) -> Result<(), ConfigError> {
        for (name, server) in &self.servers {
            if server.command.is_empty() {
                return Err(ConfigError {
                    section: Some("mcp".to_string()),
                    message: format!("server '{}' has empty command", name),
                });
            }
        }
        Ok(())
    }

    fn default() -> Self {
        Self {
            servers: HashMap::new(),
        }
    }
}

/// Trust profile configuration section.
#[derive(Debug, Clone, Default, serde::Deserialize, serde::Serialize)]
pub struct TrustProfileSection {
    #[serde(default = "default_trust_default")]
    pub default: String,
    #[serde(default)]
    pub overrides: Vec<crate::capability::CapabilityOverride>,
}

fn default_trust_default() -> String { "prompt".to_string() }

impl ConfigSection for TrustProfileSection {
    fn section_name() -> &'static str { "trust_profile" }

    fn validate(&self) -> Result<(), ConfigError> {
        let valid = ["auto", "prompt", "block"];
        if !valid.contains(&self.default.as_str()) {
            return Err(ConfigError {
                section: Some("trust_profile".to_string()),
                message: format!("default must be one of {:?}, got '{}'", valid, self.default),
            });
        }
        Ok(())
    }

    fn default() -> Self {
        Self {
            default: default_trust_default(),
            overrides: Vec::new(),
        }
    }
}

/// Compaction policy configuration.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct CompactionSection {
    #[serde(default = "default_threshold_percent")]
    pub threshold_percent: f64,
    #[serde(default = "default_threshold_absolute")]
    pub threshold_absolute: usize,
    #[serde(default = "default_min_messages")]
    pub min_messages: usize,
}

fn default_threshold_percent() -> f64 { 0.85 }
fn default_threshold_absolute() -> usize { 50_000 }
fn default_min_messages() -> usize { 4 }

impl ConfigSection for CompactionSection {
    fn section_name() -> &'static str { "compaction" }

    fn validate(&self) -> Result<(), ConfigError> {
        if !(0.0..=1.0).contains(&self.threshold_percent) {
            return Err(ConfigError {
                section: Some("compaction".to_string()),
                message: "threshold_percent must be in [0.0, 1.0]".to_string(),
            });
        }
        if self.threshold_absolute == 0 {
            return Err(ConfigError {
                section: Some("compaction".to_string()),
                message: "threshold_absolute must be > 0".to_string(),
            });
        }
        if self.min_messages == 0 {
            return Err(ConfigError {
                section: Some("compaction".to_string()),
                message: "min_messages must be > 0".to_string(),
            });
        }
        Ok(())
    }

    fn default() -> Self {
        Self {
            threshold_percent: default_threshold_percent(),
            threshold_absolute: default_threshold_absolute(),
            min_messages: default_min_messages(),
        }
    }
}

/// Orchestrator selection configuration.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct OrchestratorSection {
    #[serde(default = "default_orchestrator")]
    pub default_orchestrator: String,
    #[serde(default = "default_ralph_max_iterations")]
    pub ralph_max_iterations: usize,
}

fn default_ralph_max_iterations() -> usize { 5 }

fn default_orchestrator() -> String { "react".to_string() }

impl Default for OrchestratorSection {
    fn default() -> Self {
        Self {
            default_orchestrator: default_orchestrator(),
            ralph_max_iterations: default_ralph_max_iterations(),
        }
    }
}

impl ConfigSection for OrchestratorSection {
    fn section_name() -> &'static str { "orchestrator" }

    fn validate(&self) -> Result<(), ConfigError> {
        let valid = ["react", "plan", "ralph"];
        if !valid.contains(&self.default_orchestrator.as_str()) {
            return Err(ConfigError {
                section: Some("orchestrator".to_string()),
                message: format!("default_orchestrator must be one of {:?}, got '{}'", valid, self.default_orchestrator),
            });
        }
        Ok(())
    }

    fn default() -> Self {
        Self {
            default_orchestrator: default_orchestrator(),
            ralph_max_iterations: 5,
        }
    }
}

/// Build a default registry with all built-in sections registered.
pub fn default_registry() -> ConfigRegistry {
    let mut registry = ConfigRegistry::new();
    registry.register_section::<LoopControlSection>();
    registry.register_section::<ModelsSection>();
    registry.register_section::<MCPSection>();
    registry.register_section::<TrustProfileSection>();
    registry.register_section::<OrchestratorSection>();
    registry.register_section::<CompactionSection>();

    // Cross-validator: ensure default_model references a valid model
    registry.register_validator(Box::new(|raw| {
        let models = raw.get("models");
        let default = models
            .and_then(|m| m.get("default_model"))
            .and_then(|v| v.as_str());
        let model_map = models
            .and_then(|m| m.get("models"))
            .and_then(|v| v.as_table());

        if let Some(d) = default
            && d != "echo" {
                let has_map = model_map.is_some_and(|map| map.contains_key(d));
                if !has_map {
                    return Err(ConfigError {
                        section: None,
                        message: format!(
                            "default_model '{}' not found in models map",
                            d
                        ),
                    });
                }
            }
        Ok(())
    }));

    registry
}

/// Convert registry to legacy Config for backward compatibility during migration.
impl ConfigRegistry {
    pub fn to_legacy_config(&self) -> crate::config::Config {
        let models = self.get_section_or_default::<ModelsSection>();
        let loop_ctrl = self.get_section_or_default::<LoopControlSection>();
        let trust = self.get_section::<TrustProfileSection>();
        let orchestrator = self.get_section_or_default::<OrchestratorSection>();
        let compaction = self.get_section_or_default::<CompactionSection>();
        let mcp_sec = self.get_section_or_default::<MCPSection>();
        let mcp_fingerprint = serde_json::to_string(&mcp_sec).unwrap_or_default();

        crate::config::Config {
            default_model: models.default_model.clone(),
            max_steps_per_turn: Some(loop_ctrl.max_steps_per_turn),
            max_context_size: Some(loop_ctrl.max_context_size),
            models: if models.models.is_empty() {
                None
            } else {
                serde_json::to_value(&models.models).ok()
            },
            providers: if models.providers.is_empty() {
                None
            } else {
                serde_json::to_value(&models.providers).ok()
            },
            trust_profile: trust.map(|t| crate::capability::TrustProfile {
                default: t.default.clone(),
                overrides: t.overrides.clone(),
            }),
            default_orchestrator: orchestrator.default_orchestrator.clone(),
            ralph_max_iterations: orchestrator.ralph_max_iterations,
            compaction_threshold_percent: compaction.threshold_percent,
            compaction_threshold_absolute: compaction.threshold_absolute,
            compaction_min_messages: compaction.min_messages,
            mcp_fingerprint,
            supports_vision: models.supports_vision,
            ignore_vision_model_hint: models.ignore_vision_model_hint,
            vision_by_model: models.vision_by_model.clone(),
        }
    }
}

/// Parse a config file using the registry and return typed sections.
pub fn parse_config_file(path: &std::path::Path) -> anyhow::Result<ConfigRegistry> {
    let mut registry = default_registry();

    let raw: toml::Value = if path.exists() {
        let content = std::fs::read_to_string(path)?;
        toml::from_str(&content)?
    } else {
        toml::Value::Table(toml::Table::new())
    };

    if let Err(errors) = registry.parse(&raw) {
        for e in &errors {
            tracing::warn!("Config validation error: {}", e);
        }
        // Non-fatal: log warnings and continue with defaults
    }

    Ok(registry)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_loop_control_validation() {
        let valid = LoopControlSection {
            max_steps_per_turn: 50,
            max_context_size: 256_000,
        };
        assert!(valid.validate().is_ok());

        let invalid = LoopControlSection {
            max_steps_per_turn: 0,
            max_context_size: 256_000,
        };
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn test_trust_profile_validation() {
        let valid = TrustProfileSection {
            default: "auto".to_string(),
            overrides: vec![],
        };
        assert!(valid.validate().is_ok());

        let invalid = TrustProfileSection {
            default: "invalid".to_string(),
            overrides: vec![],
        };
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn test_registry_parse_valid() {
        let toml_str = r#"
[loop_control]
max_steps_per_turn = 50
max_context_size = 200000

[models]
default_model = "gpt-4"

[models.models.gpt-4]
provider = "openai"

[mcp.servers.fs]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem"]
"#;
        let raw: toml::Value = toml::from_str(toml_str).unwrap();
        let mut registry = default_registry();
        assert!(registry.parse(&raw).is_ok());

        let loop_ctrl = registry.get_section::<LoopControlSection>().unwrap();
        assert_eq!(loop_ctrl.max_steps_per_turn, 50);
        assert_eq!(loop_ctrl.max_context_size, 200_000);
    }

    #[test]
    fn test_registry_parse_invalid_mcp() {
        let toml_str = r#"
[mcp.servers.bad]
command = ""
"#;
        let raw: toml::Value = toml::from_str(toml_str).unwrap();
        let mut registry = default_registry();
        let result = registry.parse(&raw);
        assert!(result.is_err());
        let errors = result.unwrap_err();
        assert!(errors.iter().any(|e| e.message.contains("empty command")));
    }

    #[test]
    fn test_registry_defaults() {
        let raw = toml::Value::Table(toml::Table::new());
        let mut registry = default_registry();
        assert!(registry.parse(&raw).is_ok());

        let loop_ctrl = registry.get_section_or_default::<LoopControlSection>();
        assert_eq!(loop_ctrl.max_steps_per_turn, 100);
        assert_eq!(loop_ctrl.max_context_size, 128_000);
    }

    #[test]
    fn test_cross_validator_default_model() {
        let toml_str = r#"
[models]
default_model = "missing-model"
"#;
        let raw: toml::Value = toml::from_str(toml_str).unwrap();
        let mut registry = default_registry();
        let result = registry.parse(&raw);
        assert!(result.is_err());
        let errors = result.unwrap_err();
        assert!(errors.iter().any(|e| e.message.contains("not found in models map")));
    }

    #[test]
    fn test_to_legacy_config() {
        let toml_str = r#"
[loop_control]
max_steps_per_turn = 50
max_context_size = 200000

[models]
default_model = "gpt-4"

[models.models.gpt-4]
provider = "openai"

[trust_profile]
default = "auto"
"#;
        let raw: toml::Value = toml::from_str(toml_str).unwrap();
        let mut registry = default_registry();
        registry.parse(&raw).unwrap();

        let config = registry.to_legacy_config();
        assert_eq!(config.default_model, "gpt-4");
        assert_eq!(config.max_steps_per_turn, Some(50));
        assert_eq!(config.max_context_size, Some(200_000));
        assert!(config.trust_profile.is_some());
        assert_eq!(config.trust_profile.unwrap().default, "auto");
        assert!(config.supports_vision);
        assert!(!config.ignore_vision_model_hint);
    }

    #[test]
    fn test_to_legacy_config_models_vision_flags() {
        let toml_str = r#"
[loop_control]
max_steps_per_turn = 10
max_context_size = 128000

[models]
default_model = "echo"
supports_vision = false
ignore_vision_model_hint = true
"#;
        let raw: toml::Value = toml::from_str(toml_str).unwrap();
        let mut registry = default_registry();
        registry.parse(&raw).unwrap();
        let config = registry.to_legacy_config();
        assert!(!config.supports_vision);
        assert!(config.ignore_vision_model_hint);
    }

    #[test]
    fn test_to_legacy_config_vision_by_model() {
        let toml_str = r#"
[loop_control]
max_steps_per_turn = 10
max_context_size = 128000

[models]
default_model = "echo"

[models.models.echo]
provider = "mock"

[models.vision_by_model]
echo = false
gpt-4o = true
"#;
        let raw: toml::Value = toml::from_str(toml_str).unwrap();
        let mut registry = default_registry();
        registry.parse(&raw).unwrap();
        let config = registry.to_legacy_config();
        assert_eq!(config.vision_by_model.get("echo"), Some(&false));
        assert_eq!(config.vision_by_model.get("gpt-4o"), Some(&true));
    }

    #[test]
    fn test_to_legacy_config_defaults() {
        let raw = toml::Value::Table(toml::Table::new());
        let mut registry = default_registry();
        registry.parse(&raw).unwrap();

        let config = registry.to_legacy_config();
        assert_eq!(config.max_steps_per_turn, Some(100));
        assert_eq!(config.max_context_size, Some(128_000));
        assert_eq!(config.default_orchestrator, "react");
        assert_eq!(config.ralph_max_iterations, 5);
        assert_eq!(config.compaction_threshold_percent, 0.85);
        assert_eq!(config.compaction_threshold_absolute, 50_000);
        assert_eq!(config.compaction_min_messages, 4);
        assert!(
            config.mcp_fingerprint.contains("servers"),
            "mcp fingerprint should reflect default MCP section, got {:?}",
            config.mcp_fingerprint
        );
        assert!(config.supports_vision);
        assert!(!config.ignore_vision_model_hint);
    }

    #[test]
    fn test_compaction_section_validation() {
        let valid = CompactionSection {
            threshold_percent: 0.5,
            threshold_absolute: 10_000,
            min_messages: 8,
        };
        assert!(valid.validate().is_ok());

        let invalid_percent = CompactionSection {
            threshold_percent: 1.5,
            threshold_absolute: 10_000,
            min_messages: 8,
        };
        assert!(invalid_percent.validate().is_err());

        let invalid_messages = CompactionSection {
            threshold_percent: 0.5,
            threshold_absolute: 10_000,
            min_messages: 0,
        };
        assert!(invalid_messages.validate().is_err());
    }

    #[test]
    fn test_compaction_section_parsing() {
        let toml_str = r#"
[compaction]
threshold_percent = 0.75
threshold_absolute = 20000
min_messages = 8
"#;
        let raw: toml::Value = toml::from_str(toml_str).unwrap();
        let mut registry = default_registry();
        assert!(registry.parse(&raw).is_ok());

        let compact = registry.get_section::<CompactionSection>().unwrap();
        assert_eq!(compact.threshold_percent, 0.75);
        assert_eq!(compact.threshold_absolute, 20_000);
        assert_eq!(compact.min_messages, 8);

        let config = registry.to_legacy_config();
        assert_eq!(config.compaction_threshold_percent, 0.75);
        assert_eq!(config.compaction_threshold_absolute, 20_000);
        assert_eq!(config.compaction_min_messages, 8);
    }

    #[test]
    fn test_orchestrator_section_validation() {
        let toml_str = r#"
[orchestrator]
default_orchestrator = "invalid"
"#;
        let raw: toml::Value = toml::from_str(toml_str).unwrap();
        let mut registry = default_registry();
        let result = registry.parse(&raw);
        assert!(result.is_err());
        let errors = result.unwrap_err();
        assert!(errors.iter().any(|e| e.message.contains("react")));
    }

    #[test]
    fn test_orchestrator_section_valid() {
        let toml_str = r#"
[orchestrator]
default_orchestrator = "ralph"
ralph_max_iterations = 10
"#;
        let raw: toml::Value = toml::from_str(toml_str).unwrap();
        let mut registry = default_registry();
        assert!(registry.parse(&raw).is_ok());

        let orch = registry.get_section::<OrchestratorSection>().unwrap();
        assert_eq!(orch.default_orchestrator, "ralph");
        assert_eq!(orch.ralph_max_iterations, 10);

        let config = registry.to_legacy_config();
        assert_eq!(config.default_orchestrator, "ralph");
        assert_eq!(config.ralph_max_iterations, 10);
    }
}
