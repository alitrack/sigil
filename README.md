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

> Status: **Phase 45** — ratchet in CI + goal turn-budgets: the ratchet
> now has a deterministic `--check-only` mode (task schema + source
> anchor drift detection) wired into CI, and goals support an optional
> `turn_budget` — one LLM call = one turn, auto-pause when exhausted.
> Before: Phase 44 daemon, Phase 43 async delegation, Phase 42 goals,
> Phase 41's harness
> refinement gate + token-budget compression, Phase 39's memory write
> safety, Phase 38's LLM failover, Phase 36's session continuity and
> Phase 35's cross-instance knowledge sync.
> Pipe mode, scripted Lua tools with real FFI and host helpers (tool
> create/update/delete/list at runtime), dynamic skill selection with
> **declared dependencies** (`> depends_on:` header — dependencies are
> inlined transitively, cycle-safe) and **cycle detection** (add_skill
> warns the model when a write creates a dependency cycle; `list_skills`
> shows every skill with its description, dependencies, and any cycles
> as a warning), a self-evolution loop with **version history, rollback
> that hot-reloads in-session, garbage collection, and live prompt
> state** (harness edits apply on the very next turn, no restart) for
> every model-writable asset, three-layer safety, MCP client **and
> server** (tools, progress notifications, resources **and prompts**
> both ways), ACP server **and client** (cross-process delegation),
> persistent memory (**remember/recall/memory_list/memory_delete**),
> resumable sessions with in-REPL switching and **portable JSON
> export/import**, sub-agents, streaming output, a **10-task ratchet
> regression battery** (files/bash/git, memory lifecycle, skill deps,
> live prompt edits, tool creation, MCP server — all against the real
> binary and LLM), model-driven harness improvement, skill content
> inlining, TUI colors, `/tools` search, and a diagnostic `doctor`
> subcommand — all end to end.

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
| **17** ✅ | Shell completions; v0.1.0 release (5-platform binaries) |
| **18** ✅ | Dynamic skill selection; MCP progress notifications |
| **19** ✅ | Scripted-tool host helpers; MCP resources (harness) |
| **20** ✅ | Tool lifecycle (create/update/delete/list); MCP resource reading |
| **21** ✅ | MCP prompts (templates both ways) |
| **22** ✅ | Version library — reversible self-evolution (rollback) |
| **23** ✅ | Rollback hot-reload; version GC; MCP hot-queue drain |
| **24** ✅ | Session export/import (portable JSON transcripts) |
| **25** ✅ | Live harness state (edits apply next turn, no restart) |
| **26** ✅ | Memory lifecycle (remember / recall / memory_list / memory_delete) |
| **27** ✅ | Skill dependencies (`> depends_on:` header, closure inlining) |
| **28** ✅ | Ratchet battery 4 → 10 (memory, skill deps, live prompt, MCP) |
| **29** ✅ | Dependency cycle detection (add_skill warns on cycles) |
| **30** ✅ | Skill graph view (list_skills: descriptions, deps, cycle warning) |
| **31** ✅ | Timestamped turns + `session export --since` (time-filtered transcripts) |
| **32** ✅ | Cross-process dir locks (advisory `.sigil.lock` on evolution writes) |
| **33** ✅ | Ratchet +2 (ACP e2e, session export/import) + scripted-tool schema hardening |
| **34** ✅ | On-demand skill loading (compact always-on index, bodies read on demand) |
| **35** ✅ | Cross-instance knowledge sync (per-turn harness fingerprint → live reload) |
| **36** ✅ | Session continuity (`session list` summaries + `--resume latest`) |
| **37** ✅ | Ratchet LLM-variance tolerance (per-task retries + retry_prompt) |
| **38** ✅ | LLM failover (endpoint chain: transient retry, 4xx abort) |
| **39** ✅ | Memory write safety (remember/delete under cross-process lock) |
| **40** ✅ | Robustness (tool timeouts + context compression + atomic export) |
| **41** ✅ | Harness refinement gate (reason-gated edits + audit log; token-budget compression) |
| **42** ✅ | Persistent goals (manage_goals tool + per-turn context injection + goals.jsonl) |
| **43** ✅ | Async delegation (spawn → handle → status/result/list registry) |
| **44** ✅ | Background daemon (Unix socket agent loop + attach/send + session persistence) |
| **45** ✅ | Ratchet in CI (--check-only) + goal turn-budgets (auto-pause) |

