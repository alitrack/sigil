# sigil

**A Rust + LuaJIT coding agent with a self-evolving harness.**

One binary. Bring any model. The harness evolves itself.

```bash
# One-shot pipe mode
echo "fix the bug in src/main.rs" | sigil

# Interactive REPL (TTY): multi-turn conversation + human approval
sigil

# ACP server mode (drive sigil from editors / other agents)
sigil acp
```

sigil is a terminal coding agent built from scratch in Rust. It speaks the
OpenAI-compatible chat API, so vLLM, Ollama, llama.cpp, DeepSeek, and every
OpenAI-compatible endpoint work out of the box. **LuaJIT is embedded in the
binary** as the evolution engine: the model can write its own Lua tools at
runtime, hot-register them, and grow the agent's capabilities from within —
no recompile needed.

> Status: **Phase 6** — session persistence + sub-agents. Pipe mode, scripted
> Lua tools with real FFI, self-evolution loop, three-layer safety, MCP
> client, ACP server, persistent memory, interactive REPL, resumable
> sessions, and sub-agent delegation all work end to end.

## Quick start

```bash
cargo build --release

# Pipe mode (env-var configuration; defaults to cc-switch proxy + deepseek-v4-flash)
echo "what files are in this repo?" | ./target/release/sigil

# Interactive mode
./target/release/sigil
#   /tools    list available tools
#   /session  show this session id
#   /quit     exit

# Resumable sessions
./target/release/sigil --list-sessions
./target/release/sigil --resume <session-id>
```

### Configuration

| Env var | Default | Purpose |
|---------|---------|---------|
| `SIGIL_MODEL` | `deepseek-v4-flash` | Model name |
| `SIGIL_BASE_URL` | `http://127.0.0.1:15722/v1` | OpenAI-compatible endpoint (cc-switch local proxy) |
| `SIGIL_API_KEY` | `EMPTY` | API key (bearer token) |

Any OpenAI-compatible endpoint works via `SIGIL_BASE_URL` — vLLM, Ollama,
llama.cpp, DeepSeek, etc. Config file at `~/.sigil/config.toml`
(`[llm]`, `[agent]`, `[[mcp.servers]]`); precedence is CLI > env > file >
defaults. `--tools-dir` (Lua tools, default `~/.sigil/tools/`),
`--harness-dir` (system prompt + skills, default `~/.sigil/harness/`),
`--memory` (remember/recall store, default `~/.sigil/memory.md`).

## Capabilities

- **Native tool-calling** — streamed completions, non-streaming replay for
  full tool-call payloads (OpenAI function-calling; works against any
  compatible backend).
- **Tools** — `bash`, `read_file`, `write_file`, `git_status/diff/log`,
  plus **Lua scripted tools** (`~/.sigil/tools/*.lua`, filename = tool name,
  `-- desc:` / `-- schema:` headers).
- **Embedded LuaJIT + FFI** — `mlua` with vendored LuaJIT. Scripts can
  `require("ffi")` and call any C library (zlib `crc32` verified end to end).
- **Self-evolution** — `create_lua_tool`: the model writes Lua, sigil saves
  and hot-registers it, and it's callable from the next turn — persisted
  across sessions.
- **Harness bundle** — `~/.sigil/harness/` holds `system_prompt.md` and
  `skills/`; edit files (not code) to change agent behavior.
- **Safety (3 layers)** — approval gate (destructive commands rejected in
  non-interactive mode; interactive `y/N` in the REPL), circuit breaker
  (tool tripped after 3 consecutive failures), execution rings
  (read-only / workspace / harness mutation).
- **Memory** — `remember` / `recall`: persistent cross-session facts the
  model maintains itself.
- **MCP client** — `[[mcp.servers]]` in config spawns stdio MCP servers;
  their tools become `mcp_<server>_<tool>`.
- **ACP server** — `sigil acp` speaks the Agent Client Protocol (JSON-RPC
  over stdio) so any ACP host (editors, other agents) can drive sigil.
- **Interactive REPL** — multi-turn conversation with human approval for
  destructive commands.
- **Resumable sessions** — every REPL turn is recorded to
  `~/.sigil/sessions/<id>.jsonl`; `--resume <id>` continues a conversation.
- **Sub-agents** — `delegate` tool: the main agent spins off an independent
  sub-agent (own conversation, own tool loop) and gets back a summary.

## How it works

```
stdin / REPL / ACP ──► agent loop ──► LlmProvider (OpenAI-compatible stream)
                          │  ▲
                          ▼  │
                    ToolRegistry (bash, files, git, Lua, MCP, memory)
                          │
                    safety (gate / breaker / ring)
```

- **`llm::LlmProvider`** — the provider contract; every backend implements
  it, so the agent loop stays provider-agnostic.
- **`agent::run_turn(_with_approver)`** — the loop: stream → replay for tool
  calls → approve → execute → repeat until the model stops (or `max_steps`).
- **`tools::ToolRegistry`** — one lookup for all tools; specs are refreshed
  every turn so hot-registered tools appear to the model immediately.
- **`lua::LuaRuntime`** — vendored LuaJIT with `ffi` enabled; `host`
  helper table (`read_file` / `write_file` / `env`) is the script↔host
  boundary.

## Roadmap

| Phase | Scope |
|-------|-------|
| **0** ✅ | Scaffold, `LlmProvider`, ReAct loop, bash tool, pipe mode, native tool-calling |
| **1** ✅ | File/git tools, approval gate, TOML config |
| **2** ✅ | **Embedded LuaJIT (mlua vendored) + FFI tool surface** |
| **3** ✅ | Harness bundle + self-evolution loop (AHE) + safety 3 layers |
| **4** ✅ | MCP client, ACP server, persistent memory |
| **5** ✅ | Interactive REPL + human approval |
| **6** ✅ | Session persistence (`--resume`), sub-agents (`delegate`) |
| **7** 🔮 | MCP server mode, TUI polish, ratchet tests |

## Development

Rust 2024, single binary. `tokio`, `reqwest` (rustls — no OpenSSL link
dependency), `clap`, `serde`, `mlua` (LuaJIT + vendored).

```bash
cargo fmt -- --check
cargo clippy --all-targets -- -D warnings
cargo test
```

## License

MIT. See [LICENSE](LICENSE). Copyright (c) 2026 alitrack.

中文版说明见 [README_cn.md](README_cn.md).
