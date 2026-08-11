//! Sub-agent type definitions (the OpenHands AgentDefinition pattern,
//! adapted to sigil's file layout).
//!
//! A sub-agent type is a Markdown file with a TOML frontmatter block
//! (sigil uses TOML instead of OpenHands' YAML — same idea, no extra
//! dependency):
//!
//! ```text
//! ---
//! description = "researches a topic and returns a structured brief"
//! tools = ["web_search", "read_file"]   # optional; default = all tools
//! ---
//! <Markdown body: the system-prompt suffix for this sub-agent type>
//! ```
//!
//! The body is *appended* to the parent's system prompt (suffix, not
//! replacement — same invariant as prime-agent's immutable base prompt
//! and OpenHands' `system_message_suffix`), so the parent's global
//! rules still apply and the type adds its specialization on top.
//!
//! Discovery directories, highest priority first:
//!   1. `<harness_dir>/agents/`          (project-level)
//!   2. `~/.sigil/agents/`               (user-level fallback)
//!
//! Within one directory first-wins by name; `README.md` is skipped.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// A parsed sub-agent type definition.
#[derive(Debug, Clone)]
pub struct AgentDefinition {
    pub name: String,
    pub description: String,
    /// Tool allowlist; `None` = inherit all tools.
    pub tools: Option<Vec<String>>,
    /// The system-prompt suffix (Markdown body).
    pub prompt: String,
}

/// Parse a `<name>.md` definition file with an optional TOML
/// frontmatter block. Missing frontmatter is fine (empty description,
/// all tools). The file name (minus `.md`) is the type name.
pub fn parse_definition(path: &Path) -> std::io::Result<AgentDefinition> {
    let raw = std::fs::read_to_string(path)?;
    let name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("agent")
        .to_string();

    let (frontmatter, body) = split_frontmatter(&raw);
    let (description, tools) = match frontmatter {
        Some(toml_text) => {
            let val: toml::Value = toml::from_str(toml_text)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
            let desc = val
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let tools = val
                .get("tools")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect::<Vec<_>>()
                })
                .filter(|v| !v.is_empty());
            (desc, tools)
        }
        None => (String::new(), None),
    };

    Ok(AgentDefinition {
        name,
        description,
        tools,
        prompt: body.trim().to_string(),
    })
}

/// Split `---\n...\n---\nbody` into `(Some(frontmatter), body)`.
/// Returns `(None, whole)` when there is no frontmatter block.
fn split_frontmatter(raw: &str) -> (Option<&str>, &str) {
    let trimmed = raw.trim_start_matches('\u{feff}'); // tolerate BOM
    let Some(rest) = trimmed.strip_prefix("---") else {
        return (None, raw);
    };
    let rest = rest.strip_prefix('\n').unwrap_or(rest);
    let Some(end) = rest.find("\n---") else {
        return (None, raw);
    };
    let front = &rest[..end];
    let body = &rest[end + 4..];
    if front.trim().is_empty() {
        (None, body)
    } else {
        (Some(front), body)
    }
}

/// Default user-level agents dir: `~/.sigil/agents/`.
pub fn default_agents_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".sigil")
        .join("agents")
}

/// Load every `*.md` definition from `dir` (skipping `README.md`),
/// keyed by type name. First occurrence wins within the directory.
fn load_dir(dir: &Path, out: &mut HashMap<String, AgentDefinition>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "md"))
        .collect();
    files.sort();
    for path in files {
        if path
            .file_stem()
            .and_then(|s| s.to_str())
            .is_some_and(|s| s.eq_ignore_ascii_case("readme"))
        {
            continue;
        }
        let Ok(def) = parse_definition(&path) else {
            continue;
        };
        out.entry(def.name.clone()).or_insert(def);
    }
}

/// Shared registry of sub-agent types. Project-level definitions
/// (`<harness_dir>/agents/`) take priority over user-level ones
/// (`~/.sigil/agents/`); within a directory first-wins by name.
pub struct AgentRegistry {
    defs: HashMap<String, AgentDefinition>,
}

