//! Working-directory listing for system prompt injection (§1.2 L07).
//!
//! Shallow tree snapshot: top-level entries plus one level under each immediate subdirectory
//! (bounded), similar in spirit to Python `KIMI_WORK_DIR_LS` style hints.

use std::path::Path;

const MAX_TOP_LEVEL: usize = 80;
const MAX_SUBDIRS: usize = 10;
const MAX_PER_SUBDIR: usize = 24;

async fn list_one_level(dir: &Path, cap: usize) -> String {
    let mut rd = match tokio::fs::read_dir(dir).await {
        Ok(r) => r,
        Err(e) => return format!("    (list error: {})\n", e),
    };
    let mut names: Vec<String> = Vec::new();
    while let Ok(Some(e)) = rd.next_entry().await {
        names.push(e.file_name().to_string_lossy().into_owned());
    }
    names.sort_by(|a, b| a.to_lowercase().cmp(&b.to_lowercase()));
    let mut out = String::new();
    for (i, name) in names.into_iter().enumerate() {
        if i >= cap {
            out.push_str("    … (truncated)\n");
            break;
        }
        out.push_str(&format!("    {}\n", name));
    }
    out
}

/// Build a fenced, human-readable tree of `work_dir`.
///
/// - `max_depth == 0`: empty string.
/// - `max_depth == 1`: only top-level names under `work_dir`.
/// - `max_depth >= 2`: top level plus bounded listing under up to [`MAX_SUBDIRS`] immediate subdirectories.
pub async fn format_work_dir_tree(work_dir: &Path, max_depth: u8) -> String {
    if max_depth == 0 {
        return String::new();
    }

    let mut out = String::new();
    out.push_str("```text\n");
    if !work_dir.exists() {
        out.push_str(&format!("(missing: {})\n", work_dir.display()));
        out.push_str("```\n");
        return out;
    }

    out.push_str(&format!("{}\n", work_dir.display()));

    let mut rd = match tokio::fs::read_dir(work_dir).await {
        Ok(r) => r,
        Err(e) => {
            out.push_str(&format!("(list error: {})\n", e));
            out.push_str("```\n");
            return out;
        }
    };

    let mut rows: Vec<(String, bool)> = Vec::new();
    while let Ok(Some(e)) = rd.next_entry().await {
        let name = e.file_name().to_string_lossy().into_owned();
        let is_dir = e.file_type().await.map(|t| t.is_dir()).unwrap_or(false);
        rows.push((name, is_dir));
    }
    rows.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));

    let mut subdirs: Vec<String> = Vec::new();
    for (i, (name, is_dir)) in rows.iter().enumerate() {
        if i >= MAX_TOP_LEVEL {
            out.push_str("… (top-level truncated)\n");
            break;
        }
        let kind = if *is_dir { "[dir]" } else { "[file]" };
        out.push_str(&format!("  {} {}\n", kind, name));
        if max_depth >= 2 && *is_dir && subdirs.len() < MAX_SUBDIRS {
            subdirs.push(name.clone());
        }
    }

    if max_depth >= 2 {
        for sub in subdirs {
            let p = work_dir.join(&sub);
            out.push_str(&format!("  -- {}/\n", sub));
            out.push_str(&list_one_level(&p, MAX_PER_SUBDIR).await);
        }
    }

    out.push_str("```\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_format_work_dir_tree_depth1() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("alpha.txt"), "x").unwrap();
        let s = format_work_dir_tree(temp.path(), 1).await;
        assert!(s.contains("```text"));
        assert!(s.contains("alpha.txt"));
        assert!(s.contains("[file]"));
    }

    #[tokio::test]
    async fn test_format_work_dir_tree_depth2_lists_subdir() {
        let temp = tempfile::tempdir().unwrap();
        let sub = temp.path().join("pkg");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("mod.rs"), "").unwrap();
        let s = format_work_dir_tree(temp.path(), 2).await;
        assert!(s.contains("-- pkg/"));
        assert!(s.contains("mod.rs"));
    }

    #[tokio::test]
    async fn test_format_work_dir_tree_zero_depth_empty() {
        let temp = tempfile::tempdir().unwrap();
        let s = format_work_dir_tree(temp.path(), 0).await;
        assert!(s.is_empty());
    }
}
