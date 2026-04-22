use crate::tools::{ContentBlock, Tool, ToolContext, ToolMetrics, ToolOutput, ToolResult};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

type ToolFactory = fn() -> Box<dyn Tool>;

/// Registry of built-in tools addressable by manifest `python_class` / `rust_type` entries.
/// Maps "module:class" → factory function that produces a `Box<dyn Tool>`.
static BUILTIN_REGISTRY: OnceLock<HashMap<String, ToolFactory>> = OnceLock::new();

/// Initialize the built-in tool registry with known tools.
fn init_builtin_registry() -> HashMap<String, ToolFactory> {
    let mut reg: HashMap<String, ToolFactory> = HashMap::new();
    reg.insert("rki_rs::tools::shell::ShellTool".to_string(), || {
        Box::new(crate::tools::ShellTool)
    });
    reg.insert("rki_rs::tools::file::ReadFileTool".to_string(), || {
        Box::new(crate::tools::ReadFileTool)
    });
    reg.insert("rki_rs::tools::file::WriteFileTool".to_string(), || {
        Box::new(crate::tools::WriteFileTool)
    });
    reg.insert(
        "rki_rs::tools::file::StrReplaceFileTool".to_string(),
        || Box::new(crate::tools::StrReplaceFileTool),
    );
    reg.insert("rki_rs::tools::web::SearchWebTool".to_string(), || {
        Box::new(crate::tools::SearchWebTool)
    });
    reg.insert("rki_rs::tools::web::FetchURLTool".to_string(), || {
        Box::new(crate::tools::FetchURLTool)
    });
    reg.insert("rki_rs::tools::misc::ThinkTool".to_string(), || {
        Box::new(crate::tools::ThinkTool)
    });
    reg.insert("rki_rs::tools::task::TaskListTool".to_string(), || {
        Box::new(crate::tools::TaskListTool)
    });
    reg.insert("rki_rs::tools::task::TaskOutputTool".to_string(), || {
        Box::new(crate::tools::TaskOutputTool)
    });
    reg.insert("rki_rs::tools::task::TaskStopTool".to_string(), || {
        Box::new(crate::tools::TaskStopTool)
    });
    reg.insert("rki_rs::tools::misc::SetTodoListTool".to_string(), || {
        Box::new(crate::tools::set_todo_list_tool())
    });
    reg.insert(
        "rki_rs::tools::misc::AskUserQuestionTool".to_string(),
        || Box::new(crate::tools::AskUserQuestionTool),
    );
    reg.insert("rki_rs::tools::misc::SendDMailTool".to_string(), || {
        Box::new(crate::tools::send_dmail_tool())
    });
    reg.insert("rki_rs::tools::agent::AgentTool".to_string(), || {
        Box::new(crate::tools::AgentTool)
    });
    reg.insert("rki_rs::tools::plan::EnterPlanModeTool".to_string(), || {
        Box::new(crate::tools::enter_plan_mode_tool())
    });
    reg.insert("rki_rs::tools::plan::ExitPlanModeTool".to_string(), || {
        Box::new(crate::tools::exit_plan_mode_tool())
    });
    reg
}

fn get_builtin_registry() -> &'static HashMap<String, ToolFactory> {
    BUILTIN_REGISTRY.get_or_init(init_builtin_registry)
}

fn lookup_builtin_tool(key: &str) -> Option<Box<dyn Tool>> {
    get_builtin_registry().get(key).map(|f| f())
}

/// Returns the list of registered built-in tool keys (for diagnostics).
#[allow(dead_code)]
pub fn builtin_tool_registry_keys() -> Vec<String> {
    get_builtin_registry().keys().cloned().collect()
}

