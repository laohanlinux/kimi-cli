//! Skill discovery from package defaults and user directories.
//!
//! Scans `~/.kimi/skills/`, `.kimi/skills/`, and bundled skill roots.

use std::path::{Path, PathBuf};

/// A discovered skill with its metadata.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Skill {
    pub name: String,
    pub path: PathBuf,
    pub content: String,
}

/// Discover skills from configured roots:
/// 1. ~/.kimi/skills/
/// 2. <work_dir>/.kimi/skills/
/// 3. Package defaults (if any)
pub async fn discover_skills(work_dir: &Path) -> Vec<Skill> {
    let mut roots = Vec::new();

    // User-wide skills
    if let Some(home) = dirs::home_dir() {
        roots.push(home.join(".kimi").join("skills"));
    }

    // Project-local skills
    roots.push(work_dir.join(".kimi").join("skills"));

    let mut skills = Vec::new();
    for root in roots {
        if !root.is_dir() {
            continue;
        }
        let mut entries = match tokio::fs::read_dir(&root).await {
            Ok(e) => e,
            Err(_) => continue,
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let skill_md = path.join("SKILL.md");
            if !skill_md.is_file() {
                continue;
            }
            let name = path.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();
            match tokio::fs::read_to_string(&skill_md).await {
                Ok(content) => {
                    skills.push(Skill { name, path, content });
                }
                Err(e) => {
                    tracing::warn!("Failed to read {}: {}", skill_md.display(), e);
                }
            }
        }
    }

    skills
}

/// Build a markdown section listing discovered skills for the system prompt (§1.2, `KIMI_SKILLS`).
pub fn skills_prompt_section(skills: &[Skill]) -> Option<String> {
    if skills.is_empty() {
        return None;
    }
    let mut s = String::from("## Discovered skills\n\n");
    for sk in skills {
        let hint = sk
            .content
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("(see SKILL.md)");
        s.push_str(&format!(
            "- **{}** — {}\n  Users can inject this skill with `/skill:{}`.\n",
            sk.name, hint, sk.name
        ));
    }
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[tokio::test]
    async fn test_discover_skills_from_work_dir() {
        let temp = tempfile::tempdir().unwrap();
        let skills_dir = temp.path().join(".kimi").join("skills");
        let skill_dir = skills_dir.join("doc");
        std::fs::create_dir_all(&skill_dir).unwrap();
        let mut file = std::fs::File::create(skill_dir.join("SKILL.md")).unwrap();
        writeln!(file, "# Doc Skill").unwrap();

        let skills = discover_skills(temp.path()).await;
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "doc");
        assert!(skills[0].content.contains("# Doc Skill"));
    }

    #[tokio::test]
    async fn test_discover_skills_ignores_missing_skill_md() {
        let temp = tempfile::tempdir().unwrap();
        let skills_dir = temp.path().join(".kimi").join("skills");
        let skill_dir = skills_dir.join("incomplete");
        std::fs::create_dir_all(&skill_dir).unwrap();
        // No SKILL.md

        let skills = discover_skills(temp.path()).await;
        assert!(skills.is_empty());
    }

    #[tokio::test]
    async fn test_discover_skills_multiple() {
        let temp = tempfile::tempdir().unwrap();
        let skills_dir = temp.path().join(".kimi").join("skills");
        for name in &["rust", "python"] {
            let dir = skills_dir.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            let mut file = std::fs::File::create(dir.join("SKILL.md")).unwrap();
            writeln!(file, "# {} Skill", name).unwrap();
        }

        let skills = discover_skills(temp.path()).await;
        assert_eq!(skills.len(), 2);
        let names: Vec<_> = skills.iter().map(|s| s.name.clone()).collect();
        assert!(names.contains(&"rust".to_string()));
        assert!(names.contains(&"python".to_string()));
    }

    #[tokio::test]
    async fn test_discover_skills_ignores_files() {
        let temp = tempfile::tempdir().unwrap();
        let skills_dir = temp.path().join(".kimi").join("skills");
        std::fs::create_dir_all(&skills_dir).unwrap();
        let mut file = std::fs::File::create(skills_dir.join("not_a_dir.md")).unwrap();
        writeln!(file, "oops").unwrap();

        let skills = discover_skills(temp.path()).await;
        assert!(skills.is_empty());
    }

    #[tokio::test]
    async fn test_discover_skills_no_skills_dir() {
        let temp = tempfile::tempdir().unwrap();
        let skills = discover_skills(temp.path()).await;
        assert!(skills.is_empty());
    }

    #[tokio::test]
    async fn test_skills_prompt_section_lists_names() {
        let temp = tempfile::tempdir().unwrap();
        let skills_dir = temp.path().join(".kimi").join("skills");
        let skill_dir = skills_dir.join("lint");
        std::fs::create_dir_all(&skill_dir).unwrap();
        let mut file = std::fs::File::create(skill_dir.join("SKILL.md")).unwrap();
        writeln!(file).unwrap();
        writeln!(file, "# Lint skill").unwrap();
        writeln!(file, "Run linters.").unwrap();

        let skills = discover_skills(temp.path()).await;
        let block = skills_prompt_section(&skills).expect("section");
        assert!(block.contains("## Discovered skills"));
        assert!(block.contains("**lint**"));
        assert!(block.contains("/skill:lint"));
        assert!(block.contains("# Lint skill"));
    }

    #[test]
    fn test_skills_prompt_section_empty() {
        assert!(skills_prompt_section(&[]).is_none());
    }
}
