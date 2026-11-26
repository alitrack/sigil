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

### Context budgeting and cache diagnostics

Skill relevance selects which bodies are inlined; score ties and the final selected set use name ordering. Changes in relevance scores alone therefore do not reorder an unchanged set of skill bodies. Skill membership, content edits, and dynamic hook output can still change the prompt.

When rebuilding a system prompt from an event-sourced history, skill relevance and `on_prompt_build` use the latest user message. A resumed pipe turn can therefore select skills for its current task instead of the first task in the session.

Active-goal prompts show the objective and static turn budget. The changing turns-used counter stays in persistent state and `manage_goals` output, so accounting alone does not rewrite the prompt prefix. Goal additions, completion, pauses, and budget changes still refresh the context.

The default compaction trigger is an estimated 48,000 input-prompt tokens, with no message-count trigger. Configure it for your model under `[agent]`:

```toml
[agent]
max_context_tokens = 48000
max_context_messages = 0
planning_interval = 0       # restate the plan every N steps (0 = off)
```

Each zero disables that trigger; setting both to zero disables proactive compaction. The estimate includes message framing and tool schemas. Real usage calibrates the current request rather than reusing a previous absolute token count, so new tool outputs and history shrinkage are reflected. Cold estimates allow roughly two tokens per non-ASCII character and one per four ASCII characters; this is a heuristic, not a model tokenizer. The threshold is not a hard request-size limit: pinned instructions, summary overhead, and an indivisible current tool turn can exceed it. Configure it below your model context window to reserve output capacity. Provider context errors still allow one reactive compaction retry.

Compaction pins leading system instructions and live goal blocks outside the historical summary, uses bounded multiline excerpts, carries previous summary content forward, and preserves tool-call/result pairing. It remains lossy: excerpts retain at most 2,000 user characters, 1,000 assistant characters, 500 tool characters, or 8,000 previous-summary characters; the combined summary is capped at 16,000 characters plus a truncation notice. Consult the original session log when details are omitted. It does not generate an LLM summary request.

`SIGIL_DEBUG_CACHE=1` observes actual stream, tool replay, and final-summary requests without sending diagnostic completions. Streaming requests ask for usage; a usage-only trailer after `finish_reason` is read for at most 250 ms (32 frames). Missing usage/cache fields remain unavailable rather than becoming a reported zero hit rate. Session event sinks record `request_usage` metadata, including the request kind, model, and observed usage. Aggregate cache metrics retain the number of prompt tokens with reported cache coverage; old logs without this field remain readable but have unknown coverage. Multiple usage snapshots within one request are counted once, using the latest observed snapshot even if the bounded trailer read times out. Token-only compaction retains the original history when its summary would not reduce the input estimate. This is token telemetry, not a complete bill: unreported usage, cache-write rates, provider-internal retries, and pricing are not inferred. Compression can invalidate the changed prefix and increase first-request cost; fewer tokens alone do not demonstrate savings. Compare successful-task cost and quality on your actual provider.


Record pipe turns for subsequent comparison:

```bash
sigil --sessions-dir ./evaluation-sessions prompt -p "Read src/cli.rs and describe its first line"
sigil --sessions-dir ./evaluation-sessions --resume latest prompt -p "Continue the task"
```

Explicit `--sessions-dir` records pipe history and request usage; `--resume` appends new events to the resumed session. Ordinary pipe runs without either option remain ephemeral. The session ID is printed on stderr; pipe IDs must be plain filenames. Session logs contain conversation and tool content, so keep evaluation artifacts in an appropriate local directory.

Report explicit session logs offline (Python standard library only):

```bash
python3 test-tools/usage_report.py path/to/session.jsonl
python3 test-tools/usage_report.py --json path/to/session.jsonl
```

Run a paired synthetic task-state retention experiment (Python 3.11+):

```bash
python3 test-tools/context_eval.py
python3 test-tools/context_eval.py --execute --output ./context-eval-run --model MODEL --base-url http://127.0.0.1:3001/v1
```

The first command only prints a plan. The second uses an already configured `SIGIL_API_KEY`; generated configuration and summary reports do not contain credentials. The output directory must be new. Both policies receive identical seeded histories; one disables proactive compaction and the other defaults to an 8,000-token trigger (`--threshold` overrides it). Two repetitions alternate policy order. `report.json` includes exact-answer success, request counts, observed tokens, cache coverage, per-policy totals, and the binary SHA-256. Failed attempts remain in the denominator, and interrupted/invalid observations keep accounting incomplete. The default fixture measures one key-retention task, not general coding quality. Provider cache state is uncontrolled, and these runs do not establish cold-cache/warm-cache cost or monetary savings. Three tool steps and `--timeout` bound execution, not token billing; the model's output limit remains provider-controlled. Sessions contain synthetic histories plus actual model/tool output; keep them local. Compare provider bills separately before drawing cost conclusions.

