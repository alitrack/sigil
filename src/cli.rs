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

    /// Directory with agent-loop Lua hooks (default: ~/.sigil/hooks/).
    /// Hook files follow the scripted-tool contract and run at loop
    /// points: on_turn_start.lua (per-turn system injection),
    /// on_tool_result.lua (tool output rewrite).
    #[arg(long, global = true)]
    pub hooks_dir: Option<String>,

    /// Harness bundle directory (system prompt + skills; default: ~/.sigil/harness/).
    #[arg(long, global = true)]
    pub harness_dir: Option<String>,

    /// Session-local harness directory (optional). When set, sigil loads
    /// a merged view: local system_prompt.md overrides the global one and
    /// local skills shadow same-named global skills. improve_harness can
    /// write here with scope=local — experiments stay out of the global
    /// harness (prime-agent local/global dual-track).
    #[arg(long, global = true)]
    pub local_harness_dir: Option<String>,

    /// Memory file for remember/recall tools (default: ~/.sigil/memory.md).
    #[arg(long, global = true)]
    pub memory: Option<String>,

    /// Resume a saved session by id or latest (see ~/.sigil/sessions/).
    #[arg(long, global = true)]
    pub resume: Option<String>,

    /// Sessions directory for history; explicitly setting it also records pipe turns.
    /// Default: ~/.sigil/sessions/.
    #[arg(long, global = true)]
    pub sessions_dir: Option<String>,

    /// Directory where sub-agent delegate state is persisted
    /// (default: ~/.sigil/delegates/). Each sub-agent gets a
    /// `<id>/meta.json`; the registry is recovered from here on
    /// startup, so delegates survive process restarts and can be
    /// resumed.
    #[arg(long, global = true)]
    pub delegates_dir: Option<String>,

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
        /// Session id to talk to (default: the daemon's default session).
        #[arg(long)]
        session: Option<String>,
    },

    /// Send one message to a running daemon and print the reply.
    Send {
        /// Message text.
        message: String,
        /// Socket path (default: ~/.sigil/daemon.sock).
        #[arg(long)]
        socket: Option<PathBuf>,
        /// Session id to send to (default: the daemon's default
        /// session; an unknown id starts a fresh session).
        #[arg(long)]
        session: Option<String>,
        /// Seconds to wait for the daemon's reply (0 = wait forever).
        #[arg(long, default_value_t = 300)]
        timeout: u64,
    },

    /// Serve as an MCP (Model Context Protocol) server on stdio —
    /// expose sigil's tools (bash, files, git, Lua, ...) to any MCP
    /// client (Claude Desktop, Cursor, other agents).
    Mcp,

    /// Run environment diagnostics: config, endpoint, harness, tools,
    /// memory, sessions, git. Exit 0 ok / 1 warnings / 2 failures.
    Doctor,

    /// Check / price / run a dynamic-workflow plan (LuaJIT sandbox).
    /// `sigil plan check <file>` — static gate only.
    /// `sigil plan price <file>` — static gate + blind dry-run estimate + approval verdict.
    Plan {
        #[command(subcommand)]
        action: PlanAction,
    },

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

    /// Show the harness refinement audit log (refinements.jsonl).
    /// Each event carries a stable id (content fingerprint) so the
    /// same improvement can be traced across scopes.
    Refinements {
        /// Also merge in the session-local refinements: local entries
        /// win over same-id global ones (the prime-agent
        /// mergeRefinementHistory pattern).
        #[arg(long)]
        merged: bool,
    },
}

/// Actions for `sigil plan`.
#[derive(Subcommand, Debug)]
pub enum PlanAction {
    /// Run the static gate on a plan file; print problems or OK.
    Check {
        /// Path to the plan (.lua source).
        file: String,
    },
    /// Static gate + blind dry-run price estimate + approval verdict.
    /// Budget seen by the stand-in = the budget the real run would see.
    Price {
        /// Path to the plan (.lua source).
        file: String,
        /// Budget cap in credits (default 150).
        #[arg(long, default_value_t = 150.0)]
        budget: f64,
        /// Price per scan call in credits (default 0.2).
        #[arg(long, default_value_t = 0.2)]
        price: f64,
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
