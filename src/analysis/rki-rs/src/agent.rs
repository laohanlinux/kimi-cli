use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Subagent entry declared in an agent spec YAML.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct SubagentEntry {
    pub path: String,
    pub description: String,
}

/// Raw agent spec as deserialized from YAML (before extend resolution).
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct RawAgentSpec {
    /// Omitted in child specs that only `extend` a parent.
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub system_prompt_path: Option<String>,
    #[serde(default)]
    pub system_prompt_args: HashMap<String, String>,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub subagents: HashMap<String, SubagentEntry>,
    #[serde(default)]
    pub extend: Option<String>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct AgentSpec {
    pub name: String,
    pub system_prompt: String,
    pub tools: Vec<String>,
    pub capabilities: Vec<String>,
    pub subagents: HashMap<String, SubagentEntry>,
}

impl Default for AgentSpec {
    fn default() -> Self {
        Self {
            name: String::new(),
            system_prompt: String::new(),
            tools: vec![],
            capabilities: vec![],
            subagents: HashMap::new(),
        }
    }
}

impl AgentSpec {
    /// Load an agent spec from YAML, resolving the `extend` inheritance chain.
    pub fn from_yaml(path: &Path) -> anyhow::Result<Self> {
        Self::load_with_seen(path, &mut std::collections::HashSet::new())
    }

    fn load_with_seen(
        path: &Path,
        seen: &mut std::collections::HashSet<PathBuf>,
    ) -> anyhow::Result<Self> {
        let canonical = std::fs::canonicalize(path)?;
        if !seen.insert(canonical.clone()) {
            anyhow::bail!("Circular extend detected in agent spec: {:?}", path);
        }

        let content = std::fs::read_to_string(path)?;
        let raw: serde_yaml::Value = serde_yaml::from_str(&content)?;

        // Extract the "agent" key if present, else treat whole doc as agent
        let agent_value = raw.get("agent").cloned().unwrap_or(raw);
        let mut raw_spec: RawAgentSpec = serde_yaml::from_value(agent_value)?;

        // Resolve extend chain
        let base_dir = path.parent().unwrap_or(Path::new("."));
        let mut parent_system_prompt = String::new();
        if let Some(ref extend_path) = raw_spec.extend {
            let parent_path = base_dir.join(extend_path);
            let parent = Self::load_with_seen(&parent_path, seen)?;
            parent_system_prompt = parent.system_prompt.clone();
            // Inherit: parent values are defaults, child overrides
            if raw_spec.name.is_empty() {
                raw_spec.name = parent.name;
            }
            // Merge tools: child tools override parent tools (deduped, child first)
            let mut merged_tools = raw_spec.tools.clone();
            for t in parent.tools {
                if !merged_tools.contains(&t) {
                    merged_tools.push(t);
                }
            }
            raw_spec.tools = merged_tools;
            // Merge capabilities
            let mut merged_caps = raw_spec.capabilities.clone();
            for c in parent.capabilities {
                if !merged_caps.contains(&c) {
                    merged_caps.push(c);
                }
            }
            raw_spec.capabilities = merged_caps;
            // Merge subagents: child overrides parent entries by name
            let mut merged_subagents = parent.subagents.clone();
            for (k, v) in raw_spec.subagents {
                merged_subagents.insert(k, v);
            }
            raw_spec.subagents = merged_subagents;
        }

        // Load system prompt from file if specified
        let system_prompt = if let Some(ref sp_path) = raw_spec.system_prompt_path {
            let full_path = base_dir.join(sp_path);
            let template = std::fs::read_to_string(&full_path)?;
            // Simple template substitution for system_prompt_args
            let mut rendered = template;
            for (key, value) in &raw_spec.system_prompt_args {
                rendered = rendered.replace(&format!("${{{}}}", key), value);
            }
            rendered
        } else {
            String::new()
        };

        let system_prompt = if system_prompt.is_empty() && !parent_system_prompt.is_empty() {
            parent_system_prompt
        } else {
            system_prompt
        };

        Ok(Self {
            name: raw_spec.name,
            system_prompt,
            tools: raw_spec.tools,
            capabilities: raw_spec.capabilities,
            subagents: raw_spec.subagents,
        })
    }
}

#[derive(Debug, Clone)]
pub struct Agent {
    pub spec: AgentSpec,
    pub system_prompt: String,
}

#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
pub struct LaborMarket {
    subagent_types: HashMap<String, AgentSpec>,
}

impl LaborMarket {
    pub fn new() -> Self {
        Self {
            subagent_types: HashMap::new(),
        }
    }

    pub fn register(&mut self, name: String, spec: AgentSpec) {
        self.subagent_types.insert(name, spec);
    }

    pub fn get(&self, name: &str) -> Option<&AgentSpec> {
        self.subagent_types.get(name)
    }

    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.subagent_types.keys()
    }

    /// Load each `spec.subagents` YAML (`path` relative to the spec file directory) and register by name.
    pub fn register_subagents_from_spec(
        &mut self,
        spec: &AgentSpec,
        spec_yaml_dir: &Path,
    ) -> anyhow::Result<()> {
        for (name, entry) in &spec.subagents {
            let nested_path = spec_yaml_dir.join(&entry.path);
            let nested = AgentSpec::from_yaml(&nested_path)?;
            self.register(name.clone(), nested);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_labor_market_register_and_get() {
        let mut lm = LaborMarket::new();
        let spec = AgentSpec {
            name: "coder".to_string(),
            system_prompt: "You code.".to_string(),
            tools: vec!["shell".to_string()],
            capabilities: vec!["filesystem:read".to_string()],
            subagents: HashMap::new(),
        };
        lm.register("coder".to_string(), spec.clone());

        let got = lm.get("coder").unwrap();
        assert_eq!(got.name, "coder");
        assert_eq!(got.tools.len(), 1);

        assert!(lm.get("missing").is_none());
    }

    #[test]
    fn test_agent_spec_clone() {
        let spec = AgentSpec {
            name: "a".to_string(),
            system_prompt: "sys".to_string(),
            tools: vec!["t1".to_string()],
            capabilities: vec![],
            subagents: HashMap::new(),
        };
        let agent = Agent {
            spec: spec.clone(),
            system_prompt: "rendered".to_string(),
        };
        assert_eq!(agent.spec.name, "a");
        assert_eq!(agent.system_prompt, "rendered");
    }

    #[test]
    fn test_labor_market_default() {
        let lm: LaborMarket = Default::default();
        assert!(lm.get("anything").is_none());
    }

    #[test]
    fn test_agent_debug_format() {
        let agent = Agent {
            spec: AgentSpec {
                name: "test".to_string(),
                system_prompt: "sys".to_string(),
                tools: vec![],
                capabilities: vec![],
                subagents: HashMap::new(),
            },
            system_prompt: "rendered".to_string(),
        };
        let debug = format!("{:?}", agent);
        assert!(debug.contains("test"));
    }

    #[test]
    fn test_agent_spec_yaml_basic() {
        let dir = tempfile::tempdir().unwrap();
        let yaml_path = dir.path().join("agent.yaml");
        let mut f = std::fs::File::create(&yaml_path).unwrap();
        f.write_all(
            b"agent:
  name: test-agent
  system_prompt_path: ./system.md
  tools:
    - shell
",
        )
        .unwrap();

        let system_path = dir.path().join("system.md");
        let mut sf = std::fs::File::create(&system_path).unwrap();
        sf.write_all(b"Hello ${NAME}!").unwrap();

        let spec = AgentSpec::from_yaml(&yaml_path).unwrap();
        assert_eq!(spec.name, "test-agent");
        assert_eq!(spec.tools, vec!["shell"]);
        assert!(spec.system_prompt.contains("Hello"));
    }

    #[test]
    fn test_agent_spec_yaml_extend() {
        let dir = tempfile::tempdir().unwrap();

        let base_yaml = dir.path().join("base.yaml");
        let mut f = std::fs::File::create(&base_yaml).unwrap();
        f.write_all(
            b"agent:
  name: base
  tools:
    - shell
  capabilities:
    - fs:read
",
        )
        .unwrap();

        let child_yaml = dir.path().join("child.yaml");
        let mut f = std::fs::File::create(&child_yaml).unwrap();
        f.write_all(
            b"agent:
  extend: ./base.yaml
  tools:
    - read_file
  subagents:
    coder:
      path: ./coder.yaml
      description: A coder
",
        )
        .unwrap();

        let spec = AgentSpec::from_yaml(&child_yaml).unwrap();
        assert_eq!(spec.name, "base"); // inherited
        assert!(spec.tools.contains(&"shell".to_string()));
        assert!(spec.tools.contains(&"read_file".to_string()));
        assert!(spec.capabilities.contains(&"fs:read".to_string()));
        assert!(spec.subagents.contains_key("coder"));
    }

    #[test]
    fn test_agent_spec_extend_inherits_parent_system_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let base_yaml = dir.path().join("base.yaml");
        let mut f = std::fs::File::create(&base_yaml).unwrap();
        f.write_all(
            b"agent:
  name: base
  system_prompt_path: ./base.md
",
        )
        .unwrap();
        std::fs::write(dir.path().join("base.md"), b"Parent prompt body.").unwrap();

        let child_yaml = dir.path().join("child.yaml");
        let mut f = std::fs::File::create(&child_yaml).unwrap();
        f.write_all(
            b"agent:
  extend: ./base.yaml
  tools:
    - shell
",
        )
        .unwrap();

        let spec = AgentSpec::from_yaml(&child_yaml).unwrap();
        assert_eq!(spec.system_prompt, "Parent prompt body.");
    }

    #[test]
    fn test_labor_market_register_subagents_from_spec() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("coder.yaml"),
            b"agent:
  name: coder
  system_prompt_path: ./c.md
",
        )
        .unwrap();
        std::fs::write(dir.path().join("c.md"), b"Coder sys").unwrap();

        let root_yaml = dir.path().join("root.yaml");
        std::fs::write(
            &root_yaml,
            b"agent:
  name: root
  subagents:
    coder:
      path: ./coder.yaml
      description: sub
",
        )
        .unwrap();

        let spec = AgentSpec::from_yaml(&root_yaml).unwrap();
        let mut lm = LaborMarket::new();
        lm.register_subagents_from_spec(&spec, dir.path()).unwrap();
        let got = lm.get("coder").expect("coder registered");
        assert_eq!(got.name, "coder");
        assert!(got.system_prompt.contains("Coder"));
    }

    #[test]
    fn test_agent_spec_circular_extend_fails() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.yaml");
        let b = dir.path().join("b.yaml");

        let mut f = std::fs::File::create(&a).unwrap();
        f.write_all(
            b"agent:
  extend: ./b.yaml
",
        )
        .unwrap();
        let mut f = std::fs::File::create(&b).unwrap();
        f.write_all(
            b"agent:
  extend: ./a.yaml
",
        )
        .unwrap();

        assert!(AgentSpec::from_yaml(&a).is_err());
    }
}
