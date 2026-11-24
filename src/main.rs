mod acp;
mod agent;
mod cli;
mod config;
mod cron;
#[cfg(unix)]
mod daemon;
mod doctor;
mod goals;
mod harness;
mod hooks;
mod llm;
mod lua;
mod mcp;
mod memory;
#[cfg(feature = "plan")]
mod plan;
mod repl;
mod revertible;
mod safety;
mod session;
mod tools;
mod ui;

use anyhow::{Context, Result};
use clap::Parser;
use serde_json::Value;
use std::io::Write;

#[cfg(feature = "plan")]
use crate::cli::PlanAction;
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

    // `sigil plan check|price <file>` — pure local sandbox work, no provider.
    // FROZEN behind the non-default `plan` feature (see Cargo.toml).
    #[cfg(feature = "plan")]
    if let Some(Command::Plan { action }) = &cli.command {
        return plan_cli(action);
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

    // Agent-loop Lua hooks (P-A thin-core roadmap): hook files under
    // the hooks dir (default ~/.sigil/hooks) run at agent-loop points.
    let hooks_dir = cli
        .hooks_dir
        .as_deref()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(hooks::default_hooks_dir);
    let hooks_lua = std::sync::Arc::new(std::sync::Mutex::new(
        lua::LuaRuntime::new().expect("lua runtime init"),
    ));

    // HookRunner borrows the dir first (with_tools needs it too), then
    // takes ownership.
    let mut tools = ToolRegistry::with_tools(
        Some(tools_dir.as_path()),
        cli.harness_dir.as_deref().map(std::path::Path::new),
        Some(prompt_state.clone()),
        Some(goals.clone()),
        cli.local_harness_dir.as_deref().map(std::path::Path::new),
        Some(hooks_dir.as_path()),
    );
    let hooks = std::sync::Arc::new(hooks::HookRunner::new(hooks_lua, hooks_dir));

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
        let connected = if let Some(url) = &server.url {
            mcp::McpClient::connect_http(&server.name, url, server.bearer_token.as_deref()).await
        } else {
            mcp::McpClient::connect(&server.name, &server.command, &server.args).await
        };
        match connected {
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
        // Agent-loop Lua hooks (P-A thin-core roadmap).
        hooks: Some(hooks.clone()),
        // Dynamic skill selection: inline at most 3 relevant skills
        // per turn (keyword-overlap ranked).
        max_inline_skills: 3,
        // Cross-instance sync: reload the prompt state when another
        // sigil process edits the harness dir.
        sync_dir: cli.harness_dir.as_deref().map(std::path::PathBuf::from),
        // Default transcript threshold reserves headroom for tools and output.
        // Message-count compaction is opt-in to preserve short cached histories.
        max_context_messages: config.max_context_messages,
        max_context_tokens: config.max_context_tokens,
        goals: Some(goals),
        // Where goals.jsonl lives — the agent loop persists turn-budget
        // auto-pauses there (same dir the manage_goals tool writes).
        goals_dir: Some(goals_dir),
        planning_interval: config.planning_interval,
        session_sink: None,
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
        cli.harness_dir.as_deref().map(std::path::PathBuf::from),
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
            acp::serve(
                tools,
                agent_config,
                provider_arc,
                Some(
                    cli.delegates_dir
                        .as_deref()
                        .map(std::path::PathBuf::from)
                        .unwrap_or_else(tools::delegate::default_delegates_dir),
                ),
                cli.harness_dir.as_deref().map(std::path::PathBuf::from),
            )
            .await
        }
        Some(Command::Daemon { socket }) => {
            #[cfg(unix)]
            {
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
            #[cfg(not(unix))]
            {
                let _ = (socket, provider_arc, tools, agent_config, &cli);
                anyhow::bail!("sigil daemon uses a Unix socket and is not available on Windows")
            }
        }
        Some(Command::Attach { socket, session }) => {
            #[cfg(unix)]
            {
                let socket = socket.clone().unwrap_or_else(daemon::default_socket_path);
                daemon::attach(&socket, session.as_deref()).await
            }
            #[cfg(not(unix))]
            {
                let _ = (socket, session);
                anyhow::bail!("sigil daemon uses a Unix socket and is not available on Windows")
            }
        }
        Some(Command::Send {
            message,
            socket,
            timeout,
            session,
        }) => {
            #[cfg(unix)]
            {
                let socket = socket.clone().unwrap_or_else(daemon::default_socket_path);
                let answer =
                    daemon::send_once(&socket, message, *timeout, session.as_deref()).await?;
                println!("{answer}");
                Ok(())
            }
            #[cfg(not(unix))]
            {
                let _ = (socket, message, timeout, session);
                anyhow::bail!("sigil daemon uses a Unix socket and is not available on Windows")
            }
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
        Some(Command::Refinements { merged }) => {
            let global_dir = cli
                .harness_dir
                .as_deref()
                .map(std::path::PathBuf::from)
                .unwrap_or_else(harness::default_harness_dir);
            let local_dir = cli
                .local_harness_dir
                .as_deref()
                .map(std::path::PathBuf::from);
            let events = if *merged {
                tools::evolve::load_merged_refinements(local_dir.as_deref(), &global_dir)
            } else {
                tools::evolve::load_refinements(&global_dir)
            };
            if events.is_empty() {
                println!(
                    "no refinements yet (harness dir: {}). Run the agent and \
                     improve_harness writes here.",
                    global_dir.display()
                );
            }
            for ev in &events {
                let ts = ev.get("ts").and_then(Value::as_str).unwrap_or("-");
                let id = ev.get("id").and_then(Value::as_str).unwrap_or("-");
                let action = ev.get("action").and_then(Value::as_str).unwrap_or("?");
                let scope = ev.get("scope").and_then(Value::as_str).unwrap_or("?");
                let reason = ev.get("reason").and_then(Value::as_str).unwrap_or("");
                let name = ev.get("name").and_then(Value::as_str).unwrap_or("");
                let name_part = if name.is_empty() {
                    String::new()
                } else {
                    format!(" {name}")
                };
                println!("{ts} [{scope}] {action}{name_part} {id} — {reason}");
            }
            Ok(())
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
            let (run_msgs, session_id) = prepare_pipe_turn(
                &sessions_dir,
                cli.resume.as_deref(),
                cli.sessions_dir.is_some(),
                &prompt,
            )?;
            let mut pipe_config = agent_config.clone();
            if let Some(id) = session_id {
                eprintln!("  (session {id})");
                let sink_dir = sessions_dir.clone();
                pipe_config.session_sink = Some(std::sync::Arc::new(move |event| {
                    if let Err(error) = session::append_event(&sink_dir, &id, &event) {
                        eprintln!("warning: failed to persist session event: {error}");
                    }
                }));
            }
            let result = agent::run_messages(
                provider_arc.as_ref(),
                &mut tools,
                &pipe_config,
                &run_msgs,
                None,
                Some(&stream_cb),
            )
            .await
            .context("agent turn failed")?;
            println!();
            // Machine-readable per-run stats on stderr (benchmark drivers
            // capture stderr; humans never see it in streaming mode).
            if let Some(u) = &result.tokens {
                eprintln!(
                    "[stats] steps={} prompt_tokens={} completion_tokens={} prompt_cache_hit_tokens={}",
                    result.steps, u.prompt_tokens, u.completion_tokens, u.prompt_cache_hit_tokens
                );
            } else {
                eprintln!("[stats] steps={} tokens=unreported", result.steps);
            }
            Ok(())
        }
    }
}

fn prepare_pipe_turn(
    sessions_dir: &std::path::Path,
    resume: Option<&str>,
    record: bool,
    prompt: &str,
) -> Result<(Vec<llm::ChatMessage>, Option<String>)> {
    if let Some(id) = resume {
        anyhow::ensure!(
            !id.is_empty()
                && id != "."
                && id != ".."
                && id
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c)),
            "invalid session id: expected a filename without path components"
        );
    }
    let id = match resume {
        Some("latest") => Some(match session::latest(sessions_dir)? {
            Some(id) => id,
            None => session::start(sessions_dir)?,
        }),
        Some(id) => Some(id.to_string()),
        None if record => Some(session::start(sessions_dir)?),
        None => None,
    };
    let mut messages = if let Some(id) = &id {
        session::repair_unpaired_tool_calls(sessions_dir, id)?;
        session::events_to_messages(&session::load_events(sessions_dir, id)?)
    } else {
        Vec::new()
    };
    if let Some(id) = &id {
        session::append_event(
            sessions_dir,
            id,
            &session::Event::User {
                ts: None,
                text: prompt.to_string(),
            },
        )?;
    }
    messages.push(llm::ChatMessage::user(prompt));
    Ok((messages, id))
}

