//! Interactive REPL: `sigil` with no prompt and a TTY stdin enters a
//! conversational loop. Unlike piped mode (one turn, gate rejects
//! destructive commands), the REPL lets the human approve rejected
//! tool calls interactively.

use std::io::{BufRead, IsTerminal, Write};

use crate::agent::{AgentConfig, Approver};
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

    // True multi-turn context: the message list accumulates across
    // turns. Resumed sessions load their transcript as real messages
    // (not a text blob), so the model sees the actual conversation.
    let mut messages: Vec<crate::llm::ChatMessage> = Vec::new();
    for t in &history {
        match t.role.as_str() {
            "assistant" => messages.push(crate::llm::ChatMessage::assistant(&t.text)),
            _ => messages.push(crate::llm::ChatMessage::user(&t.text)),
        }
    }

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
        // Turn with interactive approval + live streaming output, over
        // the accumulated conversation (multi-turn context).
        messages.push(crate::llm::ChatMessage::user(line));
        let stream_cb = |delta: Option<&str>| {
            match delta {
                Some(d) => {
                    print!("{d}");
                    let _ = std::io::stdout().flush();
                }
                None => {
                    // Tool call interrupted the text stream.
                    eprintln!("\n[using tools…]");
                }
            }
        };
        let answer = match crate::agent::run_messages(
            provider,
            tools,
            config,
            &messages,
            Some(&approver),
            Some(&stream_cb),
        )
        .await
        {
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
        messages.push(crate::llm::ChatMessage::assistant(&answer));
        println!();
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
