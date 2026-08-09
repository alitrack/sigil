//! `sigil doctor` — environment self-check.
//!
//! Deterministic diagnostics, no agent loop involved: config loading,
//! endpoint reachability (light probe), harness/tools/memory/sessions
//! dirs, and git availability. Exit code: 0 all good, 1 warnings,
//! 2 hard failures.

use anyhow::Result;

/// Result of one check.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Ok,
    Warn,
    Fail,
}

struct Check {
    level: Level,
    name: String,
    detail: String,
}

pub async fn run(config: &crate::config::Config, cli: &crate::cli::Cli) -> Result<i32> {
    let mut checks: Vec<Check> = Vec::new();
    let mut fail: usize = 0;
    let mut warn: usize = 0;

    // ---- 1. Config file ----
    match &cli.config {
        Some(path) => {
            if std::path::Path::new(path).exists() {
                ok(
                    &mut checks,
                    &mut fail,
                    "config file",
                    format!("found at {path}"),
                );
            } else {
                fail_check(
                    &mut checks,
                    &mut fail,
                    "config file",
                    format!("{path} does not exist"),
                );
            }
        }
        None => {
            let def = crate::config::default_config_path();
            if def.exists() {
                ok(
                    &mut checks,
                    &mut fail,
                    "config file",
                    format!("found at {}", def.display()),
                );
            } else {
                warn_check(
                    &mut checks,
                    &mut warn,
                    "config file",
                    format!("{} not found (defaults used — fine)", def.display()),
                );
            }
        }
    }

    // ---- 2. Endpoint ----
    let base = config.base_url.trim_end_matches('/');
    let url = format!("{base}/models");
    match probe_endpoint(&url, &config.api_key).await {
        Ok(Some(n)) => {
            ok(
                &mut checks,
                &mut fail,
                "endpoint",
                format!("{base} reachable ({} model(s) listed)", n),
            );
        }
        Ok(None) => {
            warn_check(
                &mut checks,
                &mut warn,
                "endpoint",
                format!("{base} responded but listed 0 models (proxy may hide them)"),
            );
        }
        Err(e) => {
            fail_check(
                &mut checks,
                &mut fail,
                "endpoint",
                format!("{base} unreachable: {e}"),
            );
        }
    }

    // ---- 3. Harness dir ----
    let harness_dir = cli
        .harness_dir
        .as_deref()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(crate::harness::default_harness_dir);
    let prompt = harness_dir.join("system_prompt.md");
    if prompt.exists() {
        ok(
            &mut checks,
            &mut fail,
            "harness",
            format!("{} present", prompt.display()),
        );
    } else {
        warn_check(
            &mut checks,
            &mut warn,
            "harness",
            format!(
                "{} absent (default prompt will be written on first run)",
                prompt.display()
            ),
        );
    }
    let skills_dir = harness_dir.join("skills");
    match std::fs::read_dir(&skills_dir) {
        Ok(entries) => {
            let n = entries.flatten().count();
            ok(
                &mut checks,
                &mut fail,
                "skills",
                format!("{} skill file(s) in {}", n, skills_dir.display()),
            );
        }
        Err(e) => {
            warn_check(
                &mut checks,
                &mut warn,
                "skills",
                format!("{}: {e}", skills_dir.display()),
            );
        }
    }

    // ---- 4. Tools dir ----
    let tools_dir = cli
        .tools_dir
        .as_deref()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(crate::tools::scripted::default_tools_dir);
    match std::fs::read_dir(&tools_dir) {
        Ok(entries) => {
            let n = entries
                .flatten()
                .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("lua"))
                .count();
            ok(
                &mut checks,
                &mut fail,
                "tools dir",
                format!("{n} Lua scripted tool(s) in {}", tools_dir.display()),
            );
        }
        Err(e) => {
            warn_check(
                &mut checks,
                &mut warn,
                "tools dir",
                format!("{}: {e}", tools_dir.display()),
            );
        }
    }

    // ---- 5. Memory file ----
    let mem = cli
        .memory
        .as_deref()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(crate::memory::default_memory_path);
    match std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&mem)
    {
        Ok(_) => ok(
            &mut checks,
            &mut fail,
            "memory",
            format!("{} writable", mem.display()),
        ),
        Err(e) => fail_check(
            &mut checks,
            &mut fail,
            "memory",
            format!("{} not writable: {e}", mem.display()),
        ),
    }

    // ---- 6. Sessions dir ----
    let sessions_dir = cli
        .sessions_dir
        .as_deref()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(crate::session::default_sessions_dir);
    match std::fs::create_dir_all(&sessions_dir) {
        Ok(()) => ok(
            &mut checks,
            &mut fail,
            "sessions",
            format!("{} writable", sessions_dir.display()),
        ),
        Err(e) => fail_check(
            &mut checks,
            &mut fail,
            "sessions",
            format!("{} not writable: {e}", sessions_dir.display()),
        ),
    }

    // ---- 7. git ----
    match std::process::Command::new("git").arg("--version").output() {
        Ok(out) if out.status.success() => {
            let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
            ok(&mut checks, &mut fail, "git", v);
        }
        _ => warn_check(
            &mut checks,
            &mut warn,
            "git",
            "git not found — git_* tools will error".to_string(),
        ),
    }

    // ---- 8. LuaJIT / FFI ----
    match lua_check() {
        Ok(detail) => ok(&mut checks, &mut fail, "LuaJIT + FFI", detail),
        Err(e) => fail_check(&mut checks, &mut fail, "LuaJIT + FFI", format!("{e}")),
    }

    // ---- Report ----
    println!("\n  sigil doctor — environment check\n");
    let mut worst = Level::Ok;
    for c in &checks {
        worst = worst.max(c.level);
        let (icon, color) = match c.level {
            Level::Ok => ("✓", crate::ui::GREEN),
            Level::Warn => ("⚠", crate::ui::YELLOW),
            Level::Fail => ("✗", crate::ui::RED),
        };
        let line = color.paint(
            &format!("  {icon} {} — {}", c.name, c.detail),
            &std::io::stdout(),
        );
        println!("{line}");
    }
    println!(
        "\n  {} passed, {} warned, {} failed\n",
        checks.len() - warn - fail,
        warn,
        fail
    );

    Ok(match worst {
        Level::Ok => 0,
        Level::Warn => 1,
        Level::Fail => 2,
    })
}