/// `sigil plan check|price` — local-only plan sandbox CLI.
/// FROZEN behind the non-default `plan` feature (see Cargo.toml).
#[cfg(feature = "plan")]
fn plan_cli(action: &PlanAction) -> Result<()> {
    use std::time::Duration;
    let (file, budget, price) = match action {
        PlanAction::Check { file } => (file, 0.0, 0.0),
        PlanAction::Price {
            file,
            budget,
            price,
        } => (file, *budget, *price),
    };
    let src = std::fs::read_to_string(file).with_context(|| format!("read plan file {file}"))?;
    match action {
        PlanAction::Check { .. } => {
            let problems = plan::static_gate(&src);
            if problems.is_empty() {
                println!(
                    "OK: plan passes the static gate ({} lines)",
                    src.lines().count()
                );
            } else {
                println!("REFUSED ({} problems):", problems.len());
                for p in problems.iter().take(8) {
                    println!("  - {p}");
                }
                std::process::exit(1);
            }
        }
        PlanAction::Price { .. } => {
            match plan::dry_run(&src, budget, price, Duration::from_secs(60)) {
                Ok(est) => {
                    let (verdict, why) = plan::approve(&est, budget);
                    println!(
                        "estimate: {} calls, {:.1} credits (price {:.1}/call, budget {:.0})",
                        est.calls, est.credits, price, budget
                    );
                    println!("gate: {verdict} — {why}");
                }
                Err(problems) => {
                    println!("REFUSED ({} problems):", problems.len());
                    for p in problems.iter().take(8) {
                        println!("  - {p}");
                    }
                    std::process::exit(1);
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod pipe_tests {
    use super::*;

    #[test]
    fn recorded_pipe_turns_resume_without_losing_new_history() {
        let dir = std::env::temp_dir().join(format!(
            "sigil-pipe-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let (first, id) = prepare_pipe_turn(&dir, None, true, "first").unwrap();
        assert_eq!(first.last().unwrap().content, "first");
        let id = id.unwrap();
        session::append_event(
            &dir,
            &id,
            &session::Event::Assistant {
                ts: None,
                text: "answer".into(),
            },
        )
        .unwrap();
        let (next, resumed) = prepare_pipe_turn(&dir, Some(&id), false, "second").unwrap();
        assert_eq!(resumed.as_deref(), Some(id.as_str()));
        assert_eq!(
            next.iter().map(|m| m.content.as_str()).collect::<Vec<_>>(),
            vec!["first", "answer", "second"]
        );
        assert_eq!(
            session::load(&dir, &id).unwrap().last().unwrap().text,
            "second"
        );
        let (third, latest) = prepare_pipe_turn(&dir, Some("latest"), false, "third").unwrap();
        assert_eq!(latest, Some(id));
        assert_eq!(third.last().unwrap().content, "third");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn pipe_resume_rejects_paths_outside_session_directory() {
        for id in [
            "../outside",
            "/tmp/outside",
            "..",
            "",
            "folder\\outside",
            "C:outside",
        ] {
            assert!(
                prepare_pipe_turn(
                    std::path::Path::new("/tmp/sigil-pipe-invalid-id-must-not-be-created"),
                    Some(id),
                    false,
                    "hello"
                )
                .is_err()
            );
        }
    }

    #[test]
    fn ordinary_pipe_turn_does_not_create_a_session() {
        let dir =
            std::env::temp_dir().join(format!("sigil-pipe-unrecorded-{}", std::process::id()));
        let (messages, id) = prepare_pipe_turn(&dir, None, false, "hello").unwrap();
        assert!(id.is_none());
        assert_eq!(messages.len(), 1);
        assert!(!dir.exists());
    }
}
