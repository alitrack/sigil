mod agent;
mod cli;
mod config;
mod llm;
mod tools;

use std::io::Read;

use anyhow::{Context, Result};
use clap::Parser;

use crate::cli::Cli;
use crate::config::Config;
use crate::llm::openai::{OpenAiConfig, OpenAiProvider};
use crate::tools::ToolRegistry;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = Config::from_env()?.with_overrides(
        cli.model.as_deref(),
        cli.base_url.as_deref(),
        cli.api_key.as_deref(),
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

    let tools = ToolRegistry::with_native_tools();
    let agent_config = agent::AgentConfig {
        model: config.model.clone(),
        max_steps: cli.max_steps,
        system_prompt: config.system_prompt.clone(),
    };

    let answer = agent::run_turn(&provider, &tools, &agent_config, &prompt)
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
