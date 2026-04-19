//! AGENTS.md discovery and loading.
//!
//! Scans the working directory tree for `AGENTS.md` files and concatenates
//! them root-to-leaf.

use std::path::{Path, PathBuf};

/// Discover AGENTS.md files from `work_dir` up to filesystem root.
/// Returns concatenated content, ordered from root → leaf so deeper
/// directories take precedence (appear later).
pub async fn discover(work_dir: &Path) -> String {
    let mut paths = Vec::new();
    let mut current = Some(work_dir.to_path_buf());

    while let Some(dir) = current {
        let candidate = dir.join("AGENTS.md");
        if candidate.is_file() {
            paths.push(candidate);
        }
        current = dir.parent().map(PathBuf::from);
    }

    // Reverse so root comes first, leaf comes last
    paths.reverse();

    let mut parts = Vec::new();
    for path in paths {
        match tokio::fs::read_to_string(&path).await {
            Ok(content) => {
                parts.push(format!(
                    "<!-- AGENTS.md from {} -->\n{}",
                    path.parent().unwrap_or(Path::new(".")).display(),
                    content
                ));
            }
            Err(e) => {
                tracing::warn!("Failed to read {}: {}", path.display(), e);
            }
        }
    }

    parts.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[tokio::test]
    async fn test_discover_single_agents_md() {
        let temp = tempfile::tempdir().unwrap();
        let agents = temp.path().join("AGENTS.md");
        let mut file = std::fs::File::create(&agents).unwrap();
        writeln!(file, "# Test Agent").unwrap();

        let content = discover(temp.path()).await;
        assert!(content.contains("# Test Agent"));
        assert!(content.contains("AGENTS.md from"));
    }

    #[tokio::test]
    async fn test_discover_nested_agents_md() {
        let temp = tempfile::tempdir().unwrap();
        let root_agents = temp.path().join("AGENTS.md");
        let mut file = std::fs::File::create(&root_agents).unwrap();
        writeln!(file, "# Root").unwrap();

        let sub = temp.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        let sub_agents = sub.join("AGENTS.md");
        let mut file = std::fs::File::create(&sub_agents).unwrap();
        writeln!(file, "# Sub").unwrap();

        let content = discover(&sub).await;
        // Root should come first, then sub
        let root_pos = content.find("# Root").unwrap();
        let sub_pos = content.find("# Sub").unwrap();
        assert!(root_pos < sub_pos, "Root should come before sub");
    }

    #[tokio::test]
    async fn test_discover_no_agents_md() {
        let temp = tempfile::tempdir().unwrap();
        let content = discover(temp.path()).await;
        assert!(content.is_empty());
    }

    #[tokio::test]
    async fn test_discover_deeply_nested() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("AGENTS.md");
        let mut file = std::fs::File::create(&root).unwrap();
        writeln!(file, "# Root").unwrap();

        let mid = temp.path().join("a").join("b");
        std::fs::create_dir_all(&mid).unwrap();
        let mid_agents = mid.join("AGENTS.md");
        let mut file = std::fs::File::create(&mid_agents).unwrap();
        writeln!(file, "# Mid").unwrap();

        let leaf = temp.path().join("a").join("b").join("c");
        std::fs::create_dir_all(&leaf).unwrap();
        let leaf_agents = leaf.join("AGENTS.md");
        let mut file = std::fs::File::create(&leaf_agents).unwrap();
        writeln!(file, "# Leaf").unwrap();

        let content = discover(&leaf).await;
        let root_pos = content.find("# Root").unwrap();
        let mid_pos = content.find("# Mid").unwrap();
        let leaf_pos = content.find("# Leaf").unwrap();
        assert!(root_pos < mid_pos);
        assert!(mid_pos < leaf_pos);
    }

    #[tokio::test]
    async fn test_discover_parent_without_agents_md() {
        let temp = tempfile::tempdir().unwrap();
        let sub = temp.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        let agents = sub.join("AGENTS.md");
        let mut file = std::fs::File::create(&agents).unwrap();
        writeln!(file, "# Only Sub").unwrap();

        let content = discover(&sub).await;
        assert!(content.contains("# Only Sub"));
        // Should not contain anything from parent since no AGENTS.md there
        let count = content.matches("AGENTS.md from").count();
        assert_eq!(count, 1);
    }
}
