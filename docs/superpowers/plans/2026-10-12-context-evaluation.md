# Context Evaluation Implementation Plan

**Goal:** Compare compaction policies on identical synthetic task histories, retaining correctness checks and observed request usage without embedding endpoint credentials.

**Architecture:** Keep evaluation in Python beside usage_report.py. Generate isolated harness, tools, hooks, memory, delegate state and two identical event histories. Invoke the same release binary with token-trigger disabled versus a configured threshold, record sessions, and validate an exact task-state answer. Reuse usage_report for accounting. Default to printing the plan; explicit --execute makes model calls. Alternate policy order across repetitions and mark provider cache state uncontrolled. No automatic price or causal savings claim.

**Tech stack:** Python standard library and the existing sigil CLI.

- [x] Write tests for identical histories, credential-free generated configuration, policy limits, exact outcome validation, failure handling and incomplete usage.
- [x] Verify tests fail before implementing the evaluator.
- [x] Generate seeded user/assistant histories with a retained task-state marker and removable historical bulk, unique trial markers, and explicit local output directories.
- [x] Run both policies with bounded subprocess timeouts and max steps; retain timeout/error outcomes rather than dropping failed attempts.
- [x] Produce report.json with per-policy task outcome and observed usage; do not print/save model stdout or stderr in summary artifacts.
- [x] Verify using a local simulated LLM and self-contained Python tests; inspect the real endpoint's availability separately.
- [x] Document usage, billing/quality limits, cache-state limitations, and deterministic fixture scope in both READMEs.
- [x] Independent review and whitespace checks.
- [x] Create local commit after final verification.


Validation: 9 evaluation tests including local simulated-LLM E2E passed; 8 existing usage-report tests passed. The local service verifies identical off/on history, alternating order, actual compaction in on requests, exact-answer success, four requests with no telemetry probes, and shorter compacted input. Independent review found interrupted-request completeness and corrupt-log sample loss; both were reproduced and fixed. Summaries retain observed counters but mark interrupted accounting incomplete. Valid observations preceding corrupt lines are retained; reports are replaced atomically and fingerprint the binary.

Real endpoint status: 127.0.0.1:15722 is unavailable outside the sandbox. The user selected 127.0.0.1:3001; its models endpoint is reachable but requires an access key. SIGIL_API_KEY/OPENAI_API_KEY/GPT_LOAD_API_KEY are absent from the execution environment. No real-model generation or billing comparison has run yet; model name, authenticated runtime and any required group path are pending.


## Real endpoint follow-through

The user selected localhost:3001 and supplied authentication. Used the credential only in transient process environment; generated configurations, summaries and committed artifacts contain no credential. The first listed DeepSeek model failed with provider HTTP 410. Switched to listed GLM 5.3 Flash and completed four requests: off/on strict success 1/2 each, off/on prompt totals 27,798/17,652, completion totals 106/141, zero reported cache hits, full reported usage coverage. The token observation and limitations are saved under docs/evaluations/2026-10-12-context-ab.md and .json. Currency billing and broader quality validation remain unmeasured.
