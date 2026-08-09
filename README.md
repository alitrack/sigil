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

# MCP server mode (expose sigil's tools to MCP clients)
sigil mcp
```

sigil is a terminal coding agent built from scratch in Rust. It speaks the
OpenAI-compatible chat API, so vLLM, Ollama, llama.cpp, DeepSeek, and every
OpenAI-compatible endpoint work out of the box. **LuaJIT is embedded in the
binary** as the evolution engine: the model can write its own Lua tools at
runtime, hot-register them, and grow the agent's capabilities from within —
no recompile needed.

> Status: **Phase 16** — `sigil doctor` environment self-check. Pipe
> mode, scripted Lua tools with real FFI, self-evolution loop, three-layer
> safety, MCP client **and server**, ACP server **and client** (cross-process
> delegation), persistent memory, resumable sessions with in-REPL
> switching, sub-agents, streaming output, ratchet regression battery,
> model-driven harness improvement, skill content inlining, TUI colors,
> `/tools` search, and a diagnostic `doctor` subcommand — all end to end.

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
`--memory` (remember/recall store, default `~/.sigil/memory.md`),
`--sessions-dir` (session transcripts, default `~/.sigil/sessions/`),
`--resume <id>` (continue a session), `--list-sessions`.

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
- **Harness self-improvement** — `improve_harness`: the model can edit its
  own `system_prompt.md` (`edit_prompt`) and add skills (`add_skill`) —
  the instruction layer evolves from within, recompile-free.
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
- **MCP server** — `sigil mcp` exposes sigil's own tools to any MCP client
  (Claude Desktop, Cursor, other agents) over stdio.
- **ACP server** — `sigil acp` speaks Agent Client Protocol (JSON-RPC
  over stdio); any ACP host (editors, other agents) can drive sigil.
- **ACP client** — the `acp_call` tool spawns an external ACP server
  (another sigil, codex, an editor agent) and delegates a task to it —
  cross-process delegation, protocol-both-ways.
- **Interactive REPL** — multi-turn conversation with human approval for
  destructive commands.
- **True multi-turn context** — the REPL keeps the real message list across
  turns; `--resume` loads the transcript as actual user/assistant messages
  (the model remembers the conversation, not a text blob).
- **Line editing + history** — rustyline: arrow-key history, inline editing;
  per-session `.history` files persist commands across sessions.
- **Streaming output** — assistant text streams live in pipe and REPL modes
  (`[using tools…]` marks tool calls mid-stream).
- **Resumable sessions** — every REPL turn is recorded to
  `~/.sigil/sessions/<id>.jsonl`; `--resume <id>` continues a conversation.
- **Sub-agents** — `delegate` tool: the main agent spins off an independent
  sub-agent (own conversation, own tool loop) and gets back a summary.
- **Ratchet tests** — `test-tools/ratchet.py` runs a fixed task battery
  through the real binary + real LLM, asserting core behavior holds.

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

## Using sigil with Claude Desktop (MCP)

`sigil mcp` is a standard MCP server over stdio, so Claude Desktop can
use every sigil tool (bash, files, git, Lua scripted tools, memory,
delegate, …). Add this to Claude Desktop's config
(`claude_desktop_config.json`):

```json
{
  "mcpServers": {
    "sigil": {
      "command": "/absolute/path/to/sigil",
      "args": ["mcp"],
      "env": {
        "SIGIL_MODEL": "deepseek-v4-flash",
        "SIGIL_BASE_URL": "http://127.0.0.1:15722/v1"
      }
    }
  }
}
```

Point `command` at `cargo build --release` output, set `env` to your
endpoint, restart Claude Desktop, and the sigil tools appear in the
MCP tool list.

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
| **7** ✅ | MCP server mode (`sigil mcp`) |
| **8** ✅ | Streaming output, ratchet regression battery |
| **9** ✅ | True multi-turn context, rustyline line editing + history |
| **10** ✅ | `improve_harness` — instruction-layer self-evolution |
| **11** ✅ | In-REPL session switching (`/sessions`, `/session <id>`) |
| **12** ✅ | ACP client + `acp_call` — cross-process delegation |
| **13** ✅ | Skill content inlining (small skills embedded in the prompt) |
| **14** ✅ | TUI colors + `/tools <query>` search |
| **15** ✅ | `sigil doctor` environment self-check |
| **16** ✅ | Doctor LuaJIT/FFI check; global CLI args after subcommands |

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
