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
    sessions_dir: &std::path::Path,
    resume: Option<&str>,
) -> anyhow::Result<()> {
    let approver = interactive_approver();
    let mut stdout = std::io::stdout();
    let mut stdin = std::io::stdin().lock();
    let mut buf = String::new();

    // Session: resume an existing one or start fresh.
    let session_id = match resume {
        Some(id) => id.to_string(),
        None => crate::session::start(sessions_dir)?,
    };
    let history = crate::session::load(sessions_dir, &session_id)?;
    if !history.is_empty() {
        println!("(resumed session {session_id} — {} turns)", history.len());
    }

    println!("sigil — Rust + LuaJIT coding agent (interactive)");
    println!("Type /help for commands, /quit to exit.\n");

    // Prefix the resumed transcript so the model has conversation context.
    let context_prefix = if history.is_empty() {
        String::new()
    } else {
        let mut s = String::from("(Earlier conversation in this session:\n");
        for t in &history {
            s.push_str(&format!("{}: {}\n", t.role, t.text));
        }
        s.push_str(")\n\n");
        s
    };

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
                println!("  /session show this session id");
                println!("  /quit    exit");
                println!("  anything else is sent to the agent");
                continue;
            }
            "/session" => {
                println!("  session id: {session_id}");
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
        crate::session::append(
            sessions_dir,
            &session_id,
            &crate::session::Turn {
                role: "user".into(),
                text: line.into(),
            },
        )?;

        // Turn with interactive approval.
        let prompt = format!("{context_prefix}{line}");
        let answer =
            match run_turn_with_approver(provider, tools, config, &prompt, Some(&approver)).await {
                Ok(a) => a,
                Err(e) => {
                    eprintln!("✗ error: {e}");
                    continue;
                }
            };
        crate::session::append(
            sessions_dir,
            &session_id,
            &crate::session::Turn {
                role: "assistant".into(),
                text: answer.clone(),
            },
        )?;
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