impl AgentRegistry {
    pub fn new(harness_dir: Option<&Path>) -> Self {
        let mut defs = HashMap::new();
        // User-level first so project-level can shadow it — priority
        // is therefore: project > user (same as OpenHands' project
        // beats user).
        load_dir(&default_agents_dir(), &mut defs);
        if let Some(harness) = harness_dir {
            load_dir(&harness.join("agents"), &mut defs);
        }
        Self { defs }
    }

    pub fn resolve(&self, name: &str) -> Option<&AgentDefinition> {
        self.defs.get(name)
    }

    /// All known types, sorted by name (for the dynamic tool
    /// description and error messages).
    pub fn list(&self) -> Vec<&AgentDefinition> {
        let mut v: Vec<&AgentDefinition> = self.defs.values().collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sigil-agents-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn parses_toml_frontmatter_and_body() {
        let dir = tmp_dir("parse");
        let f = dir.join("researcher.md");
        std::fs::write(
            &f,
            "---\ndescription = \"researches topics\"\ntools = [\"web_search\", \"read_file\"]\n---\n# Researcher\n\nYou dig into questions.\n",
        )
        .unwrap();
        let def = parse_definition(&f).unwrap();
        assert_eq!(def.name, "researcher");
        assert_eq!(def.description, "researches topics");
        assert_eq!(
            def.tools.as_deref(),
            Some(vec!["web_search".to_string(), "read_file".to_string()].as_slice())
        );
        assert_eq!(def.prompt, "# Researcher\n\nYou dig into questions.");
    }

    #[test]
    fn missing_frontmatter_is_fine() {
        let dir = tmp_dir("nofront");
        let f = dir.join("plain.md");
        std::fs::write(&f, "Just a body.\n").unwrap();
        let def = parse_definition(&f).unwrap();
        assert_eq!(def.name, "plain");
        assert_eq!(def.description, "");
        assert!(def.tools.is_none());
        assert_eq!(def.prompt, "Just a body.");
    }

    #[test]
    fn malformed_frontmatter_errors_cleanly() {
        let dir = tmp_dir("bad");
        let f = dir.join("bad.md");
        std::fs::write(&f, "---\nnot = = valid toml\n---\nbody\n").unwrap();
        let err = parse_definition(&f).unwrap_err();
        assert!(err.to_string().contains("expected"), "err: {err}");
    }

    #[test]
    fn registry_project_shadows_user_and_skips_readme() {
        let user = tmp_dir("user");
        let project = tmp_dir("proj");
        std::fs::create_dir_all(project.join("agents")).unwrap();
        std::fs::write(
            user.join("coder.md"),
            "---\ndescription = \"user coder\"\n---\nUser body\n",
        )
        .unwrap();
        // Project shadows the user-level type of the same name.
        std::fs::write(
            project.join("agents").join("coder.md"),
            "---\ndescription = \"project coder\"\n---\nProject body\n",
        )
        .unwrap();
        // Project-only type.
        std::fs::write(
            project.join("agents").join("researcher.md"),
            "---\ntools = [\"web_search\"]\n---\nResearch body\n",
        )
        .unwrap();
        // README.md must be skipped.
        std::fs::write(project.join("agents").join("README.md"), "docs\n").unwrap();

        let reg = AgentRegistry::new(Some(&project));
        let coder = reg.resolve("coder").unwrap();
        assert_eq!(coder.description, "project coder", "project shadows user");
        assert_eq!(coder.prompt, "Project body");
        let res = reg.resolve("researcher").unwrap();
        assert_eq!(
            res.tools.as_deref(),
            Some(vec!["web_search".to_string()].as_slice())
        );
        assert!(reg.resolve("README").is_none(), "README.md skipped");
        assert_eq!(reg.list().len(), 2);
    }

    #[test]
    fn empty_tools_list_means_inherit_all() {
        let dir = tmp_dir("emptytools");
        let f = dir.join("generalist.md");
        std::fs::write(&f, "---\ntools = []\n---\nbody\n").unwrap();
        let def = parse_definition(&f).unwrap();
        assert!(def.tools.is_none(), "empty allowlist = inherit all");
    }

    #[test]
    fn split_frontmatter_handles_no_delimiter() {
        let (fm, body) = split_frontmatter("no frontmatter here\n---\nnot a block");
        assert!(fm.is_none());
        assert_eq!(body, "no frontmatter here\n---\nnot a block");
    }
}