## Changelog

### Phase 45 — 2026-08-10
- **Ratchet in CI**: `ratchet.py --check-only` is a deterministic,
  no-LLM validation (task schema + source-anchor drift detection) —
  wired into the CI pipeline. Catches the P44-style failure where a
  changed source line silently broke a file-anchored assertion.
- **Goal turn-budgets**: `manage_goals add` accepts an optional
  `turn_budget` (one LLM call = one turn). Active goals show
  `(turns N/M)` in the injected block and `manage_goals list`; the
  goal auto-pauses once the budget is spent and stops being injected.
  Pre-budget goals.jsonl files load unchanged (serde defaults).
- 120 tests; ratchet 12/12; E2E: goal-budget flow (add → 2 turns →
  auto-pause persisted → no further injection) green.

### Phase 44r — 2026-08-10 (review fixes)
Systematic review of Phases 41–44 (compression, refinement gate,
goals, async delegation, daemon). Three real bugs found and fixed,
plus one doc correction:
- **Goals duplicated every agent-loop iteration**: the injection ran
  unconditionally inside the tool loop, so a multi-tool turn stacked
  an extra `## Active goals` system message per iteration. Now
  keyed by `GOALS_MARKER` — re-injection *replaces* the previous
  block in place (`inject_goals_block`, regression-tested).
- **Delegate registry silently dropped updates**: `insert`/`update`/
  `snapshot` used `try_lock()` and discarded the result on lock
  contention — a sub-agent finishing while the main agent polled
  could lose its result forever. Now `lock().await` (async methods,
  regression-tested with 8 concurrent sub-agent updates).
- **Daemon `--resume latest` not resolved**: the daemon loaded a
  session literally named "latest" (error). Now resolves to the most
  recently active session, matching the REPL (E2E: daemon#1 → 42,
  restart with `--resume latest` → 43).
- `record_refinement` doc corrected (append-only via O_APPEND, not
  the dir lock).
- 117 tests; ratchet 12/12; E2E: goals flow, delegate flow, daemon
  resume — all green.

