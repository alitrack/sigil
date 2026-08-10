use std::path::PathBuf;

use clap::{CommandFactory, Parser, Subcommand};

/// sigil — a Rust + LuaJIT coding agent with a self-evolving harness.
#[derive(Parser, Debug)]
#[command(name = "sigil", version, about)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Model to use (overrides config).
    #[arg(long, global = true)]
    pub model: Option<String>,

    /// Base URL of an OpenAI-compatible endpoint (overrides config).
    #[arg(long, global = true)]
    pub base_url: Option<String>,

    /// API key (overrides env/config).
    #[arg(long, global = true)]
    pub api_key: Option<String>,

    /// Maximum tool-calling steps per turn (0 = unlimited).
    #[arg(long, default_value_t = 0, global = true)]
    pub max_steps: u32,

    /// Config file (default: ~/.sigil/config.toml).
    #[arg(long, global = true)]
    pub config: Option<String>,

    /// Directory with Lua scripted tools (default: ~/.sigil/tools/).
    #[arg(long, global = true)]
    pub tools_dir: Option<String>,

    /// Harness bundle directory (system prompt + skills; default: ~/.sigil/harness/).
    #[arg(long, global = true)]
    pub harness_dir: Option<String>,

    /// Memory file for remember/recall tools (default: ~/.sigil/memory.md).
    #[arg(long, global = true)]
    pub memory: Option<String>,

    /// Resume an interactive session by id (see ~/.sigil/sessions/).
    #[arg(long, global = true)]
    pub resume: Option<String>,

    /// Sessions directory for REPL history (default: ~/.sigil/sessions/).
    #[arg(long, global = true)]
    pub sessions_dir: Option<String>,

    /// List saved sessions and exit.
    #[arg(long, global = true)]
    pub list_sessions: bool,
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

    /// Start the background daemon: the agent loop lives on a Unix
    /// socket, so `sigil attach` / `sigil send` can drive it from
    /// anywhere — and a client can disconnect mid-run while the task
    /// keeps going.
    Daemon {
        /// Socket path (default: ~/.sigil/daemon.sock).
        #[arg(long)]
        socket: Option<PathBuf>,
    },

    /// Attach to a running daemon (interactive: send lines, watch the
    /// streamed output).
    Attach {
        /// Socket path (default: ~/.sigil/daemon.sock).
        #[arg(long)]
        socket: Option<PathBuf>,
    },

    /// Send one message to a running daemon and print the reply.
    Send {
        /// Message text.
        message: String,
        /// Socket path (default: ~/.sigil/daemon.sock).
        #[arg(long)]
        socket: Option<PathBuf>,
    },

    /// Serve as an MCP (Model Context Protocol) server on stdio —
    /// expose sigil's tools (bash, files, git, Lua, ...) to any MCP
    /// client (Claude Desktop, Cursor, other agents).
    Mcp,

    /// Run environment diagnostics: config, endpoint, harness, tools,
    /// memory, sessions, git. Exit 0 ok / 1 warnings / 2 failures.
    Doctor,

    /// Generate shell completions (bash, zsh, fish, elvish, powershell).
    Completions {
        /// Shell to generate completions for.
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },

    /// Export / import session transcripts (portable JSON).
    Session {
        #[command(subcommand)]
        action: SessionAction,
    },
}

/// Actions for `sigil session`.
#[derive(Subcommand, Debug)]
pub enum SessionAction {
    /// Export a session to a portable JSON archive.
    Export {
        /// Session id to export (see `sigil session list`).
        id: String,
        /// Output file path (default: <id>.json in the sessions dir).
        #[arg(short, long)]
        output: Option<String>,
        /// Only include turns at or after this cutoff (same format as the
        /// turn timestamps, e.g. `2026-22000-120000-000000000`; legacy
        /// turns without a timestamp are always kept).
        #[arg(long)]
        since: Option<String>,
    },

    /// Import a session archive as a new session (returns its id).
    Import {
        /// Archive file (JSON produced by `session export`).
        file: String,
    },

    /// List saved session ids (newest first).
    List,
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

/// Generate shell completions to stdout.
pub fn print_completions(shell: clap_complete::Shell) {
    let mut cmd = Cli::command();
    let name = cmd.get_name().to_string();
    clap_complete::generate(shell, &mut cmd, name, &mut std::io::stdout());
}
