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
    /// Full file contents (inlined into the prompt when relevant).
    pub body: String,
    /// Path to the skill file (for tools that read/edit it).
    #[allow(dead_code)]
    pub path: PathBuf,
    /// Other skill names this skill needs inlined alongside it.
    /// Declared as a `> depends_on: a, b` header line.
    pub depends_on: Vec<String>,
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

/// Live prompt state shared between the agent loop and the evolution
/// tools. `improve_harness` / `skill_rollback` write to disk *and*
/// update this, so the running agent sees the new system prompt /
/// skills on the very next turn without a restart.
#[derive(Debug, Clone, Default)]
pub struct PromptState {
    pub system_prompt: String,
    pub skills: Vec<Skill>,
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

    /// Render a compact index of all skills (name + description only,
    /// no bodies) so the model knows the full library exists and can
    /// read any skill's body on demand via read_file / list_skills.
    /// This is the "on-demand loading" half: index stays cheap no matter
    /// how large the library grows; only the relevant top-N bodies are
    /// actually inlined per turn. Takes `&[Skill]` so the agent loop can
    /// feed its live `PromptState` directly.
    pub fn skills_index_only_from(skills: &[Skill]) -> String {
        if skills.is_empty() {
            return String::new();
        }
        let mut out = String::from("\n\n# Available skills\n\n");
        for s in skills {
            out.push_str(&format!("- **{}** — {}\n", s.name, s.description));
        }
        out.push_str(
            "Read a skill's full content on demand with read_file \
             (or list_skills for names + descriptions).\n",
        );
        out
    }

    /// Render the skills section for the system prompt (all skills
    /// inline). Superseded at runtime by `inline_skills_for` (dynamic
    /// per-turn selection); kept for tests and completeness.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn skills_index(&self) -> String {
        let mut out = String::from("\n\n# Available skills\n\n");
        for s in &self.skills {
            out.push_str(&format!("- **{}** — {}\n", s.name, s.description));
        }
        out.push_str("\nSkills marked *(inlined below)* are embedded in full ");
        out.push_str("after this index; read the others with read_file before acting.");
        out.push('\n');
        for s in &self.skills {
            if s.body.len() <= 2 * 1024 {
                out.push_str(&format!(
                    "\n<skill name=\"{}\">\n{}</skill>\n",
                    s.name,
                    s.body.trim()
                ));
            }
        }
        out
    }

    /// Full system prompt: base + skill index (all skills inline).
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn full_system_prompt(&self) -> String {
        let mut p = self.system_prompt.clone();
        p.push_str(&self.skills_index());
        p
    }
}

/// Render the `<skill>` blocks for the given skills, ranked by keyword
/// overlap with `prompt` (most relevant first), capped at `max_inline`.
/// When a skill is inlined, its `depends_on` skills are pulled in too
/// (transitively, cycle-safe, deduped). Empty prompt = no inlining.
pub fn inline_skills_for(prompt: &str, skills: &[Skill], max_inline: usize) -> String {
    let prompt_tokens = tokenize(prompt);
    if prompt_tokens.is_empty() {
        return String::new();
    }
    let by_name: std::collections::HashMap<&str, &Skill> =
        skills.iter().map(|s| (s.name.as_str(), s)).collect();
    let mut ranked: Vec<(&Skill, usize)> = skills
        .iter()
        .filter(|s| s.body.len() <= 2 * 1024)
        .map(|s| (s, overlap_score(&prompt_tokens, s)))
        .filter(|(_, score)| *score > 0)
        .collect();
    ranked.sort_by_key(|(_, score)| std::cmp::Reverse(*score));

    // Dependency closure: BFS from the ranked hits, pulling in
    // `depends_on` transitively. Visited set makes it cycle-safe.
    let mut included: Vec<&Skill> = Vec::new();
    let mut visited: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (skill, _) in ranked.iter().take(max_inline) {
        let mut queue = std::collections::VecDeque::new();
        queue.push_back(*skill);
        while let Some(s) = queue.pop_front() {
            if !visited.insert(s.name.clone()) {
                continue;
            }
            if s.body.len() <= 2 * 1024 {
                included.push(s);
            }
            for dep in &s.depends_on {
                if let Some(dep_skill) = by_name.get(dep.as_str()) {
                    queue.push_back(dep_skill);
                }
            }
        }
    }

    let mut out = String::new();
    for s in included {
        out.push_str(&format!(
            "\n<skill name=\"{}\">\n{}</skill>\n",
            s.name,
            s.body.trim()
        ));
    }
    out
}