fn ok(checks: &mut Vec<Check>, _fail: &mut usize, name: &str, detail: String) {
    checks.push(Check {
        level: Level::Ok,
        name: name.into(),
        detail,
    });
}
fn warn_check(checks: &mut Vec<Check>, warn: &mut usize, name: &str, detail: String) {
    *warn += 1;
    checks.push(Check {
        level: Level::Warn,
        name: name.into(),
        detail,
    });
}
fn fail_check(checks: &mut Vec<Check>, fail: &mut usize, name: &str, detail: String) {
    *fail += 1;
    checks.push(Check {
        level: Level::Fail,
        name: name.into(),
        detail,
    });
}

/// Probe `GET {base}/models`. Returns Ok(Some(n)) on success, Ok(None)
/// on 200 with zero models, Err on network/HTTP failure.
async fn probe_endpoint(url: &str, api_key: &str) -> Result<Option<usize>> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .build()?;
    let mut req = client.get(url);
    if !api_key.is_empty() {
        req = req.bearer_auth(api_key);
    }
    let resp = req.send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("HTTP {}", resp.status());
    }
    let body: serde_json::Value = resp.json().await?;
    Ok(body.get("data").and_then(|d| d.as_array()).map(|a| a.len()))
}

/// Check the embedded LuaJIT runtime and real FFI: boot a Lua state
/// and run the crc32 FFI anchor (crc32("hello") == 0x3610a686).
fn lua_check() -> Result<String> {
    let rt = crate::lua::LuaRuntime::new()?;
    let out = rt.run(
        "local ffi = require('ffi')
         ffi.cdef[[unsigned long crc32(unsigned long, const unsigned char*, unsigned long);]]
         local z = ffi.load('z')
         local crc = z.crc32(0, 'hello', 5)
         return string.format('crc32(hello)=%08x', crc)",
    )?;
    let trimmed = out.trim();
    if trimmed.contains("3610a686") {
        Ok(format!(
            "vendored LuaJIT + FFI ok ({trimmed} — real zlib crc32 call)"
        ))
    } else {
        anyhow::bail!("FFI crc32 anchor mismatch: {trimmed}")
    }
}

#[cfg(test)]
mod doctor_tests;