`write_file` accepts text strings verbatim for any target. For `.json` targets (case-insensitive), it also accepts JSON objects or arrays and serializes them as formatted JSON. Structured content for other file types, null, and numeric/boolean values are rejected before any filesystem mutation.

Use `--case json-edit` for a file-edit fixture instead of exact-answer retention:

```bash
python3 test-tools/context_eval.py --case json-edit --execute --repetitions 1 --output ./context-json-run --model MODEL --base-url http://127.0.0.1:3001/v1
```

This case requires the model to read and modify `settings.json` according to earlier instructions. Success requires correct field values/types, unchanged unrelated nested/Unicode fields, an unchanged protected file, successful process exit, and a recorded `read_file` before `write_file`. Other tools, other paths, or unexpected workspace changes fail the case; evaluator session logs and runtime goal accounting are excluded from the workspace check. JSON parsing is bounded and rejects duplicate keys, symlinks and non-regular files. Generated code is never executed by the checker. This adds a small tool-use/artifact check, not a representative coding benchmark. A single repetition does not balance policy order or establish quality/cost non-inferiority.

Analyze an existing paired report without any model calls:

```bash
python3 test-tools/context_cost.py docs/evaluations/2026-10-12-context-json-repeat.json
```

Without prices, this prints the observed token difference and a symbolic charge relation. Monetary fields remain null. To estimate, supply all of `--uncached-input-rate`, `--cached-input-rate`, `--output-rate` (currency units per million tokens), and `--currency`. Rates must match the actual billed model/route; the tool never looks them up. They must be finite and nonnegative (at most 1e12, 28 decimal digits, exponents -36 to 12). Incomplete usage or unknown cache splits prevent a full estimate; identical cached/uncached rates are the exception because the split then does not affect cost. Failed attempts remain in spend, and cost per success is null at zero successes. Duplicate/unpaired records, mismatched fixture versions and mixed-model flat pricing are rejected. Estimates exclude unreported requests, cache-write fees and other invoice charges. A cache-loss regression demonstrates that fewer input tokens can cost more under explicitly hypothetical prices; this is not a current provider quote.

The JSON output groups requests by model and request kind. Cache hit rate uses only input tokens with reported cache coverage; `cache_coverage_rate` is the fraction of requests reporting cache data. Missing observations are excluded from token sums and shown through request counts and `usage_complete`; old logs without `request_usage` cannot reconstruct usage. Repeating the same file path does not count it twice, but overlapping copies or forked session histories must be excluded manually. Invalid records fail with a file/line location without printing their content. For a compaction comparison, run the same fixed tasks under the same model, tool definitions, and output limits; evaluate task success separately and compare provider bills as well as observed tokens. Keep cold-cache and warm-cache runs separate. This reporter does not send model requests or infer task success or monetary cost.


## DREAM exploration and replay

`test-tools/dream.py` provides an opt-in discovery → historical replay → policy
selection → next-round deployment loop around the unchanged sigil binary.

```bash
# Preview concrete tasks and bounded execution; no model calls.
python3 test-tools/dream.py run --output ./dream-run --rounds 2 --decision-rounds 3 --attempts 3 --model MODEL --base-url http://127.0.0.1:3001/v1
# Collect new worlds and redeploy the selected policy into the next round.
python3 test-tools/dream.py run --execute --output ./dream-run --rounds 2 --decision-rounds 3 --attempts 3 --model MODEL --base-url http://127.0.0.1:3001/v1
# Compare policies again using stored outcomes only; no agent/evaluator execution.
python3 test-tools/dream.py replay --world ./dream-run/round-1/world.json --world ./dream-run/round-2/world.json --policy ./dream-run/selected-policy.json --output ./dream-replay
```

Online output directories must be new. Authentication uses `SIGIL_API_KEY` in
the environment. Each attempt has separate artifact and agent-state directories;
these prevent state contamination and are not an OS sandbox. The first task
family edits JSON with machine-checked types, preserved fields, protected files
and read-before-write workflow. Failures remain in the tree. A safe, fingerprinted
parent snapshot is restored for each continuation.