#[derive(Debug, Deserialize, Clone)]
#[allow(dead_code)]
pub struct ToolManifest {
    pub name: String,
    pub version: String,
    pub description: String,
    pub entry: ManifestEntry,
    pub parameters: Option<Value>,
    pub approval: Option<ManifestApproval>,
    pub sandbox: Option<ManifestSandbox>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ManifestEntry {
    #[serde(rename = "type")]
    pub kind: String,
    pub command: Option<Vec<String>>,
    /// Module path for python_class / rust_type entries.
    pub module: Option<String>,
    /// Class/type name for python_class / rust_type entries.
    pub class: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ManifestApproval {
    pub required: bool,
    pub action: String,
}

#[derive(Debug, Deserialize, Clone)]
#[allow(dead_code)]
pub struct ManifestSandbox {
    pub network: Option<bool>,
    pub filesystem: Option<String>,
    pub max_memory_mb: Option<usize>,
}

/// Load every `manifest.yaml` under `tools_root/<tool-name>/manifest.yaml`.
fn collect_manifests_from_tools_dir(tools_root: &Path) -> Vec<(ToolManifest, PathBuf)> {
    let mut manifests = Vec::new();
    if let Ok(entries) = std::fs::read_dir(tools_root) {
        for entry in entries.flatten() {
            let tool_dir = entry.path();
            let manifest_path = tool_dir.join("manifest.yaml");
            if manifest_path.exists()
                && let Ok(content) = std::fs::read_to_string(&manifest_path)
                && let Ok(manifest) = serde_yaml::from_str::<ToolManifest>(&content)
            {
                manifests.push((manifest, tool_dir));
            }
        }
    }
    manifests
}

/// Discover tool manifests from `~/.kimi/tools/` and `<work_dir>/.kimi/tools/` (§7.1).
///
/// User-level manifests are loaded first; workspace manifests with the same `name` override.
/// Each pair is `(manifest, tool_install_dir)` for subprocess cwd and assets.
/// Results are sorted by `name` for stable registration order.
pub fn discover_manifests(work_dir: &Path) -> Vec<(ToolManifest, PathBuf)> {
    let mut by_name: HashMap<String, (ToolManifest, PathBuf)> = HashMap::new();
    if let Some(home) = dirs::home_dir() {
        let tools_dir = home.join(".kimi").join("tools");
        for pair in collect_manifests_from_tools_dir(&tools_dir) {
            by_name.insert(pair.0.name.clone(), pair);
        }
    }
    let local_tools = work_dir.join(".kimi").join("tools");
    for pair in collect_manifests_from_tools_dir(&local_tools) {
        by_name.insert(pair.0.name.clone(), pair);
    }
    let mut out: Vec<_> = by_name.into_values().collect();
    out.sort_by(|a, b| a.0.name.cmp(&b.0.name));
    out
}

pub struct ManifestTool {
    manifest: ToolManifest,
    dir: PathBuf,
}

impl ManifestTool {
    pub fn new(manifest: ToolManifest, dir: PathBuf) -> Self {
        Self { manifest, dir }
    }
}

#[async_trait]
impl Tool for ManifestTool {
    fn name(&self) -> &str {
        &self.manifest.name
    }

    fn description(&self) -> &str {
        &self.manifest.description
    }

    fn schema(&self) -> Value {
        self.manifest
            .parameters
            .clone()
            .unwrap_or_else(|| serde_json::json!({ "type": "object", "properties": {} }))
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        if let Some(ref approval) = self.manifest.approval
            && approval.required
        {
            let approved = ctx
                .runtime
                .approval
                .request_tool(
                    "".to_string(),
                    &self.manifest.name,
                    &args,
                    format!("Run {} tool", self.manifest.name),
                    format!("Run {} tool", self.manifest.name),
                )
                .await?;
            if !approved {
                return Err(crate::tools::ToolRejected {
                    reason: "Approval rejected".to_string(),
                    has_feedback: false,
                }.into());
            }
        }

        match self.manifest.entry.kind.as_str() {
            "python_class" | "rust_type" => {
                // Look up built-in tool by module/class registry
                let module = self.manifest.entry.module.as_deref().unwrap_or("");
                let class = self.manifest.entry.class.as_deref().unwrap_or("");
                let key = format!("{}::{}", module, class);
                match lookup_builtin_tool(&key) {
                    Some(tool) => {
                        // Delegate to the built-in tool, but wrap result with manifest identity
                        tool.call(args, ctx).await
                    }
                    None => anyhow::bail!(
                        "Built-in tool not found for manifest {} (module={}, class={}). \
                        Registered keys: {:?}",
                        self.manifest.name,
                        module,
                        class,
                        builtin_tool_registry_keys()
                    ),
                }
            }
            "subprocess" => {
                let mut cmd = if let Some(ref command) = self.manifest.entry.command {
                    let mut c = Command::new(&command[0]);
                    for arg in &command[1..] {
                        c.arg(arg);
                    }
                    c
                } else {
                    anyhow::bail!("No command specified in manifest");
                };
                cmd.current_dir(&self.dir);
                cmd.stdin(std::process::Stdio::piped());
                cmd.stdout(std::process::Stdio::piped());
                cmd.stderr(std::process::Stdio::piped());

                // Apply sandbox constraints
                if let Some(ref sandbox) = self.manifest.sandbox
                    && sandbox.network == Some(false)
                {
                    // Best-effort: on macOS use sandbox-exec, on Linux use unshare
                    #[cfg(target_os = "macos")]
                    {
                        // sandbox-exec would go here; for now we log a warning
                        tracing::warn!(
                            "Network sandbox requested but not enforced on this platform"
                        );
                    }
                }

                let mut child = cmd.spawn()?;
                if let Some(stdin) = child.stdin.take() {
                    let mut writer = tokio::io::BufWriter::new(stdin);
                    writer.write_all(args.to_string().as_bytes()).await?;
                    writer.flush().await?;
                    // drop writer to close stdin
                }
                let output = child.wait_with_output().await?;
                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                let text = format!("{}{}", stdout, stderr);
                let success = output.status.success();
                Ok(ToolOutput {
                    result: ToolResult {
                        r#type: if success {
                            "success".to_string()
                        } else {
                            "error".to_string()
                        },
                        content: vec![ContentBlock::Text { text }],
                        summary: if success {
                            "Done".to_string()
                        } else {
                            "Failed".to_string()
                        },
                    },
                    artifacts: vec![],
                    metrics: ToolMetrics {
                        elapsed_ms: 0,
                        exit_code: output.status.code(),
                    },
                })
            }
            _ => anyhow::bail!(
                "Unsupported manifest entry type: {}",
                self.manifest.entry.kind
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[tokio::test]
    async fn test_manifest_tool_subprocess_echo() {
        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("echo.sh");
        {
            let mut f = std::fs::File::create(&script).unwrap();
            writeln!(f, "#!/bin/sh").unwrap();
            writeln!(f, "cat").unwrap();
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&script).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script, perms).unwrap();
        }

        let manifest = ToolManifest {
            name: "echo_manifest".to_string(),
            version: "1.0.0".to_string(),
            description: "Echo test".to_string(),
            entry: ManifestEntry {
                kind: "subprocess".to_string(),
                command: Some(vec![script.to_string_lossy().to_string()]),
                module: None,
                class: None,
            },
            parameters: None,
            approval: None,
            sandbox: None,
        };

        let tool = ManifestTool::new(manifest, temp.path().to_path_buf());
        let store = crate::store::Store::open(std::path::Path::new(":memory:")).unwrap();
        let ctx = ToolContext {
            runtime: crate::runtime::Runtime::new(
                crate::config::Config::default(),
                crate::session::Session::create(&store, std::env::current_dir().unwrap()).unwrap(),
                std::sync::Arc::new(crate::approval::ApprovalRuntime::new(
                    crate::wire::RootWireHub::new(),
                    true,
                    vec![],
                )),
                crate::wire::RootWireHub::new(),
                store,
            ),
            hub: None,
            token: crate::token::ContextToken::new("test", "test-turn"),
        };

        let args = serde_json::json!({ "hello": "world" });
        let output = tool.call(args, &ctx).await.unwrap();
        assert_eq!(output.result.r#type, "success");
        if let ContentBlock::Text { text } = &output.result.content[0] {
            assert!(text.contains("hello"));
        }
    }

    #[test]
    fn test_manifest_tool_schema_default() {
        let manifest = ToolManifest {
            name: "test".to_string(),
            version: "1.0".to_string(),
            description: "desc".to_string(),
            entry: ManifestEntry {
                kind: "subprocess".to_string(),
                command: None,
                module: None,
                class: None,
            },
            parameters: None,
            approval: None,
            sandbox: None,
        };
        let tool = ManifestTool::new(manifest, std::path::PathBuf::from("."));
        let schema = tool.schema();
        assert_eq!(schema["type"], "object");
    }

    #[test]
    fn test_collect_manifests_from_tools_dir_reads_yaml() {
        let tmp = tempfile::tempdir().unwrap();
        let tool_dir = tmp.path().join("hello_tool");
        std::fs::create_dir_all(&tool_dir).unwrap();
        let yaml = r#"
name: hello_tool
version: "0.1.0"
description: Hi
entry:
  type: rust_type
  module: rki_rs::tools::misc
  class: ThinkTool
"#;
        std::fs::write(tool_dir.join("manifest.yaml"), yaml).unwrap();
        let found = collect_manifests_from_tools_dir(tmp.path());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0.name, "hello_tool");
    }

    #[test]
    fn test_discover_manifests_does_not_panic_on_empty_workdir() {
        let tmp = tempfile::tempdir().unwrap();
        let _ = discover_manifests(tmp.path());
    }

    #[tokio::test]
    async fn test_manifest_tool_unsupported_entry_type() {
        let manifest = ToolManifest {
            name: "bad".to_string(),
            version: "1.0".to_string(),
            description: "desc".to_string(),
            entry: ManifestEntry {
                kind: "wasm".to_string(),
                command: None,
                module: None,
                class: None,
            },
            parameters: None,
            approval: None,
            sandbox: None,
        };
        let tool = ManifestTool::new(manifest, std::path::PathBuf::from("."));
        let store = crate::store::Store::open(std::path::Path::new(":memory:")).unwrap();
        let ctx = ToolContext {
            runtime: crate::runtime::Runtime::new(
                crate::config::Config::default(),
                crate::session::Session::create(&store, std::env::current_dir().unwrap()).unwrap(),
                std::sync::Arc::new(crate::approval::ApprovalRuntime::new(
                    crate::wire::RootWireHub::new(),
                    true,
                    vec![],
                )),
                crate::wire::RootWireHub::new(),
                store,
            ),
            hub: None,
            token: crate::token::ContextToken::new("test", "test-turn"),
        };
        let result = tool.call(serde_json::json!({}), &ctx).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Unsupported"));
    }

    #[tokio::test]
    async fn test_manifest_tool_rust_type_lookup() {
        let manifest = ToolManifest {
            name: "think_via_manifest".to_string(),
            version: "1.0".to_string(),
            description: "Think tool via manifest registry".to_string(),
            entry: ManifestEntry {
                kind: "rust_type".to_string(),
                command: None,
                module: Some("rki_rs::tools::misc".to_string()),
                class: Some("ThinkTool".to_string()),
            },
            parameters: None,
            approval: None,
            sandbox: None,
        };
        let tool = ManifestTool::new(manifest, std::path::PathBuf::from("."));
        let store = crate::store::Store::open(std::path::Path::new(":memory:")).unwrap();
        let ctx = ToolContext {
            runtime: crate::runtime::Runtime::new(
                crate::config::Config::default(),
                crate::session::Session::create(&store, std::env::current_dir().unwrap()).unwrap(),
                std::sync::Arc::new(crate::approval::ApprovalRuntime::new(
                    crate::wire::RootWireHub::new(),
                    true,
                    vec![],
                )),
                crate::wire::RootWireHub::new(),
                store,
            ),
            hub: None,
            token: crate::token::ContextToken::new("test", "test-turn"),
        };
        let args = serde_json::json!({ "thought": "I am thinking via manifest" });
        let output = tool.call(args, &ctx).await.unwrap();
        assert_eq!(output.result.r#type, "success");
        assert!(output.result.summary.contains("Thought"));
    }

    #[tokio::test]
    async fn test_manifest_tool_rust_type_missing() {
        let manifest = ToolManifest {
            name: "missing".to_string(),
            version: "1.0".to_string(),
            description: "desc".to_string(),
            entry: ManifestEntry {
                kind: "rust_type".to_string(),
                command: None,
                module: Some("unknown".to_string()),
                class: Some("UnknownTool".to_string()),
            },
            parameters: None,
            approval: None,
            sandbox: None,
        };
        let tool = ManifestTool::new(manifest, std::path::PathBuf::from("."));
        let store = crate::store::Store::open(std::path::Path::new(":memory:")).unwrap();
        let ctx = ToolContext {
            runtime: crate::runtime::Runtime::new(
                crate::config::Config::default(),
                crate::session::Session::create(&store, std::env::current_dir().unwrap()).unwrap(),
                std::sync::Arc::new(crate::approval::ApprovalRuntime::new(
                    crate::wire::RootWireHub::new(),
                    true,
                    vec![],
                )),
                crate::wire::RootWireHub::new(),
                store,
            ),
            hub: None,
            token: crate::token::ContextToken::new("test", "test-turn"),
        };
        let result = tool.call(serde_json::json!({}), &ctx).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }
}
