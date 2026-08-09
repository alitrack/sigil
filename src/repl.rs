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
        let warn = crate::ui::YELLOW.paint(
            &format!(
                "\n⚠ sigil wants to run a command the gate flags as destructive:\n  {name} {args}\n"
            ),
            &std::io::stderr(),
        );
        eprintln!("{warn}");
        loop {
            let prompt = crate::ui::GREEN.paint("  Allow it? [y/N] ", &std::io::stderr());
            eprint!("{prompt}");
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
    let stderr = std::io::stderr();

    // Session: resume an existing one or start fresh. `session_id`,
    // `history` and `hist_path` are mutable: `/session <id>` can switch
    // mid-REPL.
    let mut session_id = match resume {
        Some(id) => id.to_string(),
        None => crate::session::start(sessions_dir)?,
    };
    let mut history = crate::session::load(sessions_dir, &session_id)?;
    if !history.is_empty() {
        println!(
            "{}",
            crate::ui::CYAN.paint(
                &format!("(resumed session {session_id} — {} turns)", history.len()),
                &stderr
            )
        );
    }

    println!(
        "{}",
        crate::ui::BOLD.paint("sigil — Rust + LuaJIT coding agent (interactive)", &stderr)
    );
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

    // Line editor: created once so history accumulates within the
    // session; saved history is loaded so arrow keys work across
    // sessions.
    let mut hist_path = sessions_dir.join(format!("{session_id}.history"));
    let mut editor = match rustyline::DefaultEditor::new() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("✗ line editor error: {e}");
            return Ok(());
        }
    };
    if let Ok(h) = std::fs::read_to_string(&hist_path) {
        for l in h.lines() {
            let _ = editor.add_history_entry(l);
        }
    }

    loop {
        let Ok(line) = editor.readline("❯ ") else {
            println!();
            break; // EOF (Ctrl-D) or Ctrl-C
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match line {
            "/quit" | "/exit" | "/q" => break,
            "/help" | "/h" => {
                println!("  /tools    list available tools");
                println!("  /tools <q> search tools (name/description substring)");
                println!("  /session  show this session id");
                println!("  /sessions list all sessions (or /session <id> to switch)");
                println!("  /quit     exit");
                println!("  anything else is sent to the agent");
                continue;
            }
            "/session" => {
                println!("  session id: {session_id}");
                continue;
            }
            cmd if cmd.starts_with("/session ") => {
                // Switch to another session: load its transcript as the
                // conversation context (like --resume, mid-REPL).
                let target = cmd.trim_start_matches("/session ").trim();
                if target.is_empty() {
                    println!("  usage: /session <id>");
                    continue;
                }
                match crate::session::load(sessions_dir, target) {
                    Ok(turns) => {
                        session_id = target.to_string();
                        history = turns.clone();
                        messages.clear();
                        for t in &history {
                            match t.role.as_str() {
                                "assistant" => {
                                    messages.push(crate::llm::ChatMessage::assistant(&t.text))
                                }
                                _ => messages.push(crate::llm::ChatMessage::user(&t.text)),
                            }
                        }
                        // Point the line-editor history at the new session.
                        let new_hist = sessions_dir.join(format!("{session_id}.history"));
                        let _ = editor.save_history(&hist_path);
                        hist_path = new_hist.clone();
                        if let Ok(h) = std::fs::read_to_string(&new_hist) {
                            for l in h.lines() {
                                let _ = editor.add_history_entry(l);
                            }
                        }
                        println!(
                            "  switched to session {session_id} ({} turns)",
                            history.len()
                        );
                    }
                    Err(e) => println!("  no such session {target}: {e}"),
                }
                continue;
            }
            "/sessions" => {
                match crate::session::list(sessions_dir) {
                    Ok(ids) if ids.is_empty() => println!("  no sessions yet"),
                    Ok(ids) => {
                        println!("  {} session(s):", ids.len());
                        for id in ids {
                            let turns = crate::session::load(sessions_dir, &id)
                                .map(|t| t.len())
                                .unwrap_or(0);
                            let marker = if id == session_id { " ◀ current" } else { "" };
                            println!("    {id} — {turns} turns{marker}");
                        }
                    }
                    Err(e) => println!("  error listing sessions: {e}"),
                }
                continue;
            }
            "/tools" => {
                for spec in tools.specs() {
                    let name = crate::ui::CYAN.paint(&spec.name, &stderr);
                    println!("  {name} — {}", spec.description);
                }
                continue;
            }
            cmd if cmd.starts_with("/tools ") => {
                // /tools <query>: fuzzy-ish substring filter on name+desc.
                let q = cmd.trim_start_matches("/tools ").trim().to_lowercase();
                let mut hits: Vec<_> = tools
                    .specs()
                    .into_iter()
                    .filter(|s| {
                        s.name.to_lowercase().contains(&q)
                            || s.description.to_lowercase().contains(&q)
                    })
                    .collect();
                hits.sort_by(|a, b| a.name.cmp(&b.name));
                if hits.is_empty() {
                    println!("  no tools match '{q}'");
                } else {
                    println!("  {} tool(s) matching '{q}':", hits.len());
                    for spec in hits {
                        let name = crate::ui::CYAN.paint(&spec.name, &stderr);
                        println!("  {name} — {}", spec.description);
                    }
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
        let _ = editor.add_history_entry(line);
        let _ = editor.save_history(&hist_path);
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
                    let tag = crate::ui::YELLOW.paint("[using tools…]", &std::io::stderr());
                    eprintln!("\n{tag}");
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
                let msg = crate::ui::RED.paint(&format!("✗ error: {e}"), &stderr);
                eprintln!("{msg}");
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