Policies are bounded JSON parameters controlling branch width, observed-score or
depth ranking, stagnation, success stopping and attempt limits. A fixed interpreter
and parameter sweep implement this initial family; this is not unrestricted
model-written policy-code evolution. Use `--candidate policy.json` to include an
additional validated configuration. Online execution uses one worker; `replay
--workers N` supports historical batching. The objective is best normalized
quality minus `--beta1` times represented attempts plus `--beta2` times attempts
per decision round. The incumbent participates and wins ties; improvement is
guaranteed only on the recorded pool, not on unseen tasks. Replay cannot invent
unrecorded branches and never starts online execution to fill missing outcomes.

`run.json`, per-round `world.json`/`replay.json`, and policy files retain deployment
and rollback evidence. Reuse a round's `incumbent-policy.json` with `--policy` to
start a new experiment from that prior policy. Replay without `--output` prints
its report only. Mark fake providers with `--source simulated`; simulated and
online worlds cannot share a selection pool. Run deterministic tests with
`python3 -m unittest discover -s test-tools -p 'test_dream*.py'`; set
`SIGIL_DREAM_E2E=1` for the actual-binary local fake-provider test. This fixture
validates orchestration, not general coding improvements or the paper's benchmarks.

## Evidence: the harness really does evolve

The core thesis — *an agent that modifies its own harness gets stronger over
time* — is not a roadmap item. It was tested with a reproducible benchmark
(2026-08-19, 8 epochs, `benchmark/` + [`evolution-benchmark.md`](evolution-benchmark.md)):

**Setup.** One fixed task pool with machine-checked objective assertions
(file contents, exit codes — no LLM self-grading). Evolving group: sigil,
allowed to create tools/skills/hooks across epochs. Control group: dsh with a
**frozen** capability surface. Same model (Qwen3.8-27B on a local SGLang
endpoint), same tasks, same assertions.

**Result (per-epoch pass rate).**

| Epoch | 1 | 2 | 3 | 4 | 5 | 6 | 7 | 8 |
|---|---|---|---|---|---|---|---|---|
| sigil (evolving, all 3 tasks) | 1/3 | 2/3 | 3/3 | 1/3 | 2/3 | 3/3 | 3/3 | 2/3 |
| dsh (frozen control, T1 series) | 0/2 | 0/2 | 0/2 | 0/2 | 0/2 | 0/2 | 0/2 | 0/2 |

- The evolving group shows an *invest → ramp → plateau* curve (T1 series
  6/8 and 4/8 by task family; T2 7/8 with harness path facts recalled across
  epochs). The frozen control scored **0/16** on the T1 series — same model,
  same tasks.
- The capability surface grew monotonically from **7 → 39 assets** across 8
  epochs. Tools were not just created but *iterated*: the same summing tool
  went through 3 versions with rollback snapshots; later epochs reused
  existing tools and ran visibly faster.
- Zero security warnings on all host-sandboxed (`host.*`) self-built tools —
  but the run also produced the strongest argument for the safety stack:
  evolution spontaneously grew an FFI tool wrapping `DeleteFileA`. The
  review gate / execution ring contained it, which is exactly why they are
  hard requirements here.

**Honest limits.** Single model, single seed, 8 epochs, no held-out task
families yet — so this supports the thesis but cannot yet rule out benchmark
overfitting. Next round: multiple seeds, larger models, held-out tasks, and
an automated candidate → evaluate → promote → rollback pipeline (the pieces —
versioned rollback, refinement gate, revertible effects — already exist).

