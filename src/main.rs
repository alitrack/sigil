mod acp;
mod agent;
mod cli;
mod config;
mod harness;
mod llm;
mod lua;
mod mcp;
mod memory;
mod repl;
mod safety;
mod session;
mod tools;

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
    let config = Config::load(cli.config.as_deref())?.with_cli_overrides(
        cli.model.as_deref(),
        cli.base_url.as_deref(),
        cli.api_key.as_deref(),
        cli.max_steps,
    );

    let provider = OpenAiProvider::new(OpenAiConfig {
        base_url: config.base_url.clone(),
        api_key: config.api_key.clone(),
    })?;

    // Tools: native + scripted Lua + memory + MCP.
    let tools_dir = cli
        .tools_dir
        .as_deref()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(scripted::default_tools_dir);
    let mut tools = ToolRegistry::with_tools(
        Some(tools_dir.as_path()),
        cli.harness_dir.as_deref().map(std::path::Path::new),
    );

    // Memory tools (remember/recall) — default ~/.sigil/memory.md.
    let memory_path = cli
        .memory
        .as_deref()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(memory::default_memory_path);
    tools::memory::register(&mut tools, memory_path);

    // MCP servers: connect, discover tools, register as mcp_<server>_<tool>.
    for server in &config.mcp_servers {
        match mcp::McpClient::connect(&server.name, &server.command, &server.args).await {
            Ok((client, infos)) => {
                let client = std::sync::Arc::new(client);
                let n = infos.len();
                for info in infos {
                    let tool = tools::mcp::McpTool::new(&server.name, info, client.clone());
                    tools.register(std::sync::Arc::new(tool));
                }
                eprintln!("mcp: connected '{}' ({} tools)", server.name, n);
            }
            Err(e) => eprintln!("mcp: failed to connect '{}': {e}", server.name),
        }
    }

    // Harness bundle: system prompt + skills from disk (editable).
    let harness = harness::Harness::load(cli.harness_dir.as_deref().map(std::path::Path::new))?;

    let agent_config = agent::AgentConfig {
        model: config.model.clone(),
        max_steps: config.max_steps,
        system_prompt: harness.full_system_prompt(),
    };

    // Sub-agent delegation: the model can spin off independent tasks.
    let provider_arc: std::sync::Arc<dyn crate::llm::provider::LlmProvider> =
        std::sync::Arc::new(provider);
    tools.register(std::sync::Arc::new(tools::delegate::DelegateTool::new(
        provider_arc.clone(),
        tools.clone_shallow(),
        agent_config.clone(),
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
        Some(Command::Acp) => {
            eprintln!("sigil acp: serving Agent Client Protocol on stdio");
            acp::serve(tools, agent_config, provider_arc).await
        }
        Some(Command::Mcp) => {
            eprintln!(
                "sigil mcp: serving MCP server on stdio ({} tools)",
                tools.specs().len()
            );
            mcp::server::serve(&tools).await
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
            // Pipe mode with live streaming to stdout.
            let stream_cb = |delta: Option<&str>| {
                if let Some(d) = delta {
                    print!("{d}");
                    let _ = std::io::stdout().flush();
                }
            };
            let _answer = agent::run_turn_with_approver(
                provider_arc.as_ref(),
                &mut tools,
                &agent_config,
                &prompt,
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