/// Lowercase alphanumeric tokens (≥ 2 chars) from a string.
fn tokenize(s: &str) -> Vec<String> {
    s.chars()
        .map(|c| {
            if c.is_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .filter(|t| t.len() >= 2)
        .map(|t| t.to_string())
        .collect()
}

/// Keyword overlap between the prompt and a skill
/// (name/description/body tokens).
fn overlap_score(prompt_tokens: &[String], skill: &Skill) -> usize {
    if prompt_tokens.is_empty() {
        return 0;
    }
    let mut corpus = tokenize(&skill.name);
    corpus.extend(tokenize(&skill.description));
    corpus.extend(tokenize(&skill.body));
    let mut score = 0;
    for t in prompt_tokens {
        if corpus.iter().any(|c| c == t) {
            score += 1;
        }
    }
    score
}

pub fn load_skills(dir: &Path) -> Vec<Skill> {
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
        // Read once: full body + description from the `> ` header line.
        let Some(text) = std::fs::read_to_string(&path).ok() else {
            continue;
        };
        let description = text
            .lines()
            .find_map(|l| l.trim_start().strip_prefix("> ").map(String::from))
            .unwrap_or_else(|| "no description".to_string());
        // `> depends_on: a, b` header — comma-separated skill names.
        let depends_on: Vec<String> = text
            .lines()
            .find_map(|l| {
                l.trim_start().strip_prefix("> depends_on:").map(|rest| {
                    rest.split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect()
                })
            })
            .unwrap_or_default();
        skills.push(Skill {
            name: stem.to_string(),
            description,
            body: text,
            path,
            depends_on,
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

/// Detect cycles in the skill dependency graph.
///
/// Returns one entry per cycle found, each a chain like
/// `["a", "b", "a"]` showing the cycle. Unknown dependency names are
/// ignored (they just don't resolve to a skill). Uses DFS with
/// white/grey/black coloring; cycle-safe by construction.
pub fn find_dependency_cycles(skills: &[Skill]) -> Vec<Vec<String>> {
    use std::collections::HashMap;
    let by_name: HashMap<&str, &Skill> = skills.iter().map(|s| (s.name.as_str(), s)).collect();

    // 0 = white (unvisited), 1 = grey (on the DFS stack), 2 = black (done).
    let mut color: HashMap<&str, u8> = HashMap::new();
    let mut cycles: Vec<Vec<String>> = Vec::new();

    fn dfs<'a>(
        name: &'a str,
        by_name: &HashMap<&'a str, &'a Skill>,
        color: &mut HashMap<&'a str, u8>,
        stack: &mut Vec<&'a str>,
        cycles: &mut Vec<Vec<String>>,
    ) {
        color.insert(name, 1);
        stack.push(name);
        let skill = by_name.get(name);
        if let Some(s) = skill {
            for dep in &s.depends_on {
                match color.get(dep.as_str()).copied() {
                    // Grey = back edge → cycle: slice from the dep
                    // back to itself in the current stack.
                    Some(1) => {
                        let pos = stack.iter().position(|n| *n == dep.as_str());
                        let mut chain: Vec<String> = stack[pos.unwrap_or(0)..]
                            .iter()
                            .map(|n| (*n).to_string())
                            .collect();
                        chain.push(dep.to_string());
                        cycles.push(chain);
                    }
                    None => dfs(dep, by_name, color, stack, cycles),
                    _ => {}
                }
            }
        }
        stack.pop();
        color.insert(name, 2);
    }

    for s in skills {
        if color.get(s.name.as_str()).copied().unwrap_or(0) == 0 {
            dfs(&s.name, &by_name, &mut color, &mut Vec::new(), &mut cycles);
        }
    }
    cycles
}

const DEFAULT_SYSTEM_PROMPT: &str = r#"You are sigil, a coding agent that works inside a terminal.

You solve the user's task by calling tools. Prefer the dedicated tools
(read_file, write_file, git_status, crc32, ...) over bash where they exist.

After a tool result comes back, either call the next tool or give the
final answer in plain text. Keep answers concise. Do not call tools after
you have finished the task."#;

#[cfg(test)]
mod harness_tests;

pub mod versions;

/// Cross-process advisory lock for harness/tools directories. See
/// [`lock::with_dir_lock`] — the evolution tools wrap their write
/// sequences with it so concurrent sigil instances never interleave
/// "read old → archive → write new".
pub mod lock;
