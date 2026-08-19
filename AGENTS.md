# AGENTS.md — Work Contract for AI Agents

sigil is a Rust + LuaJIT coding agent with a self-evolving harness
(Phase 0-13 complete: pipe mode, interactive REPL with true multi-turn
context, scripted LuaJIT tools with real FFI, self-evolution loop
(create_lua_tool + improve_harness), skill content inlining, three-layer
safety, MCP client **and server**, ACP server **and client**
(cross-process delegation), persistent memory, resumable sessions with
in-REPL switching, sub-agents, streaming output, ratchet tests). This
file tells agent tooling how to work here without breaking things.

## Build & Test

```bash
cargo build                # debug
cargo build --release      # release (strip + lto, single binary)
cargo test                 # unit tests (45: agent, safety, lua, evolve, harness, mcp, acp, session, ...)
cargo fmt -- --check       # must be zero diff
cargo clippy --all-targets -- -D warnings   # must be zero warnings
```

- Rust 2024 edition, `rust-version = "1.97"`.
- Crate name is `sigil-agent` (crates.io name), binary is `sigil`.
- CI (.github/workflows/ci.yml): fmt → clippy → test → release build, with
  `--locked` so Cargo.lock drift is a hard failure.
- Local regression battery (needs the real LLM endpoint):
  `python3 test-tools/ratchet.py` — fixed tasks asserting core behavior.
- E2E protocol scripts: `test-tools/acp_e2e_client.py` (ACP),
  `test-tools/mcp_sigil_client.py` (MCP server), `test-tools/mcp_echo_server.py` (MCP client).

## Architecture

```
src/
├── main.rs          # entry: CLI parse → unified init → route (pipe/REPL/acp/mcp)
├── cli.rs           # clap: subcommands (Prompt/Acp/Mcp) + global args (--model, --base-url, --resume, ...)
├── config.rs        # TOML + env + CLI priority; [llm]/[agent]/[[mcp.servers]]
├── agent/mod.rs     # loop: stream → replay tool calls → execute → repeat; run_messages core
│   └── tests.rs     # unit tests (tool-call extraction, SSE parsing, native calls)
├── repl.rs          # interactive REPL: rustyline editing, sessions, approval, streaming
├── session.rs       # JSONL session store (~/.sigil/sessions/), --resume
├── safety/mod.rs    # approval gate: destructive-command detection; CircuitBreaker
├── harness/mod.rs   # bundle: system_prompt.md + skills/ from %APPDATA%\sigil\harness (Win) / ~/.config/sigil/harness (Linux)
│   └── harness_tests.rs  # skill inlining (small inline / large index-only)
├── llm/
│   ├── mod.rs       # re-exports the provider surface
│   ├── provider.rs  # LlmProvider trait + ChatRequest/ChatMessage/ToolSpec/ToolCall
│   └── openai.rs    # OpenAI-compatible client: chat_stream + chat_complete (tool replay)
├── lua/mod.rs       # embedded LuaJIT (mlua vendored) + ffi; scripted-tool runner
├── memory/mod.rs    # persistent memory file (remember/recall tools)
├── mcp/
│   ├── mod.rs       # MCP client (stdio spawn, env_clear, tools discovery)
│   ├── server.rs    # MCP server mode (sigil mcp): expose registry to clients
├── acp/
│   ├── mod.rs       # ACP server mode (sigil acp): handle_message + serve
│   ├── client.rs    # ACP client: spawn external ACP server, prompt, collect reply
│   └── client_tests.rs  # client against test-tools/fake_acp_server.py
└── tools/
    ├── mod.rs       # Tool trait + ToolRegistry (unified lookup, hot registration)
    ├── bash.rs      # bash tool
    ├── files.rs     # read_file / write_file
    ├── git.rs       # git_status / git_diff / git_log (read-only)
    ├── scripted.rs  # Lua scripted tools from %APPDATA%\sigil\tools (Win) / ~/.config/sigil/tools (Linux)
    ├── evolve.rs    # create_lua_tool + improve_harness (AHE: tools + instruction layer)
    ├── memory.rs    # remember / recall tools
    ├── mcp.rs       # MCP-backed tools (mcp_<server>_<tool>)
    ├── acp.rs       # acp_call: cross-process delegation to an external ACP server
    └── delegate.rs  # delegate: sub-agent with own tool loop
```

Key contracts:
- `LlmProvider` trait is the only seam between the agent loop and backends.
- `ToolRegistry` is keyed by tool name, registration order preserved for
  stable spec ordering in prompts; `clone_shallow()` shares tools across
  sessions/sub-agents; `drain_hot()` picks up evolution-created tools.