### Phase 44 — 2026-08-10
- **Background daemon** (herdr-inspired: "the runtime your coding
  agents live on"): `sigil daemon` serves the agent loop on a Unix
  socket (`~/.sigil/daemon.sock` by default); `sigil attach` is the
  interactive client, `sigil send "message"` the one-shot client —
  CLI and socket speak the same JSON-line surface. The daemon owns
  the loop: a client can disconnect mid-run and the task keeps
  going; multi-turn context is carried across connections; every
  turn is appended to the session file (resumable with
  `--resume <id>` when the daemon restarts). Turns are serialized
  with a loop lock (FIFO across concurrent clients); deltas stream
  to the requesting client via a channel.
  - Also fixed: the REPL never sent the system prompt (multi-turn
    history only); the daemon always sends
    `system_prompt_for(user)` + history + the new turn.
  - Tests: 115 total — role mapping, one-shot protocol parsing
    (delta + done), daemon lifecycle E2E.
  - E2E: daemon up → `sigil send` replies "42" → second `send`
    replies "43" (context carried across connections) → daemon
    killed → session file has 4 turns (user/assistant × 2).

### Phase 43 — 2026-08-10
- **Async sub-agent delegation** (mirrors prime-agent's `rlm(...)`
  handle semantics): `delegate` gained a registry — `action=spawn`
  starts the sub-agent and returns a handle (`d00000`) immediately;
  it keeps running in the background (tokio task) while the main
  agent works. `action=status` polls running/completed/failed,
  `action=result` collects the final summary, `action=list`
  enumerates all sub-agents. The result still never pollutes the
  main conversation — only the summary text does.
  - Tests: 113 total — delegate schema (4 actions), registry
    lifecycle tracking.
  - E2E: model spawns a sub-agent → handle returned immediately; the
    next turn polls status and collects the result (echo via bash).

### Phase 42 — 2026-08-10
- **Persistent goals** (inspired by prime-agent's `GoalState`):
  `manage_goals` lets the model declare long-running objectives
  (add/complete/pause/resume/list/clear). Active goals are injected
  into the system context every turn right after the first system
  message (never folded away by compaction), and persisted to
  `harness_dir/goals.jsonl` under the directory lock (atomic tmp +
  rename), so goals survive restarts and are shared across sigil
  instances. Completed goals stop being injected.
  - Quality gates: objectives must be ≥ 8 chars (one clear sentence);
    unknown ids and unknown actions are rejected; corrupt jsonl lines
    degrade gracefully.
  - Tests: 112 total — goals roundtrip, active-block formatting,
    add/list/complete/pause/resume flow, noise rejection.
  - E2E: a model adds a goal → `goals.jsonl` on disk; a fresh sigil
    process sees it injected into its context; completing it stops
    the injection.

### Phase 41 — 2026-08-10
- **Harness refinement gate** (inspired by prime-agent's `/refine`):
  `improve_harness` writes (`edit_prompt` / `add_skill`) now require a
  `reason` field — one sentence of evidence why the change is a
  durable, generally-useful improvement — and pass deterministic
  quality gates: empty content, identical-to-current rewrites,
  too-short prompts (< 100 chars), header-less skills and duplicate
  skills are all rejected before any write. Every accepted edit is
  appended to `harness_dir/refinements.jsonl` (ts / action / name /
  reason / result) for audit, inside the existing directory lock.
- **Token-budget compression**. `compact_messages_with_budget` adds a
  chars/4 token budget (default 64k) on top of the message-count
  budget: the kept tail is chosen by walking backwards accumulating
  token estimates. The summary now also tracks `Files read:` /
  `Files modified:` from `read_file`/`write_file` tool calls, so a
  compressed session tells the model which files it touched
  (prime-agent tracks the same via `CompactionEntry.details`).
  - Tests: 107 total — token-budget compression, file-op tracking,
    refinement gate (missing reason / noop rewrite / noise shape /
    audit log).
  - E2E: a model calling `improve_harness` without `reason` is
    rejected and the prompt file stays untouched; with `reason` the
    skill is written and `refinements.jsonl` grows.

### Phase 40 — 2026-08-10
- **Tool timeouts**. `bash` and `git` ran `tokio::process::Command`
  with no wall-clock budget — a model asking for `sleep 10000` (or a
  hung git fetch) hung the agent loop forever. Both now go through
  `run_with_timeout` (60s budget, `kill_on_drop` so no orphan keeps
  running) and a timeout is reported to the model as a normal tool
  output (`[bash: command timed out after 60s — process killed]`),
  letting the loop continue. Scripted Lua tools already had a similar
  timeout; this closes the native-tool gap.
- **Context compression**. REPL sessions grew the transcript without
  bound — every turn re-sent the whole history. `compact_messages`
  folds the oldest turns into one deterministic summary `system`
  message (role-tagged one-line excerpts, no LLM call) once the
  transcript exceeds `max_context_messages` (default 40, `0` disables).
  Cutting only happens at safe boundaries (a `user` or tool-free
  `assistant` message), so the kept tail never orphans a `tool` result
  from its tool-call message.
- **Atomic session export**. `session export` wrote the archive with a
  plain `fs::write` — a crash mid-write left a truncated file. It now
  writes to `<name>.json.tmp` then renames over the target (atomic on
  the same filesystem), cleaning the temp file on error.
  - Tests: `run_with_timeout_returns_output_for_fast_command`,
    `run_with_timeout_kills_slow_command`, `compact_*` ×4, tmp-leak
    assertion in export roundtrip. 100 total.
  - E2E: bash `sleep 120` returns the timeout note in ~60s without
    hanging; export roundtrip valid JSON, no `.tmp` leftover.

### Phase 39 — 2026-08-10
- **Memory write safety**. `remember` and `delete` were plain
  read-modify-write (`read_to_string` → edit → whole-file `write`):
  two concurrent sigil instances appending facts could lose each
  other's entries (last writer wins). Both now run their full
  read-check-append-write / read-edit-write sequence under the Phase 32
  cross-process directory lock (`with_dir_lock` on the memory file's
  parent, falling back to `.` for a bare filename). Reads (`recall`,
  `list`) stay lock-free.
  - Tests: `concurrent_remembers_lose_nothing` (2×25 interleaved
    appends, all 50 facts survive), `concurrent_delete_and_remember_are_consistent`
    (append vs delete race keeps the file valid). 94 total.
  - E2E: two real pipe-mode sigil processes racing 10 remembers each —
    all 20 facts present, zero loss.

### Phase 38 — 2026-08-10
- **LLM failover**. The OpenAI-compatible provider now holds an endpoint
  chain: the primary plus optional fallbacks. On transient failures
  (connection error, 5xx, 429) it retries the next endpoint with a
  small backoff (300ms × index); 4xx client errors (bad schema, auth,
  unknown model) abort immediately — retrying another endpoint would
  not help. Configure via `[llm] fallbacks = [{ base_url, api_key }]`
  in config.toml or `SIGIL_FALLBACK_BASE_URL` (comma-separated).
  - `OpenAiProvider::with_fallbacks(config, fallbacks)` (replaces `new`);
    `PostError::{Fatal, Transient}` classification; `post_with_fallback`
    chain loop with per-endpoint stderr notice.
  - Tests: `falls_back_to_second_endpoint_when_primary_is_down`,
    `falls_back_on_server_error_but_not_on_client_error`,
    `all_endpoints_down_reports_each_failure` (local TCP fake
    endpoints, no network). 92 total.
  - E2E: dead primary + cc-switch fallback → model answers with the
    `[sigil] primary endpoint failed` notice; live primary → no notice.

### Phase 37 — 2026-08-10
- **Ratchet LLM-variance tolerance**. `live_prompt_edit` occasionally
  fails because the model *claims* completion without calling the tool
  (sampling randomness, seen in Phases 34/36). Instead of relaxing
  assertions, LLM-driven tasks now support per-task retries:
  `retries: N` + `retry_prompt` with explicit "you MUST call the tool"
  phrasing. The main loop retries on assertion miss or timeout; live
  tasks restore the baseline `system_prompt.md` before each retry.
  Invisible when the model passes first try; absorbs sampling variance
  when it doesn't.

### Phase 36 — 2026-08-10
- **Session continuity**. `session list` now renders a summary per
  session (turn count, whitespace-flattened preview of the first user
  turn, last-activity timestamp) ordered newest-first; `--resume latest`
  and `/session latest` continue the most recently active session
  instead of hand-copying nanosecond ids. Legacy sessions (pre-Phase 31,
  no `ts`) fall back to id ordering and never break the sort.
  - `session::SessionSummary` + `session::summaries(dir)` (newest-first
    by last turn `ts`, id fallback) + `session::latest(dir)`.
  - Tests: `summaries_order_by_last_activity_and_preview`,
    `summaries_fallback_to_id_order_for_legacy_sessions` (hand-written
    JSONL — `append` auto-stamps so legacy lines must bypass it),
    `summaries_empty_dir`. 89 total.

### Phase 35 — 2026-08-10
- **Cross-instance knowledge sync**. A running agent loop now
  fingerprints its harness dir every iteration (stat-only: mtime + size
  of `system_prompt.md` and each `skills/*.md`, sorted for
  order-independence) and reloads the live prompt state when another
  sigil process edited it — a concurrent REPL, MCP server or pipe job's
  `improve_harness` / `skill_rollback` is visible on the very next turn,
  no restart. Memory was already shared (every call reads the file).
  - `harness::harness_fingerprint(dir) -> u64` (cheap, deterministic,
    order-independent) + `harness::sync_prompt_state(&RwLock<PromptState>,
    dir) -> io::Result<bool>` (reloads only on change).
  - `AgentConfig.sync_dir` wired in `run_messages` loop head; set from
    `--harness-dir` in main.
  - Tests: `sync_prompt_state_reloads_after_external_edit` (prompt edit
    + skill add picked up; idle = no reload),
    `harness_fingerprint_changes_with_content` (content/skill-add
    change it; identical state is stable). 86 total.

### Phase 34 — 2026-08-10
- **On-demand skill loading**. The system prompt now always carries a
  compact index of every skill (name + description only, no bodies), so
  the model knows the full library exists and reads any skill's body on
  demand via `read_file` / `list_skills`. Per-turn relevance-ranked
  inlining of the top-N bodies is unchanged. The index stays cheap no
  matter how large the library grows — bodies are only ever loaded when
  they are actually inlined or explicitly read.
  - `Harness::skills_index_only_from(&[Skill])` renders the index
    (empty when no skills); wired into `AgentConfig::system_prompt_for`.
  - Tests: `skills_index_only_lists_names_no_bodies` (index has names +
    descriptions, never bodies), `empty_skills_index_is_empty`, plus the
    per-prompt test now asserts the index is always present while only
    relevant bodies are inlined.

### Phase 33 — 2026-08-10
- **Ratchet battery grown to 12 tasks**: new `acp_server_tool_use`
  (drives `sigil acp` over the real protocol — init → session/new →
  prompt → tool use → close — and asserts the model's reply) and
  `session_export_import` (deterministic CLI path: hand-built timestamped
  JSONL → full export → `--since` filter → import → list; every assertion
  must hold, no LLM involved).
- **Ratchet tools-dir isolation**: the battery now runs with its own
  `--tools-dir /tmp/sigil-ratchet-tools` instead of the real
  `~/.sigil/tools`. Previously, tools the model had created in earlier
  sessions leaked into every run — a bad persisted schema 400'd every LLM
  call and failed the whole battery.
- **Scripted-tool schema hardening** (`LuaTool::new`): a model-written
  `-- schema:` header missing the top-level `"type":"object"` now falls
  back to the safe default instead of being advertised verbatim (which
  made the LLM provider reject every tool-bearing request). Covered by
  `malformed_schema_falls_back_to_default` + `valid_schema_is_kept`.

### Phase 32 — 2026-08-10
- **Cross-process directory locks** for self-evolution writes. Concurrent
  sigil instances (REPL + MCP server + pipe jobs) can now safely write the
  same harness/tools dirs: `create_lua_tool`, `improve_harness` and
  `skill_rollback`/`versions_gc` wrap their read-archive-write sequences in
  an advisory `<dir>/.sigil.lock` (fd-lock; flock on Unix, LockFileEx on
  Windows), so two writers never interleave. Reads never lock; list actions
  stay outside the lock.

### Phase 31 — 2026-08-10
- **Timestamped turns**: every turn written to a session JSONL now carries a
  nanosecond `ts` field (auto-stamped on append; legacy untimestamped lines
  still load fine via `#[serde(default)]`).
- **`session export --since <cutoff>`**: export only the turns written at or
  after the cutoff (same format as turn timestamps). Untimestamped legacy
  turns are always kept; an export that would be empty fails cleanly.

### v0.1.0 — 2026-08-09
First tagged release. Full agent core: pipe / REPL / ACP / MCP modes,
vendored LuaJIT with real FFI, self-evolution (Lua tools + harness
instruction layer), three-layer safety, sessions with true multi-turn
context, sub-agent delegation, streaming, ratchet tests, `sigil doctor`,
shell completions, 48 unit tests.

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
