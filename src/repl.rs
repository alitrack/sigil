//! Interactive REPL: `sigil` with no prompt and a TTY stdin enters a
//! conversational loop. Unlike piped mode (one turn, gate rejects
//! destructive commands), the REPL lets the human approve rejected
//! tool calls interactively.

use std::io::{BufRead, IsTerminal, Write};

use crate::agent::{AgentConfig, Approver, run_turn_with_approver};
use crate::llm::provider::LlmProvider;
use crate::tools::ToolRegistry;

/// True when we should enter the REPL: stdin is a terminal and no
/// prompt was given via -p/--prompt.
pub fn should_enter_repl(sub_prompt: &Option<String>) -> bool {
    sub_prompt.is_none() && std::io::stdin().is_terminal()
}

/// Interactive approver: print the rejected call, ask y/N.
fn interactive_approver() -> Box<Approver> {
    Box::new(|name: &str, args: &serde_json::Value| {
        eprintln!(
            "\n⚠ sigil wants to run a command the gate flags as destructive:\n  {name} {args}\n"
        );
        loop {
            eprint!("  Allow it? [y/N] ");
            let _ = std::io::stderr().flush();
            let mut line = String::new();
            if std::io::stdin().lock().read_line(&mut line).is_err() {
                return false;
            }
            match line.trim().to_ascii_lowercase().as_str() {
                "y" | "yes" => return true,
                "" | "n" | "no" => return false,
                _ => continue,
            }
        }
    })
}

/// Run the interactive loop until the user quits.
pub async fn run(
    provider: &dyn LlmProvider,
    tools: &mut ToolRegistry,
    config: &AgentConfig,
) -> anyhow::Result<()> {
    let approver = interactive_approver();
    let mut stdout = std::io::stdout();
    let mut stdin = std::io::stdin().lock();
    let mut buf = String::new();

    println!("sigil — Rust + LuaJIT coding agent (interactive)");
    println!("Type /help for commands, /quit to exit.\n");

    loop {
        eprint!("❯ ");
        let _ = std::io::stderr().flush();
        buf.clear();
        let n = stdin.read_line(&mut buf)?;
        if n == 0 {
            println!();
            break; // EOF
        }
        let line = buf.trim();
        if line.is_empty() {
            continue;
        }
        match line {
            "/quit" | "/exit" | "/q" => break,
            "/help" | "/h" => {
                println!("  /tools   list available tools");
                println!("  /quit    exit");
                println!("  anything else is sent to the agent");
                continue;
            }
            "/tools" => {
                for spec in tools.specs() {
                    println!("  {} — {}", spec.name, spec.description);
                }
                continue;
            }
            _ => {}
        }

        // Turn with interactive approval.
        let answer =
            match run_turn_with_approver(provider, tools, config, line, Some(&approver)).await {
                Ok(a) => a,
                Err(e) => {
                    eprintln!("✗ error: {e}");
                    continue;
                }
            };
        println!("\n{answer}\n");
        let _ = stdout.flush();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::should_enter_repl;

    #[test]
    fn repl_requires_no_sub_prompt() {
        // With a -p/--prompt given, never enter REPL even on a TTY.
        assert!(!should_enter_repl(&Some("hello".into())));
    }
}
