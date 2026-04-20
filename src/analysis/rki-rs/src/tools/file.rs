use crate::tools::{ContentBlock, Tool, ToolContext, ToolMetrics, ToolOutput, ToolResult};
use async_trait::async_trait;
use serde_json::Value;
use tokio::io::AsyncWriteExt;

pub struct ReadFileTool;

pub struct ReadMediaFileTool;

#[async_trait]
impl Tool for ReadMediaFileTool {
    fn name(&self) -> &str {
        "read_media_file"
    }
    fn description(&self) -> &str {
        "Read an image or video file and return it as a data URL"
    }
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" }
            },
            "required": ["path"]
        })
    }

    async fn call(&self, args: Value, _ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
        let meta = tokio::fs::metadata(path).await?;
        let max_size = 100 * 1024 * 1024; // 100MB
        if meta.len() > max_size as u64 {
            anyhow::bail!("File too large: {} bytes (max {})", meta.len(), max_size);
        }
        let data = tokio::fs::read(path).await?;
        let ext = std::path::Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
        let mime = match ext.to_lowercase().as_str() {
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "gif" => "image/gif",
            "webp" => "image/webp",
            "svg" => "image/svg+xml",
            "mp4" => "video/mp4",
            "webm" => "video/webm",
            "mov" => "video/quicktime",
            "mp3" => "audio/mpeg",
            "wav" => "audio/wav",
            "ogg" => "audio/ogg",
            _ => "application/octet-stream",
        };
        let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &data);
        let data_url = format!("data:{};base64,{}", mime, b64);
        Ok(ToolOutput {
            result: ToolResult {
                r#type: "success".to_string(),
                content: vec![ContentBlock::Text { text: data_url }],
                summary: format!("Read {} bytes of {}", data.len(), mime),
            },
            artifacts: vec![],
            metrics: ToolMetrics::default(),
        })
    }
}

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }
    fn description(&self) -> &str {
        "Read contents of a file"
    }
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "line_offset": { "type": "integer", "default": 1 },
                "n_lines": { "type": "integer", "default": 1000 }
            },
            "required": ["path"]
        })
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
        let approved = ctx
            .runtime
            .approval
            .request_tool(
                "".to_string(),
                "str_replace_file",
                &args,
                format!("Edit file {}", path),
                format!("Replace text in {}", path),
            )
            .await?;
        if !approved {
            return Ok(ToolOutput {
                result: ToolResult {
                    r#type: "error".to_string(),
                    content: vec![ContentBlock::Text {
                        text: "Approval rejected".to_string(),
                    }],
                    summary: "Approval rejected".to_string(),
                },
                artifacts: vec![],
                metrics: ToolMetrics::default(),
            });
        }
        let line_offset = args
            .get("line_offset")
            .and_then(|v| v.as_i64())
            .unwrap_or(1);
        let n_lines = args.get("n_lines").and_then(|v| v.as_u64()).unwrap_or(1000) as usize;
        let content = tokio::fs::read_to_string(path).await?;
        let lines: Vec<&str> = content.lines().collect();
        let start = if line_offset < 0 {
            lines.len().saturating_sub((-line_offset) as usize)
        } else {
            (line_offset as usize).saturating_sub(1)
        };
        let end = (start + n_lines).min(lines.len());
        let selected = lines[start..end].join("\n");
        Ok(ToolOutput {
            result: ToolResult {
                r#type: "success".to_string(),
                content: vec![ContentBlock::Text { text: selected }],
                summary: format!("Read {} lines", end - start),
            },
            artifacts: vec![],
            metrics: ToolMetrics::default(),
        })
    }
}

pub struct WriteFileTool;

