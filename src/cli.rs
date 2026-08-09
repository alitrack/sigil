use clap::{Parser, Subcommand};

/// sigil — a Rust + LuaJIT coding agent with a self-evolving harness.
#[derive(Parser, Debug)]
#[command(name = "sigil", version, about)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Model to use (overrides config).
    #[arg(long)]
    pub model: Option<String>,

    /// Base URL of an OpenAI-compatible endpoint (overrides config).
    #[arg(long)]
    pub base_url: Option<String>,

    /// API key (overrides env/config).
    #[arg(long)]
    pub api_key: Option<String>,

    /// Maximum tool-calling steps per turn (0 = unlimited).
    #[arg(long, default_value_t = 0)]
    pub max_steps: u32,

    /// Config file (default: ~/.sigil/config.toml).
    #[arg(long)]
    pub config: Option<String>,

    /// Directory with Lua scripted tools (default: ~/.sigil/tools/).
    #[arg(long)]
    pub tools_dir: Option<String>,

    /// Harness bundle directory (system prompt + skills; default: ~/.sigil/harness/).
    #[arg(long)]
    pub harness_dir: Option<String>,

    /// Memory file for remember/recall tools (default: ~/.sigil/memory.md).
    #[arg(long)]
    pub memory: Option<String>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Run one turn with the given prompt (pipe mode: reads stdin).
    #[command(alias = "run")]
    Prompt {
        /// Initial task.
        #[arg(short, long)]
        prompt: Option<String>,
    },

    /// Serve as an ACP (Agent Client Protocol) server on stdio —
    /// any ACP client (editors, other agents) can drive sigil.
    Acp,
}

/// The prompt to use: from -p/--prompt, else stdin (pipe mode).
pub fn prompt_from(sub_prompt: Option<String>) -> anyhow::Result<String> {
    if let Some(p) = sub_prompt {
        return Ok(p);
    }
    let mut buf = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)
        .map_err(|e| anyhow::anyhow!("reading stdin: {e}"))?;
    if buf.trim().is_empty() {
        anyhow::bail!("no prompt: pass -p/--prompt or pipe stdin");
    }
    Ok(buf)
}
