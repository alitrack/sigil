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
cargo test --locked        # unit tests — `cargo test` output is the count's source of truth
cargo fmt -- --check       # must be zero diff
cargo clippy --all-targets -- -D warnings   # must be zero warnings
```

Do not hard-code the test count in docs: it drifts. CI runs three
clippy/test configurations (default, `--no-default-features` containment
build, `--features plan`); all three must stay green.

### Frozen feature: `plan`

`src/plan/` (the dynamic-workflow plan DSL) is built **only** with
`--features plan`; it is not part of the default surface. It is the fourth
planning mechanism in the tree and its `run_plan` entry point has no caller
(`sigil plan` exposes `check` / `price` only). Treat it as parked: keep the
feature compiling and tested, do not extend it, and do not wire it into the
agent loop before the candidate→evaluate→promote gate exists. Rationale:
`review-multidomain-263bd58.md` §3.1.

### Build modes (Lua sandbox)

Default (`lua-ffi` feature ON): scripted tools may `require("ffi")` and
load system C libraries — the core self-evolution capability; the
sandbox is intentionally porous (documented risk, see src/lua/mod.rs).

Containment build: `cargo build --no-default-features` strips the FFI
library, its `package.preload`/`loaded` entries, and the require
allowlist entry — no Lua script can reach C libraries. Use it for
untrusted/leveraged environments. Verify with
`cargo test --no-default-features` (FFI-dependent tests are feature-
gated; a dedicated containment test asserts FFI is unreachable).

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
│   └── gate.rs      # ToolGate: THE tool-execution chokepoint (gate → run → observe)
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

## State Locations on this machine (probed on disk; re-probed 2026-08-26)

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
box (WSL 2.7.11) but no distro is accessible from this account
(`wsl -l -v` → E_ACCESSDENIED) — the WSL side is not the active sigil
state location. This fact is pinned for sigil's own sessions as the
global skill `harness-locations`: source of truth is
`harness-skill/harness-locations.md` in this repo; the live installed
copy sigil loads is
`%APPDATA%\sigil\harness\skills\harness-locations.md`. The 2026-08-26
re-probe found that installed copy **still missing** (global `skills/`
empty; the installed `system_prompt.md` is the 380 B built-in default,
i.e. not customized). Agent sandboxes cannot write outside the
workspace (escalation fails closed without an approval channel), so
(re)install from a full-access shell:
`Copy-Item harness-skill\harness-locations.md %APPDATA%\sigil\harness\skills\`

## Key Patterns

- **Provider-agnostic loop**: the agent loop never names a vendor; everything
  goes through `LlmProvider`.
- **Tools never fatal**: `ToolRegistry::execute` converts tool errors into
  tool output strings so the model sees them — the loop never dies on a tool.
- **Approval gate**: non-interactive mode rejects destructive commands by
  default; interactive mode (REPL) asks the human via `Approver`. Sub-agents
  and ACP/MCP paths run non-interactive (gate rejection stands).
  `Approver` returns a **future** on purpose — a synchronous callback can only
  re-prompt a terminal, which is why only the REPL ever had one. Wiring the
  remaining channels is not done yet, and each needs its own plumbing:
  **daemon: wired** (`daemon.rs`) — a reader task owns the socket lines so it
  can resolve `{"type":"approval_reply","id":N,"allow":bool}` while the turn
  runs inline; the turn itself stays in the loop, which is what keeps
  `ToolRegistry` out of a lock. No reply within 300s, or a client that
  disconnects, denies (fail closed).
  **ACP: still missing** — needs a `session/request_permission` round-trip.
  Known limit: `safety::check` only pattern-matches `bash`; every other tool
  is `Allow`. The write-scope and freeze rules below are what bound them.
- **One execution path**: every tool call goes through `agent::ToolGate`
  (`gate` → `run` → `observe`). The loop and the MCP server both use it, so
  approval, hooks, the circuit breaker and the session sink cannot diverge.
  Do not call `ToolRegistry::execute*` directly from a new path — that is how
  EDG ended up with five P0 bypasses. Sub-agents inherit it by running
  `run_messages`.
- **Write scope**: `write_file` goes through `safety::authorize_write`, which
  confines model-directed writes to the workspace, the temp dir and sigil's
  state dirs (`SIGIL_WRITE_ROOTS` extends it). Escapes via `..`, absolute
  paths outside the roots and sibling-prefix lookalikes are refused.
- **Hot registration is process-shared — one known limit**: evolution tools
  push `HotOp`s into the `hot_queue` they captured at `with_tools()` time, and
  that `Arc` is inherited by every `clone_shallow()` (sub-agents, MCP server,
  daemon connections, cron). Whoever drains first consumes the op, so a tool
  created in one session can become callable in another without a restart.
  The tools *directory* is global by design, so this only skips the restart —
  but true per-session scoping needs the evolution tools re-bound to a
  per-registry queue, which is not done yet. A frozen run now refuses the
  whole drain (below), which is the case that mattered for benchmarking.
- **Credential reads**: `safety::authorize_read` + `safety::is_credential_env`.
  A *denylist*, deliberately: an allowlist would break real work (agents read
  `/etc/os-release`, `/usr/include`, a project `.env`), while `~/.ssh`,
  `~/.aws`, `~/.netrc`, proxy stores and secret-named env vars are never part
  of the task and are exactly what injected text goes after. Both enforced in
  `read_file`, Lua `host.read_file` and Lua `host.env`.
- **Freeze**: `safety::is_frozen()` (`SIGIL_FREEZE=1`) is the single check
  every write surface calls (remember, memory_delete, create_lua_tool,
  improve_harness, hooks). Add it to any new write tool.
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
- Version bumps: README top + README release notes + CHANGELOG.md/CHANGELOG_cn.md
  phase entries + Cargo.toml version, one commit, then tag `vX.Y.Z` for
  the release workflow.
- Git push to GitHub requires the proxy:
  `https_proxy=http://172.19.112.1:7897 timeout 150 git push origin main`.
