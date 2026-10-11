# Context and cache improvement plan

Goal: reduce unnecessary history rewriting and observe cache usage without extra generations.

Architecture: extract context budgeting into `src/agent/context.rs`, retain the existing agent API, and expose transcript thresholds through `[agent]`. No new model requests, dependencies, protocol modes, or network defaults.

- [x] Reproduce the paid diagnostic completion with a provider that panics on `chat_complete`, running the test with `SIGIL_DEBUG_CACHE=1`.
- [x] Reproduce multiline constraint loss and loss across repeated compaction.
- [x] Remove diagnostic completion and report only the current request's streamed usage.
- [x] Preserve bounded multiline excerpts and previous summaries with explicit truncation notices.
- [x] Configure token threshold (default 48000) and optional message count (default 0).
- [x] Extract cohesive context helpers, keep tool pairing and existing public interfaces.
- [x] Update both README languages with settings and loss/cost limitations.
- [x] Run default and containment tests, fmt, clippy, release build; inspect diff.

Validation: default 329/329; containment 327/327; `cargo fmt -- --check`, `cargo clippy --locked --all-targets -- -D warnings`, `cargo build --locked --release`, and `git diff --check` passed. Socket-dependent tests required running outside the filesystem/network sandbox. The cache regression was also run with `SIGIL_DEBUG_CACHE=1`. No live-model ratchet or billing comparison was run.

Remaining scope: stable skill/hook prompt composition, model-specific tokenization and output reservation, semantic task-state extraction, real-provider billing A/B measurement, exploration-tree replay. These remain outside the implementation scope.

## Follow-up: stable goals and observed request usage

- [x] Reproduce goal-counter prefix mutation; show only the static budget in the injected block, retaining persisted usage and auto-pause behavior.
- [x] Reproduce unknown cache telemetry collapsing onto zero; retain optional reported-token coverage, including partial aggregates and backward-compatible reads.
- [x] Reproduce missing stream-options usage request and lost usage-only trailer; request usage and bound trailer reads to 250 ms / 32 frames.
- [x] Count request usage snapshots once; observe tool replay and final summary without extra generations.
- [x] Reproduce missing request-usage session metadata; add non-projected events and verify emission and serialization.
- [x] Verify all tests and build/lint checks for the follow-up.

Follow-up validation: default 336/336; containment 334/334; fmt, clippy with warnings denied, release build, and diff whitespace checks passed.

This patch does not calculate currency cost or infer missing cache/write pricing. Provider-internal fallback attempts are outside the agent's reported usage. Real endpoint billing and task-quality comparisons remain unmeasured.

## Follow-up: current input estimation

- [x] Reproduce appended tool output being masked by stale real usage.
- [x] Replace the absolute last-prompt snapshot with a calibration factor observed against the corresponding messages and tool schemas; estimate each current request after goals, planning, and hooks are assembled.
- [x] Include tool definitions and message framing in the input threshold; reserve pinned-prefix cost when selecting the tail.
- [x] Reproduce CJK cold underestimation; use a larger non-ASCII allowance, then calibrate from reported usage.
- [x] Reproduce over-aggressive retention when real usage is below the local estimate; calibrate both trigger and tail, including ratios below one.
- [x] Reproduce live goal directives entering old summaries; pin leading system instructions, excluding previous summaries.
- [x] Verify shrunken history does not inherit a previous absolute prompt size and final-summary requests also use current budgeting.
- [x] Run full default/containment tests, lint, formatting, release build, and whitespace checks.
- [x] Preserve the latest bounded usage trailer snapshot, including on timeout and when the finish frame already reports usage.
- [x] Avoid non-reducing summaries in the default token-only compaction policy.

Final validation: 343 default tests and 341 containment tests passed; formatting, warnings-denied clippy, release build, and whitespace checks passed. Independent read-only review found no concrete issue in the final fixes.

The configured token value now applies to estimated input including schemas. Output headroom is configured by choosing this threshold below the actual model window; no model-specific tokenizer, automatic model window lookup, or hard request cap is claimed.


## Follow-up: offline observation report (2026-10-11)

Keep evaluation outside the Rust core: a dependency-free Python reader accepts explicit JSONL session paths, aggregates only request_usage metadata, groups model/request kind, and emits human-readable totals or JSON. Missing data remains unknown; rates use the reported subset. No prices, task-success inference, or live generations are added. Duplicate paths are excluded, but overlapping fork histories remain a caller responsibility.

- [x] Write and reproduce missing-reporter regression tests before implementation.
- [x] Implement subset accounting, explicit unavailable state, validation, and duplicate-path handling.
- [x] Document equivalent-task comparisons, cold/warm cache separation, and billing/quality limits in both READMEs.
- [x] Verify Python regressions and independent review; check whitespace.

Validation: all 8 Python regressions passed, plus a synthetic CLI report and whitespace/formatting checks. Independent review identified missing UTF-8 error locations and Rust-compatible default cache hit counters; both were reproduced and fixed. No live model calls or personal session logs were used.
