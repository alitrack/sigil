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

/// Ask the human at the terminal: print the rejected call, y/N.
///
/// Blocking on purpose — the REPL owns the terminal and nothing else can
/// progress while a person decides.
fn ask_terminal(name: &str, args: &serde_json::Value) -> bool {
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
}

/// Terminal approval channel: resolves as soon as the human answers. A daemon
/// or ACP channel would instead resolve when its host replies over the socket
/// or protocol — that is why [`Approver`] returns a future.
fn interactive_approver() -> Box<Approver> {
    Box::new(|name: String, args: serde_json::Value| {
        Box::pin(async move { ask_terminal(&name, &args) })
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

    // Resolve the sessions dir to an absolute path up front: entering a
    // session may chdir back to its project, which would strand a
    // relative --sessions-dir.
    let sessions_dir_abs = match std::fs::canonicalize(sessions_dir) {
        Ok(p) => p,
        Err(_) => sessions_dir.to_path_buf(),
    };
    let sessions_dir = &sessions_dir_abs;

    // Session: resume an existing one or start fresh. `session_id`,
    // `history` and `hist_path` are mutable: `/session <id>` can switch
    // mid-REPL. `--resume latest` (and `/session latest`) resume the
    // most recently active session.
    let resume_id = match resume {
        Some("latest") => crate::session::latest(sessions_dir)?.unwrap_or_else(|| {
            eprintln!("  no sessions yet — starting a new one");
            crate::session::start(sessions_dir).unwrap_or_default()
        }),
        Some(id) => id.to_string(),
        None => crate::session::start(sessions_dir)?,
    };
    // Return to the project this session was working in (its newest
    // SessionStart event) and record the effective cwd. The REPL is a
    // dedicated process, so the process chdir is safe; a failure must
    // not kill the loop.
    if let Err(e) = crate::session::enter_session(sessions_dir, &resume_id) {
        eprintln!("  warning: could not restore session cwd: {e}");
    }
    let mut session_id = resume_id;
    let mut history = crate::session::load(sessions_dir, &session_id)?;

    // Phase 63 event-sourced session logging: wire a session sink into
    // the agent config so tool calls/results inside the loop are
    // appended to the current session's event log. `session_id` is
    // mutable (`/session <id>` switches mid-REPL), so the sink reads
    // the current id through a shared cell.
    let sid_shared = std::sync::Arc::new(std::sync::Mutex::new(session_id.clone()));
    let mut config = config.clone();
    {
        let sink_dir = sessions_dir.to_path_buf();
        let sink_id = sid_shared.clone();
        config.session_sink = Some(std::sync::Arc::new(move |ev: crate::session::Event| {
            let id = match sink_id.lock() {
                Ok(g) => g.clone(),
                Err(p) => p.into_inner().clone(),
            };
            let _ = crate::session::append_event(&sink_dir, &id, &ev);
        }));
    }
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
    let mut messages = crate::session::resume_messages(sessions_dir, &session_id)?;

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
                // `/session latest` switches to the most recent one.
                let raw = cmd.trim_start_matches("/session ").trim();
                let target = if raw == "latest" {
                    match crate::session::latest(sessions_dir)? {
                        Some(id) => id,
                        None => {
                            println!("  no sessions yet");
                            continue;
                        }
                    }
                } else {
                    raw.to_string()
                };
                if target.is_empty() {
                    println!("  usage: /session <id> (or 'latest')");
                    continue;
                }
                match crate::session::load(sessions_dir, &target) {
                    Ok(turns) => {
                        session_id = target.to_string();
                        // Follow the session to its project (and re-record it).
                        if let Err(e) = crate::session::enter_session(sessions_dir, &session_id) {
                            eprintln!("  warning: could not restore session cwd: {e}");
                        }
                        if let Ok(mut g) = sid_shared.lock() {
                            *g = session_id.clone();
                        }
                        history = turns.clone();
                        messages = crate::session::resume_messages(sessions_dir, &session_id)?;
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
                ts: None,
            },
        )?;
        let _ = editor.add_history_entry(line);
        let _ = editor.save_history(&hist_path);
        // Turn with interactive approval + live streaming output, over
        // the accumulated conversation (multi-turn context).
        messages = crate::agent::messages_for_turn(&messages, config.system_prompt_for(line));
        messages.push(crate::llm::ChatMessage::user(line));
        let streamed = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let streamed_for_cb = streamed.clone();
        let stream_cb = move |delta: Option<&str>| {
            match delta {
                Some(d) => {
                    print!("{d}");
                    let _ = std::io::stdout().flush();
                    if let Ok(mut acc) = streamed_for_cb.lock() {
                        acc.push_str(d);
                    }
                }
                None => {
                    // Tool call interrupted the text stream.
                    let tag = crate::ui::YELLOW.paint("[using tools…]", &std::io::stderr());
                    eprintln!("\n{tag}");
                }
            }
        };
        let result = match crate::agent::run_messages(
            provider,
            tools,
            &config,
            &messages,
            Some(&approver),
            Some(&stream_cb),
        )
        .await
        {
            Ok(r) => r,
            Err(e) => {
                let msg = crate::ui::RED.paint(&format!("✗ error: {e}"), &stderr);
                eprintln!("{msg}");
                continue;
            }
        };
        if result.state == crate::agent::RunState::StepLimitReached {
            let msg = crate::ui::YELLOW.paint(
                &format!(
                    "⚠ step limit reached after {} steps — answer is a wrap-up summary",
                    result.steps
                ),
                &stderr,
            );
            eprintln!("\n{msg}");
        }
        messages = result.messages;
        // (assistant turn is logged by the loop's session sink — repl wired
        //  it above; run_messages emits Assistant/TurnEnd on every success
        //  exit. Appending here too would double-log the answer.)
        // A floor answer (first-frame draft / "no usable answer" verdict) is
        // never streamed, so the turn would otherwise end in a blank line.
        let streamed = streamed.lock().map(|s| s.clone()).unwrap_or_default();
        if let Some(answer) = crate::agent::unstreamed_answer(&streamed, &result.output) {
            if !streamed.is_empty() && !streamed.ends_with('\n') {
                println!();
            }
            println!("{answer}");
        }
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