#[async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> &str {
        "write_file"
    }
    fn description(&self) -> &str {
        "Write or append to a file"
    }
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "content": { "type": "string" },
                "mode": { "type": "string", "enum": ["overwrite", "append"], "default": "overwrite" }
            },
            "required": ["path", "content"]
        })
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
        let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");
        let mode = args
            .get("mode")
            .and_then(|v| v.as_str())
            .unwrap_or("overwrite");
        let approved = ctx
            .runtime
            .approval
            .request_tool(
                "".to_string(),
                "write_file",
                &args,
                format!("{} file {}", mode, path),
                format!("{} {} bytes to {}", mode, content.len(), path),
            )
            .await?;
        if !approved {
            return Ok(ToolOutput {
                result: ToolResult {
                    r#type: "error".to_string(),
                    content: vec![ContentBlock::Text {
                        text: "Approval rejected".to_string(),
                    }],
                    summary: "Approval rejected".to_string(),
                },
                artifacts: vec![],
                metrics: ToolMetrics::default(),
            });
        }
        // Capture before state for diff (§7.3 structured output)
        let before = tokio::fs::read_to_string(path).await.unwrap_or_default();
        let mut open = tokio::fs::OpenOptions::new();
        open.write(true).create(true);
        if mode == "append" {
            open.append(true);
        } else {
            open.truncate(true);
        }
        let mut file = open.open(path).await?;
        file.write_all(content.as_bytes()).await?;
        file.flush().await?;
        let _ = file.sync_all().await;
        let after = if mode == "append" {
            format!("{}{}", before, content)
        } else {
            content.to_string()
        };
        Ok(ToolOutput {
            result: ToolResult {
                r#type: "success".to_string(),
                content: vec![
                    ContentBlock::Diff { before, after },
                    ContentBlock::Text {
                        text: format!("Wrote {} bytes to {}", content.len(), path),
                    },
                ],
                summary: "File written".to_string(),
            },
            artifacts: vec![],
            metrics: ToolMetrics::default(),
        })
    }
}

pub struct StrReplaceFileTool;

#[async_trait]
impl Tool for StrReplaceFileTool {
    fn name(&self) -> &str {
        "str_replace_file"
    }
    fn description(&self) -> &str {
        "Replace strings in a file"
    }
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "edit": {
                    "type": "object",
                    "properties": {
                        "old": { "type": "string" },
                        "new": { "type": "string" },
                        "replace_all": { "type": "boolean", "default": false }
                    },
                    "required": ["old", "new"]
                }
            },
            "required": ["path", "edit"]
        })
    }

    async fn call(&self, args: Value, _ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
        let edit = args
            .get("edit")
            .ok_or_else(|| anyhow::anyhow!("Missing edit"))?;
        let old = edit.get("old").and_then(|v| v.as_str()).unwrap_or("");
        let new = edit.get("new").and_then(|v| v.as_str()).unwrap_or("");
        let replace_all = edit
            .get("replace_all")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let before = tokio::fs::read_to_string(path).await?;
        let after = if replace_all {
            before.replace(old, new)
        } else {
            before.replacen(old, new, 1)
        };
        tokio::fs::write(path, after.as_bytes()).await?;
        Ok(ToolOutput {
            result: ToolResult {
                r#type: "success".to_string(),
                content: vec![
                    ContentBlock::Diff { before, after },
                    ContentBlock::Text {
                        text: format!("Replaced in {}", path),
                    },
                ],
                summary: "Replacement done".to_string(),
            },
            artifacts: vec![],
            metrics: ToolMetrics::default(),
        })
    }
}

pub struct GlobTool;

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &str {
        "glob"
    }
    fn description(&self) -> &str {
        "Find files by glob pattern"
    }
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string" },
                "directory": { "type": "string" },
                "include_dirs": { "type": "boolean", "default": false }
            },
            "required": ["pattern"]
        })
    }

    async fn call(&self, args: Value, _ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        let pattern = args.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
        let dir = args
            .get("directory")
            .and_then(|v| v.as_str())
            .map(std::path::PathBuf::from);
        let include_dirs = args
            .get("include_dirs")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let regex = regex::escape(pattern)
            .replace("\\*", ".*")
            .replace("\\?", ".");
        let re = regex::Regex::new(&format!("^{}$", regex))?;
        let root = dir.unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        let mut results = Vec::new();
        for entry in walkdir::WalkDir::new(root).max_depth(10) {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();
            if re.is_match(&name) {
                if entry.file_type().is_dir() && !include_dirs {
                    continue;
                }
                results.push(entry.path().to_string_lossy().to_string());
                if results.len() >= 1000 {
                    break;
                }
            }
        }
        Ok(ToolOutput {
            result: ToolResult {
                r#type: "success".to_string(),
                content: vec![ContentBlock::Text {
                    text: results.join("\n"),
                }],
                summary: format!("Found {} matches", results.len()),
            },
            artifacts: vec![],
            metrics: ToolMetrics::default(),
        })
    }
}

