mod agent;
mod cli;
mod config;
mod harness;
mod llm;
mod lua;
mod safety;
mod tools;

use std::io::Read;

use anyhow::{Context, Result};
use clap::Parser;

use crate::cli::Cli;
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

    // Pipe mode: `echo "fix the bug" | sigil` — read stdin, run one turn.
    let prompt = match cli.prompt {
        Some(p) => p,
        None => read_stdin()?,
    };
    if prompt.trim().is_empty() {
        anyhow::bail!("no prompt: pass -p/--prompt or pipe stdin");
    }

    let tools_dir = cli
        .tools_dir
        .as_deref()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(scripted::default_tools_dir);
    let mut tools = ToolRegistry::with_tools(Some(tools_dir.as_path()));

    // Harness bundle: system prompt + skills from disk (editable).
    let harness = harness::Harness::load(cli.harness_dir.as_deref().map(std::path::Path::new))?;

    let agent_config = agent::AgentConfig {
        model: config.model.clone(),
        max_steps: config.max_steps,
        system_prompt: harness.full_system_prompt(),
    };

    let answer = agent::run_turn(&provider, &mut tools, &agent_config, &prompt)
        .await
        .context("agent turn failed")?;

    println!("{answer}");
    Ok(())
}

fn read_stdin() -> Result<String> {
    let mut buf = String::new();
    std::io::stdin()
        .read_to_string(&mut buf)
        .context("reading stdin")?;
    Ok(buf)
}