- Native function-calling: streamed delta does NOT carry tool_calls (only
  `finish_reason: "tool_calls"`); the loop replays the request non-streaming
  to get the full `message.tool_calls` payload, then executes and backfills
  tool messages. Preserve this two-phase behavior for any OpenAI-compatible
  backend.
- `run_messages` is the core loop over a pre-built message list; the REPL
  uses it to keep true multi-turn context across turns and sessions.
- `Harness::full_system_prompt` inlines small skills (≤ 2 KiB) directly
  into the prompt (`<skill name=...>` blocks) so the model uses them with
  zero tool calls; larger skills stay index-only (read_file on demand).
- `AcpClient` holds the spawned child for the session lifetime —
  `kill_on_drop` + early drop kills the child mid-protocol (BrokenPipe).

## State Locations on this machine (probed on disk, 2026-08)

sigil on this box is the **Windows** build (repo `D:\wsl2\sigil`,
`target/x86_64-pc-windows-msvc`, `build-win.ps1` / `test-win.ps1`).
Global state is split between two roots — do NOT assume `~/.sigil`
for everything (harness/tools/memory/config follow `dirs::config_dir()`,
sessions/hooks/agents follow `dirs::home_dir()`):

| What | Location on this machine | Code default |
|---|---|---|
| **Global harness** (`system_prompt.md`, `skills/`) | `C:\Users\DecisionLinnc\AppData\Roaming\sigil\harness` | `default_harness_dir() = dirs::config_dir()/sigil/harness` |
| Scripted Lua tools | `C:\Users\DecisionLinnc\AppData\Roaming\sigil\tools` | `config_dir()/sigil/tools` |
| Memory file (`remember`/`recall`) | `C:\Users\DecisionLinnc\AppData\Roaming\sigil\memory.md` | `config_dir()/sigil/memory.md` |
| Config file (absent on this box = built-in defaults) | `C:\Users\DecisionLinnc\AppData\Roaming\sigil\config.toml` | `config_dir()/sigil/config.toml` |
| Sessions (`--resume`), hooks, agents, delegates | `C:\Users\DecisionLinnc\.sigil\` | `dirs::home_dir()/.sigil/...` |

On Linux the same code resolves harness/tools/memory/config to
`~/.config/sigil/` (NOT `~/.sigil/harness`). WSL is installed on this
box (WSL 2.7.11) but no distro is currently accessible from this
account (LxssManager stopped, no LRM store) — the WSL side is not the
active sigil state location. A pinned copy of this fact also exists as
the global skill `harness-locations` (drop-in file kept in this repo at
`harness-skill/harness-locations.md`) for sigil's own sessions.

## Key Patterns

- **Provider-agnostic loop**: the agent loop never names a vendor; everything
  goes through `LlmProvider`.
- **Tools never fatal**: `ToolRegistry::execute` converts tool errors into
  tool output strings so the model sees them — the loop never dies on a tool.
- **Approval gate**: non-interactive mode rejects destructive commands by
  default; interactive mode (REPL) asks the human via `Approver`. Sub-agents
  and ACP/MCP paths run non-interactive (gate rejection stands).
- **Environment hygiene**: MCP servers are spawned with `env_clear()` (never
  leak parent secrets); only PATH/HOME are restored.
- **No blocking_lock / nested block_on**: inside the tokio runtime, use
  `try_lock` or async primitives; the ACP turn executor is an async BoxFuture
  for this reason.
- **Tests are self-contained**: each test uses its own temp dir (shared
  dirs cause parallel-test races — see evolve_tests).

## Risk Gates

- **AUTO-APPROVED**: changes under `src/`, adding cargo dependencies, tests.
- **REQUIRES APPROVAL**: changing `Cargo.toml` package identity (name/edition/
  rust-version), the default `SIGIL_BASE_URL` (points at the user's cc-switch
  proxy 127.0.0.1:15722/v1), CI workflows, LICENSE.
- **BOUNDARY**: `.github/workflows/release.yml` (publishing), anything that
  changes the binary's network default.
- **NEVER**: write API keys / credentials (from cc-switch.db, ~/.codex/
  config.toml, etc.) into any deliverable — values are always redacted.

## Conventions

- Default branch: `main`. README is English-first; Chinese version lives in
  README_cn.md. Keep both in sync when changing README content.
- Architecture diagrams in README use Mermaid (GitLab 9.1.3 compatible:
  no `<br/>`, no `#` in node text, no emoji).
- License: MIT (Copyright alitrack).
- Commit style: conventional commits (`feat:`, `fix:`, `docs:`, `refactor:`).
- Version bumps: README top + README changelog + Cargo.toml version, one
  commit, then tag `vX.Y.Z` for the release workflow.
- Git push to GitHub requires the proxy:
  `https_proxy=http://172.19.112.1:7897 timeout 150 git push origin main`.
