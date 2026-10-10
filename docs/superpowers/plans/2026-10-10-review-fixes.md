# Expert review repair plan

Goal: repair the actionable defects identified by the multi-domain review, with regression coverage and no changes to package identity, network defaults, or CI.

Work in the current workspace to include the two untracked analysis scripts. The tracked source has no pre-existing edits. Do not commit or publish automatically.

## Safety and Lua

- [x] Reproduce compound-command and Git-option approval bypasses, MCP gate bypass, native library loading in containment mode, and runaway Lua scripts.
- [x] Preserve interactive approvals while gating MCP noninteractive calls. Preserve structured tool failure information for MCP results.
- [x] Parse shell command boundaries and Git global options; reject dangerous nested/dynamic command forms conservatively.
- [x] Use safe Lua construction in containment mode and enforce an instruction/deadline budget that actually stops Lua execution.
- [x] Run safety, Lua, scripted-tool, and MCP server regressions in both feature modes.

## Protocols

- [x] Reproduce ACP ignored cwd, swallowed errors, overlapping prompts and misrouted questions, and HTTP SSE waiting for EOF.
- [x] Bind session execution context without changing process-global cwd; serialize or reject overlapping prompts per session and bind questions/progress to the executing session.
- [x] Propagate JSON-RPC errors and parse SSE incrementally, returning on a matching response.
- [x] Run ACP/MCP client regressions with isolated fake servers and temp directories.

## Conversation and state

- [x] Reproduce dropped transcript/compaction state, resume loss, stale cron saves, goal persistence/injection issues, and daemon socket replacement.
- [x] Retain returned messages, restore event history, transactionally update persisted schedules/goals, and protect daemon ownership.
- [x] Run relevant agent, REPL/session, goals and daemon tests.

## Statistical scripts

- [x] Test symmetric examples and reference fixtures for trim-and-fill.
- [x] Replace both incorrect estimators with a shared signed-rank iterative implementation; use the matching variances when mirroring studies.
- [x] Correct the falsely labelled HKSJ interval and add independent reference checks.

## Integration

- [x] Review each change against its reproduction and inspect the combined diff for conflicts and unintended behavior.
- [x] Run cargo fmt -- --check, cargo clippy --locked --all-targets -- -D warnings, cargo test --locked, cargo test --locked --no-default-features, and Python statistical regressions.
- [x] Report verification and any actual limitations.

## Verified result

- Default build: `cargo test --locked --quiet` — 326 passed.
- Containment: `cargo test --locked --no-default-features --quiet` — 324 passed.
- `cargo fmt -- --check`, `cargo clippy --locked --all-targets -- -D warnings`, `cargo build --locked --release`, and `git diff --check` passed.
- `python3 test-tools/test_meta_analysis.py` — 8 passed; both report scripts completed and rendered seven charts in isolated temporary output directories.
- Newly built release binary passed MCP initialize/discovery/safe-tool execution, compound-command/Git-alias/brace-expansion rejection, and structured error smoke checks with isolated state.
- Independent review found and then verified closure of Git alias and executable brace-expansion bypasses; no remaining actionable findings in the reviewed repairs.

Lua budgets stop VM execution; blocking host/FFI calls still require a process boundary for hard preemption. The shell gate remains a conservative approval heuristic. Real-endpoint ratchet tests were not run. Existing generated report files were left intact; report smoke outputs were temporary. Approved for a local commit; generated output files are excluded.

Pre-commit verification: default tests passed eight consecutive serial runs. Concurrent suite stress exposed existing shared temporary-directory races in hook tests; a startup-test failure occurred once and did not recur in serial runs. Its assertion now includes returned errors for diagnosis. CI-style sequential suites pass.