pub struct GrepTool;

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }
    fn description(&self) -> &str {
        "Search file contents with regex"
    }
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string" },
                "path": { "type": "string" },
                "glob": { "type": "string" },
                "output_mode": { "type": "string", "enum": ["content", "files_with_matches", "count_matches"], "default": "content" },
                "head_limit": { "type": "integer", "default": 250 },
                "offset": { "type": "integer", "default": 0 }
            },
            "required": ["pattern"]
        })
    }

    async fn call(&self, args: Value, _ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        let pattern = args.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
        let path = args.get("path").and_then(|v| v.as_str());
        let glob = args.get("glob").and_then(|v| v.as_str());
        let output_mode = args
            .get("output_mode")
            .and_then(|v| v.as_str())
            .unwrap_or("content");
        let head_limit = args
            .get("head_limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(250) as usize;
        let offset = args.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        let re = regex::Regex::new(pattern)?;
        let root = path
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        let mut results = Vec::new();
        let mut count = 0usize;
        for entry in walkdir::WalkDir::new(root).max_depth(10) {
            let entry = entry?;
            if !entry.file_type().is_file() {
                continue;
            }
            if let Some(g) = glob
                && !entry.file_name().to_string_lossy().contains(g)
            {
                continue;
            }
            let content = match tokio::fs::read_to_string(entry.path()).await {
                Ok(c) => c,
                Err(_) => continue,
            };
            for (i, line) in content.lines().enumerate() {
                if re.is_match(line) {
                    count += 1;
                    if output_mode == "files_with_matches" {
                        results.push(entry.path().to_string_lossy().to_string());
                        break;
                    } else if output_mode == "content" {
                        results.push(format!("{}:{}:{}", entry.path().display(), i + 1, line));
                        if results.len() >= head_limit + offset {
                            break;
                        }
                    }
                }
            }
            if output_mode == "files_with_matches" && results.len() >= head_limit + offset {
                break;
            }
        }
        let text = if output_mode == "count_matches" {
            format!("{}", count)
        } else {
            results
                .into_iter()
                .skip(offset)
                .take(head_limit)
                .collect::<Vec<_>>()
                .join("\n")
        };
        Ok(ToolOutput {
            result: ToolResult {
                r#type: "success".to_string(),
                content: vec![ContentBlock::Text { text }],
                summary: format!("Matched {} times", count),
            },
            artifacts: vec![],
            metrics: ToolMetrics::default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn test_ctx() -> ToolContext {
        let store = crate::store::Store::open(std::path::Path::new(":memory:")).unwrap();
        ToolContext {
            hub: None,
            token: crate::token::ContextToken::new("test", "test-turn"),
            runtime: crate::runtime::Runtime::new(
                crate::config::Config::default(),
                crate::session::Session::create(&store, std::env::current_dir().unwrap()).unwrap(),
                Arc::new(crate::approval::ApprovalRuntime::new(
                    crate::wire::RootWireHub::new(),
                    true,
                    vec![],
                )),
                crate::wire::RootWireHub::new(),
                store,
            ),
        }
    }

    #[tokio::test]
    async fn test_read_file() {
        let temp = tempfile::NamedTempFile::new().unwrap();
        tokio::fs::write(temp.path(), "line1\nline2\nline3\n")
            .await
            .unwrap();
        let tool = ReadFileTool;
        let args = serde_json::json!({ "path": temp.path().to_str().unwrap() });
        let output = tool.call(args, &test_ctx()).await.unwrap();
        assert_eq!(output.result.r#type, "success");
        if let ContentBlock::Text { text } = &output.result.content[0] {
            assert!(text.contains("line1"));
        } else {
            panic!("Expected Text block");
        }
    }

    #[tokio::test]
    async fn test_write_file_overwrite() {
        let temp = tempfile::NamedTempFile::new().unwrap();
        tokio::fs::write(temp.path(), "old content").await.unwrap();
        let tool = WriteFileTool;
        let args = serde_json::json!({
            "path": temp.path().to_str().unwrap(),
            "content": "new content",
            "mode": "overwrite"
        });
        let output = tool.call(args, &test_ctx()).await.unwrap();
        assert_eq!(output.result.r#type, "success");
        let read = tokio::fs::read_to_string(temp.path()).await.unwrap();
        assert_eq!(read, "new content");
        // §7.3: verify Diff content block is present
        assert!(
            output
                .result
                .content
                .iter()
                .any(|b| matches!(b, ContentBlock::Diff { before, .. } if before == "old content"))
        );
    }

    #[tokio::test]
    async fn test_write_file_append() {
        let temp = tempfile::NamedTempFile::new().unwrap();
        tokio::fs::write(temp.path(), "base").await.unwrap();
        let tool = WriteFileTool;
        let args = serde_json::json!({
            "path": temp.path().to_str().unwrap(),
            "content": "+extra",
            "mode": "append"
        });
        let output = tool.call(args, &test_ctx()).await.unwrap();
        assert_eq!(output.result.r#type, "success");
        let read = tokio::fs::read_to_string(temp.path()).await.unwrap();
        assert_eq!(read, "base+extra");
    }

    #[tokio::test]
    async fn test_str_replace_file_single() {
        let temp = tempfile::NamedTempFile::new().unwrap();
        tokio::fs::write(temp.path(), "hello world hello")
            .await
            .unwrap();
        let tool = StrReplaceFileTool;
        let args = serde_json::json!({
            "path": temp.path().to_str().unwrap(),
            "edit": { "old": "hello", "new": "hi", "replace_all": false }
        });
        let output = tool.call(args, &test_ctx()).await.unwrap();
        assert_eq!(output.result.r#type, "success");
        let read = tokio::fs::read_to_string(temp.path()).await.unwrap();
        assert_eq!(read, "hi world hello");
        // §7.3: verify Diff content block is present
        assert!(output.result.content.iter().any(|b| matches!(b, ContentBlock::Diff { before, after } if before == "hello world hello" && after == "hi world hello")));
    }

    #[tokio::test]
    async fn test_str_replace_file_all() {
        let temp = tempfile::NamedTempFile::new().unwrap();
        tokio::fs::write(temp.path(), "hello world hello")
            .await
            .unwrap();
        let tool = StrReplaceFileTool;
        let args = serde_json::json!({
            "path": temp.path().to_str().unwrap(),
            "edit": { "old": "hello", "new": "hi", "replace_all": true }
        });
        let output = tool.call(args, &test_ctx()).await.unwrap();
        assert_eq!(output.result.r#type, "success");
        let read = tokio::fs::read_to_string(temp.path()).await.unwrap();
        assert_eq!(read, "hi world hi");
    }

    #[tokio::test]
    async fn test_glob_tool() {
        let temp = tempfile::tempdir().unwrap();
        tokio::fs::write(temp.path().join("a.rs"), "")
            .await
            .unwrap();
        tokio::fs::write(temp.path().join("b.rs"), "")
            .await
            .unwrap();
        tokio::fs::write(temp.path().join("c.txt"), "")
            .await
            .unwrap();
        let tool = GlobTool;
        let args = serde_json::json!({
            "pattern": "*.rs",
            "directory": temp.path().to_str().unwrap()
        });
        let output = tool.call(args, &test_ctx()).await.unwrap();
        assert_eq!(output.result.r#type, "success");
        if let ContentBlock::Text { text } = &output.result.content[0] {
            assert!(text.contains("a.rs"));
            assert!(text.contains("b.rs"));
            assert!(!text.contains("c.txt"));
        } else {
            panic!("Expected Text block");
        }
    }

    #[tokio::test]
    async fn test_grep_tool() {
        let temp = tempfile::tempdir().unwrap();
        tokio::fs::write(temp.path().join("a.rs"), "fn main() {}\nfn helper() {}")
            .await
            .unwrap();
        tokio::fs::write(temp.path().join("b.txt"), "fn other() {}")
            .await
            .unwrap();
        let tool = GrepTool;
        let args = serde_json::json!({
            "pattern": "fn main",
            "path": temp.path().to_str().unwrap()
        });
        let output = tool.call(args, &test_ctx()).await.unwrap();
        assert_eq!(output.result.r#type, "success");
        if let ContentBlock::Text { text } = &output.result.content[0] {
            assert!(text.contains("fn main"));
            assert!(!text.contains("helper"));
            assert!(!text.contains("other"));
        } else {
            panic!("Expected Text block");
        }
    }

    #[tokio::test]
    async fn test_read_media_file_png() {
        let temp = tempfile::NamedTempFile::new().unwrap();
        // Write a minimal 1x1 PNG
        let png = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        tokio::fs::write(temp.path(), &png).await.unwrap();
        let mut path = temp.path().to_path_buf();
        path.set_extension("png");
        tokio::fs::rename(temp.path(), &path).await.unwrap();

        let tool = ReadMediaFileTool;
        let args = serde_json::json!({ "path": path.to_str().unwrap() });
        let output = tool.call(args, &test_ctx()).await.unwrap();
        assert_eq!(output.result.r#type, "success");
        if let ContentBlock::Text { text } = &output.result.content[0] {
            assert!(text.starts_with("data:image/png;base64,"));
        } else {
            panic!("Expected Text block");
        }
    }
}
