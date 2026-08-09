//! Harness bundle — the externally-editable layer of sigil's brain.
//!
//! Everything the model sees (besides tool schemas, which are generated
//! live from the registry) lives in `~/.sigil/harness/`:
//!
//! ```text
//! ~/.sigil/
//! ├── config.toml          # LLM + agent configuration
//! ├── harness/
//! │   ├── system_prompt.md # the system prompt (default if absent)
//! │   └── skills/          # skill markdown files, listed in the prompt
//! └── tools/               # Lua scripted tools (Phase 2)
//! ```
//!
//! This is the "evolve without recompiling" layer: editing a markdown
//! file changes the agent's behavior instantly. Phase-3 evolution
//! (the AHE loop) treats these files as its mutation surface.

use std::path::{Path, PathBuf};

use anyhow::Result;

/// A skill: a markdown file with a short description in its header.
#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    /// Path to the skill file (for tools that read/edit it).
    #[allow(dead_code)]
    pub path: PathBuf,
}

/// The harness bundle loaded from disk.
#[derive(Debug, Clone)]
pub struct Harness {
    pub system_prompt: String,
    pub skills: Vec<Skill>,
    /// Absolute paths to the harness dirs (for AHE tools that edit them).
    #[allow(dead_code)]
    pub harness_dir: PathBuf,
    #[allow(dead_code)]
    pub skills_dir: PathBuf,
}

pub fn default_harness_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("~"))
        .join("sigil")
        .join("harness")
}

impl Harness {
    /// Load the harness bundle from disk, creating defaults if absent.
    pub fn load(harness_dir: Option<&Path>) -> Result<Self> {
        let dir = harness_dir
            .map(Path::to_path_buf)
            .unwrap_or_else(default_harness_dir);
        let skills_dir = dir.join("skills");
        std::fs::create_dir_all(&skills_dir)?;

        let prompt_path = dir.join("system_prompt.md");
        let system_prompt = match std::fs::read_to_string(&prompt_path) {
            Ok(text) => text,
            Err(_) => {
                // First run: write the default prompt so the user can
                // edit it and see the effect immediately.
                std::fs::write(&prompt_path, DEFAULT_SYSTEM_PROMPT)?;
                DEFAULT_SYSTEM_PROMPT.to_string()
            }
        };

        let skills = load_skills(&skills_dir);

        Ok(Self {
            system_prompt,
            skills,
            harness_dir: dir,
            skills_dir,
        })
    }

    /// Render the skill index section for the system prompt.
    pub fn skills_index(&self) -> String {
        if self.skills.is_empty() {
            return String::new();
        }
        let mut out = String::from("\n\n# Available skills\n\n");
        for s in &self.skills {
            out.push_str(&format!("- **{}** — {}\n", s.name, s.description));
        }
        out.push_str("\nTo use a skill, read it with the read_file tool before acting.");
        out
    }

    /// Full system prompt: base + skill index.
    pub fn full_system_prompt(&self) -> String {
        let mut p = self.system_prompt.clone();
        p.push_str(&self.skills_index());
        p
    }
}

fn load_skills(dir: &Path) -> Vec<Skill> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut skills = Vec::new();
    for e in entries.flatten() {
        let path = e.path();
        if path.extension().and_then(|x| x.to_str()) != Some("md") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        // Description: first line starting with `> ` (blockquote).
        let description = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| {
                text.lines()
                    .find_map(|l| l.trim_start().strip_prefix("> ").map(String::from))
            })
            .unwrap_or_else(|| "no description".to_string());
        skills.push(Skill {
            name: stem.to_string(),
            description,
            path,
        });
    }
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    skills
}

/// Parse the `> description` header from a skill file.
#[allow(dead_code)]
pub fn parse_skill_description(content: &str) -> Option<String> {
    content.lines().find_map(|l| {
        l.trim_start()
            .strip_prefix("> ")
            .map(|s| s.trim().to_string())
    })
}

const DEFAULT_SYSTEM_PROMPT: &str = r#"You are sigil, a coding agent that works inside a terminal.

You solve the user's task by calling tools. Prefer the dedicated tools
(read_file, write_file, git_status, crc32, ...) over bash where they exist.

After a tool result comes back, either call the next tool or give the
final answer in plain text. Keep answers concise. Do not call tools after
you have finished the task."#;