> Status: **Thin-core P-C (v0.2.0)** — the Lua policy layer is live:
> agent-loop hooks (`on_turn_start` / `on_tool_result` / `on_prompt_build`)
> run sandboxed Lua from `~/.sigil/hooks/` and reload from disk on every
> use; `improve_harness` gained `edit_hook` / `delete_hook` / `list_hooks`
> with a `return function` contract gate, review-gated reasons, and
> versioned rollback (kind=hooks) — the model authors its own policy
> hooks mid-turn and they go live the next turn, no restart. Also:
> streamable HTTP transport for MCP clients.
>
> Status: **Phase 71** — cron jobs (prime-agent `cron-jobs.ts`,
> minimal): daemon frames `{"type":"cron","action":"add|list|pause|
> resume|remove",...}` manage persisted jobs (`<sessions_dir>/
> cron.json`) with `interval` (refires every N s) and `once` (fires at
> a unix time) schedules; a 30s scheduler loop fires due jobs into
> their target session like a user turn. E2E: an interval-5s job was
> added, fired by the scheduler, and its prompt/answer turns landed in
> the session file with run_count/next_run_at tracked.
>
> Status: **Phase 70** — side questions (prime-agent
> `side-question.ts`): a daemon frame `{"type":"side_question",
> "text":..., "id":...}` answers against a clone of the session
> context with NO tools and a single-turn budget, streaming
> `side_delta` and replying `side_answer` — the session history is
> untouched. E2E: the daemon answered a side question about its own
> conversation while the on-disk session kept only the main turns.
>
> Status: **Phase 69** — nesting depth cap (prime-agent
> `RLM_MAX_DEPTH`, default 1): each `DelegateTool` instance carries a
> `depth` (0 = top level); a sub-agent's tool registry gets a depth+1
> copy in place of the shared parent instance, so a delegated
> sub-agent's spawn/delegate are refused ("nesting depth limit
> (1) reached"). E2E: the sub-agent tried to delegate twice, got
> rejected both times, and verified via `delegate list` that no
> grandchild was created.
>
> Status: **Phase 68** — per-agent condenser (OpenHands
> `definition.condenser`): `spawn` and each `delegate` batch task
> accept `max_context_messages` / `max_context_tokens` overrides that
> replace the parent's global context budget for that sub-agent
> E2E: a sub-agent spawned with
> `max_context_messages: 4` / `max_context_tokens: 3000` completed
> normally under the tight budget.
>
> Status: **Phase 67** — max_children cap (OpenHands=5): at most 5
> sub-agents may run at once — `delegate spawn` and the `delegate`
> batch both check the cap up front (running sub-agents eat into the
> budget; finished ones free it; batches over the cap are rejected
> with a "split into smaller batches" hint). E2E: the model tried a
> 6-task batch, got rejected, and re-planned into a 3-task batch.
>
> Status: **Phase 66** — `delegate` metrics (OpenHands
> `usage_to_metrics` view): `action=metrics` lists every sub-agent's
> own prompt/completion usage plus a grand total — the whole
> delegation bill at a glance (usage is recorded once on completion,
> replace not merge, so resumes never double-count). Also: reqwest
> clients now set `no_proxy()` — an `http_proxy` env var was
> hijacking loopback endpoints (502s broke local LLM endpoints and
> the fallback tests).
>
> Status: **Phase 65** — `delegate` blocking batch (OpenHands
> `delegate` semantics): `action=delegate` with a `tasks` array spawns
> every sub-agent concurrently and blocks until they finish, returning
> aggregated results in one call (optional `timeout`, default 600s;
> still-running ones are reported, not errored).
>
> Status: **Phase 64** — daemon multi-session (herdr's pane model):
> one daemon hosts parallel sessions; `sigil send --session foo`
> opens a fresh session on first use and keeps appending to it;
> `{"type":"session","action":"list"|"new"}` lists/starts sessions.
> Each session keeps its own history and persists independently.
>
> Status: **Phase 63** — `delegate spawn` accepts an optional `name`
> (rlm `session_name` validation: trimmed, non-empty, ≤64 chars —
> mirrors `normalizeRequestedRlmSubagentSessionName`), recorded on the
> handle and preserved across `resume`.
>
> Status: **Phase 62** — compaction driven by real token usage:
> `compact_messages_with_budget` now takes the most recent LLM call's
> real `prompt_tokens` (prime-agent uses actual `usage.totalTokens`
> over the chars/4 heuristic): it drives the over-budget trigger and
> calibrates the per-message estimate while walking the retained tail
> (the heuristic underestimates CJK-heavy transcripts ~4x). The
> budget check runs every loop iteration, so the first step uses the
> heuristic and every step after uses measured usage.
>
> Status: **Phase 61** — token usage accounting: the provider's
> `usage` block is parsed from both the streaming SSE (final chunk)
> and tool-call replay completions, accumulated across the run
> (`RunResult.tokens`), persisted on the delegate handle and shown in
> `delegate status/list` (e.g. `10598 prompt + 328 completion
> tokens`). This closes the metrics gap in the rlm/OpenHands
> comparison — no provider change was needed, only parsing.
>
> Status: **Phase 60** — parent→child messaging (the other half of
> rlm `agent_message.send`): `delegate send` queues a message to a
> sub-agent's inbox (bounded, persisted to meta.json) and every
> sub-agent gets a `check_parent_messages` tool plus an injected
> COMMUNICATION RULES block telling it to check at the start of every
> step; on `resume` the queued messages transfer to the new handle
> (transfer_inbox — a continuation keeps seeing them, the old record
> never consumes them twice). rlm's message semantics are now fully
> covered. Previous: Phase 59 child→parent messaging, Phase 58
> per-sub-task model pick (rlm.find_models), Phase 57 `delegate
> delete` (rlm registry completion), Phase 56 sub-agent step
> visibility (a first slice of metrics), Phase 55 refinement audit
> ids + merged history (the prime-agent `mergeRefinementHistory`
> pattern), Phase 54 sub-agent typing (AgentDefinition pattern),
> Phase 53 sub-agent resume + delegate registry persistence, Phase 52
> scope strategy, Phase 51 the dual-track itself (--local-harness-dir
> merged view + scope parameter), Phase 50 Lua sandbox hardening,
> Phase 49 structured run results (RunResult + provide_final_answer),
> Phase 48 parallel tools + reactive compaction + planning checkpoints
> + delegate report templates, 48r daemon deadlock fix, Phase 47 CLI
> consistency, Phase 46 daemon UX, Phase 45 ratchet in CI.
> ratchet-in-CI + goal turn-budgets, Phase 44 daemon, Phase 43 async
> delegation, Phase 42 goals, Phase 41's harness refinement gate +
> token-budget compression, Phase 39's memory write
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
| `SIGIL_BASH_TIMEOUT_SECS` | `60` | Wall-clock budget (seconds) per `bash` command; the process is killed when exceeded |
| `SIGIL_NO_PROGRESS` | `2` | No-progress guard: stop and wrap up after N consecutive stalled rounds (`0` disables) |

Any OpenAI-compatible endpoint works via `SIGIL_BASE_URL` — vLLM, Ollama,
llama.cpp, DeepSeek, etc. Config file at the platform config dir
(`~/.config/sigil/config.toml` on Linux, `%APPDATA%\sigil\config.toml` on
Windows; `[llm]`, `[agent]`, `[[mcp.servers]]`); precedence is CLI > env >
file > defaults. `--tools-dir` (Lua tools, default `config dir/sigil/tools/`),
`--harness-dir` (system prompt + skills, default `config dir/sigil/harness/`),
`--memory` (remember/recall store, default `config dir/sigil/memory.md`),
`--sessions-dir` (session transcripts, default `~/.sigil/sessions/`),
`--resume <id>` (continue a session), `--list-sessions`.

## Capabilities

- **Native tool-calling** — streamed completions, non-streaming replay for
  full tool-call payloads (OpenAI function-calling; works against any
  compatible backend).
- **Tools** — `bash`, `read_file`, `write_file`, `git_status/diff/log`,
  plus **Lua scripted tools** (`config dir/sigil/tools/*.lua`, filename = tool name,
  `-- desc:` / `-- schema:` headers).
- **Embedded LuaJIT + FFI** — `mlua` with vendored LuaJIT. Scripts can
  `require("ffi")` and call any C library (zlib `crc32` verified end to end).
- **Lua execution limits** — Lua chunks and exported functions have a 5s
  deadline and a 10-million-instruction budget. JIT execution is disabled
  so instruction hooks can stop loops. Blocking host or native FFI calls
  cannot be preempted by these hooks. `--no-default-features` disables FFI
  and native library loaders.
- **Self-evolution** — `create_lua_tool`: the model writes Lua, sigil saves
  and hot-registers it, and it's callable from the next turn — persisted
  across sessions.
- **Harness self-improvement** — `improve_harness`: the model can edit its
  own `system_prompt.md` (`edit_prompt`) and add skills (`add_skill`) —
  the instruction layer evolves from within, recompile-free.
- **Harness bundle** — `config dir/sigil/harness/` holds `system_prompt.md` and
  `skills/`; edit files (not code) to change agent behavior.
- **Safety (3 layers)** — approval gate (destructive commands rejected in
  non-interactive mode; interactive `y/N` in the REPL), circuit breaker
  (tool tripped after 3 consecutive failures), execution rings
  (read-only / workspace / harness mutation). Unknown Git subcommands,
  custom aliases, and external Git helpers require approval too.
  **Read the limits honestly**: the approval gate pattern-matches `bash`
  only — every other tool is allowed, and the interactive `y/N` prompt exists
  in the REPL alone (pipe, daemon, cron and sub-agents run with no approver).
  What bounds the rest is write scope (`write_file` is confined to the
  workspace / temp / sigil state dirs; `SIGIL_WRITE_ROOTS` extends it), a
  credential-read denylist (`read_file`, Lua `host.read_file` and `host.env`
  refuse key material — `~/.ssh`, `~/.aws`, `~/.netrc`, `*.db` of the proxy,
  and any env var whose name looks like a secret; `SIGIL_READ_ALLOW` exempts a
  path deliberately), and the freeze switch (`SIGIL_FREEZE=1` refuses every
  write to the evolution surface). The default build enables LuaJIT **FFI**, so a model-authored
  scripted tool can call any C library — build `--no-default-features` for a
  containment build. In short: sigil assumes the model is trusted to act with
  your privileges. Do not point it at untrusted content with FFI enabled.
- **Memory** — `remember` / `recall`: persistent cross-session facts the
  model maintains itself.
- **MCP client** — `[[mcp.servers]]` in config spawns stdio MCP servers;
  their tools become `mcp_<server>_<tool>`.
- **MCP server** — `sigil mcp` exposes sigil's own tools to any MCP client
  (Claude Desktop, Cursor, other agents) over stdio.
- **ACP server** — `sigil acp` speaks Agent Client Protocol (JSON-RPC
  over stdio); any ACP host (editors, other agents) can drive sigil.
- **Interactive REPL** — multi-turn conversation with human approval for
  destructive commands.
- **True multi-turn context** — the REPL keeps the real message list across
  turns, including tool calls, results, and compaction summaries;
  `--resume` restores the transcript from persisted conversation events
  (the model remembers the conversation, not a text blob).
- **Line editing + history** — rustyline: arrow-key history, inline editing;
  per-session `.history` files persist commands across sessions.
- **Streaming output** — assistant text streams live in pipe and REPL modes
  (`[using tools…]` marks tool calls mid-stream).
- **Resumable sessions** — every REPL turn is recorded to
  `~/.sigil/sessions/<id>.jsonl`; `--resume <id>` continues the
  conversation and restores the session's original working directory
  (recorded at session start), so relative paths resolve the same way
  as before even when you resume from a different folder.
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
  calls → approve → execute → repeat until the model stops. Guarded by
  default: a no-progress guard stops exact-repeat rounds (disable with
  `SIGIL_NO_PROGRESS=0`), and a step budget (`max_steps`, default 100)
  ends the turn with one final tool-less wrap-up call instead of a hard
  cut.
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
| **12** ✅ | ACP client + `acp_call` — **removed 2026-11-23**: it duplicated `delegate` and made sigil into an agent that drives other agents, an extra un-auditable trust path |
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
| **46** ✅ | Daemon UX polish (persistent attach session + /quit, send --timeout, friendly errors) |
| **47** ✅ | CLI consistency (pipe --resume latest works, session list timestamps) |
| **48** ✅ | Research landing (parallel tool calls, reactive compaction, planning checkpoints, delegate reports) |

## Changelog

Full per-phase history: [CHANGELOG.md](CHANGELOG.md) (entries are frozen history — not rewritten when behavior evolves).

### v0.2.0 — 2026-08-13
Delegation & sub-agents, daemon + cron, robustness, and the thin-core
roadmap (Lua policy layer). Highlights:

- **Sub-agent delegation**: typed agents (TOML frontmatter, tool
  allowlists), spawn / resume / delete / status, per-agent model and
  context budget, parent↔child messaging, token usage metrics, nesting
  capped at depth 1.
- **Daemon + cron**: `sigil daemon` attach/send with session persistence;
  cron jobs (`once` / every N seconds); side questions.
- **Robustness**: parallel tool execution, reactive compaction, planning
  checkpoints, LLM failover chain, versioned harness rollback, local/global
  harness layers (prime-agent style).
- **Lua sandbox hardening**: stdlib IO/OS removed, host helpers only,
  require whitelist.
- **MCP**: streamable HTTP transport for MCP clients.
- **Thin-core roadmap (P-A/P-B/P-C)**: Lua hooks at agent-loop points
  (`on_turn_start` / `on_tool_result` / `on_prompt_build`), model-authored
  policy hooks via `improve_harness` with versioned rollback — the model
  evolves its own instruction layer at runtime, with no restart.
- 194 unit & integration tests.

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

Chinese version: [README_cn.md](README_cn.md).
