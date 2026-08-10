mod acp;
mod agent;
mod cli;
mod config;
mod daemon;
mod doctor;
mod goals;
mod harness;
mod llm;
mod lua;
mod mcp;
mod memory;
mod repl;
mod safety;
mod session;
mod tools;
mod ui;

use anyhow::{Context, Result};
use clap::Parser;
use std::io::Write;

use crate::cli::{Cli, Command};
use crate::config::Config;
use crate::llm::openai::{OpenAiConfig, OpenAiProvider};
use crate::tools::{ToolRegistry, scripted};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // `sigil completions <shell>` needs no config, env, or provider.
    if let Some(Command::Completions { shell }) = &cli.command {
        cli::print_completions(*shell);
        return Ok(());
    }

    let config = Config::load(cli.config.as_deref())?.with_cli_overrides(
        cli.model.as_deref(),
        cli.base_url.as_deref(),
        cli.api_key.as_deref(),
        cli.max_steps,
    );

    // `sigil doctor` runs before any provider/tooling is built —
    // diagnostics must not depend on a working LLM endpoint.
    if matches!(cli.command, Some(Command::Doctor)) {
        let code = doctor::run(&config, &cli).await?;
        std::process::exit(code);
    }

    let provider = OpenAiProvider::with_fallbacks(
        OpenAiConfig {
            base_url: config.base_url.clone(),
            api_key: config.api_key.clone(),
        },
        config
            .fallbacks
            .iter()
            .map(|f| OpenAiConfig {
                base_url: f.base_url.clone(),
                api_key: f.api_key.clone(),
            })
            .collect(),
    )?;

    // Harness bundle: system prompt + skills from disk (editable).
    // Loaded before the tools so the evolution tools can share the
    // live prompt state with the agent loop. A --local-harness-dir
    // creates a merged view (local shadows global) — the session's
    // private experiment surface.
    let harness = harness::Harness::load_with_local(
        cli.harness_dir.as_deref().map(std::path::Path::new),
        cli.local_harness_dir.as_deref().map(std::path::Path::new),
    )?;
    let prompt_state = std::sync::Arc::new(std::sync::RwLock::new(harness::PromptState {
        system_prompt: harness.system_prompt.clone(),
        skills: harness.skills.clone(),
        fingerprint: None,
    }));

    // Tools: native + scripted Lua + memory + MCP.
    let tools_dir = cli
        .tools_dir
        .as_deref()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(scripted::default_tools_dir);
    // Persistent goals: the same `Arc` goes to the manage_goals tool
    // and the agent loop (which injects active goals into the system
    // context every turn). Loaded from the harness dir so goals
    // survive restarts and are shared across instances.
    let goals_dir: std::path::PathBuf = cli
        .harness_dir
        .as_deref()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(harness::default_harness_dir);
    let goals = std::sync::Arc::new(std::sync::RwLock::new(goals::load_goals(&goals_dir)));
    let mut tools = ToolRegistry::with_tools(
        Some(tools_dir.as_path()),
        cli.harness_dir.as_deref().map(std::path::Path::new),
        Some(prompt_state.clone()),
        Some(goals.clone()),
        cli.local_harness_dir.as_deref().map(std::path::Path::new),
    );

    // Memory tools (remember/recall) — default ~/.sigil/memory.md.
    let memory_path = cli
        .memory
        .as_deref()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(memory::default_memory_path);
    tools::memory::register(&mut tools, memory_path);

    // MCP servers: connect, discover tools, register as mcp_<server>_<tool>.
    let mut mcp_servers: Vec<(String, std::sync::Arc<mcp::McpClient>)> = Vec::new();
    for server in &config.mcp_servers {
        match mcp::McpClient::connect(&server.name, &server.command, &server.args).await {
            Ok((client, infos)) => {
                let client = std::sync::Arc::new(client);
                let n = infos.len();
                for info in infos {
                    let tool = tools::mcp::McpTool::new(&server.name, info, client.clone());
                    tools.register(std::sync::Arc::new(tool));
                }
                mcp_servers.push((server.name.clone(), client));
                eprintln!("mcp: connected '{}' ({} tools)", server.name, n);
            }
            Err(e) => eprintln!("mcp: failed to connect '{}': {e}", server.name),
        }
    }
    // One tool to read resources from every connected MCP server.
    if !mcp_servers.is_empty() {
        let servers = mcp_servers.clone();
        tools.register(std::sync::Arc::new(tools::mcp::McpResourcesTool {
            servers,
        }));
        tools.register(std::sync::Arc::new(tools::mcp::McpPromptsTool {
            servers: mcp_servers,
        }));
    }

    // Live prompt state is built above (before the tools); the agent
    // loop shares it with the evolution tools so harness edits apply
    // on the very next turn.
    let agent_config = agent::AgentConfig {
        model: config.model.clone(),
        max_steps: config.max_steps,
        prompt_state: prompt_state.clone(),
        // Dynamic skill selection: inline at most 3 relevant skills
        // per turn (keyword-overlap ranked).
        max_inline_skills: 3,
        // Cross-instance sync: reload the prompt state when another
        // sigil process edits the harness dir.
        sync_dir: cli.harness_dir.as_deref().map(std::path::PathBuf::from),
        // Context budget: fold old turns into a summary when the
        // transcript exceeds 40 messages or ~64k tokens (long REPL
        // sessions; deepseek-v4-flash context is 64k).
        max_context_messages: 40,
        max_context_tokens: 64_000,
        goals: Some(goals),
        // Where goals.jsonl lives — the agent loop persists turn-budget
        // auto-pauses there (same dir the manage_goals tool writes).
        goals_dir: Some(goals_dir),
        planning_interval: 0,
    };

    // Sub-agent delegation: the model can spin off independent tasks.
    let provider_arc: std::sync::Arc<dyn crate::llm::provider::LlmProvider> =
        std::sync::Arc::new(provider);
    tools.register(std::sync::Arc::new(tools::delegate::DelegateTool::new(
        provider_arc.clone(),
        tools.clone_shallow(),
        agent_config.clone(),
        Some(
            cli.delegates_dir
                .as_deref()
                .map(std::path::PathBuf::from)
                .unwrap_or_else(tools::delegate::default_delegates_dir),
        ),
    )));

    if cli.list_sessions {
        let sessions_dir = cli
            .sessions_dir
            .as_deref()
            .map(std::path::PathBuf::from)
            .unwrap_or_else(session::default_sessions_dir);
        for id in session::list(&sessions_dir)? {
            println!("{id}");
        }
        return Ok(());
    }

    let sub_prompt = match &cli.command {
        Some(Command::Prompt { prompt }) => prompt.clone(),
        _ => None,
    };

    match &cli.command {
        Some(Command::Session { action }) => {
            let sessions_dir = cli
                .sessions_dir
                .as_deref()
                .map(std::path::PathBuf::from)
                .unwrap_or_else(session::default_sessions_dir);
            match action {
                cli::SessionAction::Export { id, output, since } => {
                    let out = output
                        .as_deref()
                        .map(std::path::PathBuf::from)
                        .unwrap_or_else(|| sessions_dir.join(format!("{id}.json")));
                    let archive =
                        session::export_session(&sessions_dir, id, &out, since.as_deref())?;
                    println!(
                        "exported session {id} ({} turns) → {}",
                        archive.turns.len(),
                        out.display()
                    );
                }
                cli::SessionAction::Import { file } => {
                    let path = std::path::Path::new(file);
                    let new_id = session::import_session(&sessions_dir, path)?;
                    println!(
                        "imported {file} as new session {new_id} (resume with --resume {new_id})"
                    );
                }
                cli::SessionAction::List => {
                    let sessions = session::summaries(&sessions_dir)?;
                    if sessions.is_empty() {
                        println!("no sessions");
                    } else {
                        for s in sessions {
                            let when = s
                                .last_ts
                                .as_deref()
                                .map(|t| &t[..t.len().min(17)])
                                .unwrap_or("?");
                            if s.preview.is_empty() {
                                println!("{when}  {}  {} turns", s.id, s.turns);
                            } else {
                                println!("{when}  {}  {} turns  {}", s.id, s.turns, s.preview);
                            }
                        }
                    }
                }
            }
            return Ok(());
        }
        Some(Command::Acp) => {
            eprintln!("sigil acp: serving Agent Client Protocol on stdio");
            acp::serve(tools, agent_config, provider_arc).await
        }
        Some(Command::Daemon { socket }) => {
            let socket = socket.clone().unwrap_or_else(daemon::default_socket_path);
            let sessions_dir = cli
                .sessions_dir
                .as_deref()
                .map(std::path::PathBuf::from)
                .unwrap_or_else(session::default_sessions_dir);
            daemon::serve(
                &socket,
                provider_arc.clone(),
                &mut tools,
                &agent_config,
                &sessions_dir,
                cli.resume.as_deref(),
            )
            .await
        }
        Some(Command::Attach { socket }) => {
            let socket = socket.clone().unwrap_or_else(daemon::default_socket_path);
            daemon::attach(&socket).await
        }
        Some(Command::Send {
            message,
            socket,
            timeout,
        }) => {
            let socket = socket.clone().unwrap_or_else(daemon::default_socket_path);
            let answer = daemon::send_once(&socket, message, *timeout).await?;
            println!("{answer}");
            Ok(())
        }
        Some(Command::Mcp) => {
            eprintln!(
                "sigil mcp: serving MCP server on stdio ({} tools)",
                tools.specs().len()
            );
            // Expose the harness as read-only MCP resources: the
            // system prompt plus every skill, so MCP clients (Claude
            // Desktop, Cursor, ...) can read sigil's instruction layer.
            let mut resources: Vec<mcp::server::Resource> = Vec::new();
            resources.push(mcp::server::Resource {
                uri: "sigil://system-prompt".into(),
                name: "sigil system prompt".into(),
                description: "sigil's harness system prompt (editable via improve_harness)".into(),
                content: harness.system_prompt.clone(),
            });
            for s in &harness.skills {
                resources.push(mcp::server::Resource {
                    uri: format!("sigil://skills/{}", s.name),
                    name: format!("skill: {}", s.name),
                    description: s.description.clone(),
                    content: s.body.clone(),
                });
            }
            mcp::server::serve(&tools, &resources).await
        }
        _ if repl::should_enter_repl(&sub_prompt) => {
            // Interactive mode: multi-turn conversation with human
            // approval for destructive tool calls.
            let sessions_dir = cli
                .sessions_dir
                .as_deref()
                .map(std::path::PathBuf::from)
                .unwrap_or_else(session::default_sessions_dir);
            repl::run(
                provider_arc.as_ref(),
                &mut tools,
                &agent_config,
                &sessions_dir,
                cli.resume.as_deref(),
            )
            .await
        }
        _ => {
            let prompt = cli::prompt_from(sub_prompt)?;
            let sessions_dir = cli
                .sessions_dir
                .as_deref()
                .map(std::path::PathBuf::from)
                .unwrap_or_else(session::default_sessions_dir);
            // Pipe mode with live streaming to stdout.
            let stream_cb = |delta: Option<&str>| {
                if let Some(d) = delta {
                    print!("{d}");
                    let _ = std::io::stdout().flush();
                }
            };
            // `--resume <id>` (or `latest`) continues an existing
            // session: load its transcript as the conversation context
            // and append this prompt, exactly like the REPL/daemon.
            let run_msgs = match cli.resume.as_deref() {
                Some(r) => {
                    let id = if r == "latest" {
                        match session::latest(&sessions_dir)? {
                            Some(id) => id,
                            None => {
                                eprintln!("  no sessions yet — starting a new one");
                                session::start(&sessions_dir)?
                            }
                        }
                    } else {
                        r.to_string()
                    };
                    let turns = session::load(&sessions_dir, &id)?;
                    eprintln!("  (resumed session {id} — {} turns)", turns.len());
                    let mut msgs = session::turns_to_messages(turns);
                    msgs.push(crate::llm::ChatMessage::user(prompt));
                    msgs
                }
                None => vec![crate::llm::ChatMessage::user(prompt)],
            };
            let _result = agent::run_messages(
                provider_arc.as_ref(),
                &mut tools,
                &agent_config,
                &run_msgs,
                None,
                Some(&stream_cb),
            )
            .await
            .context("agent turn failed")?;
            println!();
            Ok(())
        }
    }
}
