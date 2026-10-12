# DREAM Implementation Plan

> **For agentic workers:** Use superpowers:subagent-driven-development for isolated implementation and read-only reviews. Follow test-driven-development and verification-before-completion.

**Goal:** Ship the approved opt-in discovery → replay → select → redeploy loop.

**Architecture:** Keep Python orchestration separate from the unchanged Rust agent. `dream_core.py` owns bounded policy/tree validation, shared action selection, replay and incumbent-preserving selection. `dream_online.py` owns the fixed evaluator, isolated attempts and atomic world persistence. `dream.py` owns commands and recursive rounds.

**Tech Stack:** Python standard library, existing sigil release binary, unittest and a local OpenAI-compatible fake provider.

## Task 1: Replay core

- [x] Create `test-tools/test_dream_core.py` with hand-built worlds. Confirm missing implementation fails before implementation.
- [x] Create `test-tools/dream_core.py` with `validate_policy`, `validate_world`, `select_batch`, `replay`, `select_policy` and `candidate_policies`.
- [x] Test exact revelation order, unseen outcomes, root-child ordering, leaf legality, batching, support exhaustion, arithmetic and multiple-world compatibility.
- [x] Test incumbent ties, worse/invalid candidates and unsafe/non-finite/excessive inputs.
- [x] Run `python3 -m unittest discover -s test-tools -p test_dream_core.py -v`; obtain spec and code-quality review.

Contracts: policy version 1, named configuration, branch_width 1..32, ranking score/depth/breadth, stagnation 0..64, stop_score null or [0,1], max_attempts 1..256. World version 1, root `root`, nodes in recorded generation order, unique safe IDs, root parent null, scores [0,1], resumable boolean, bounded diagnostic labels. Compatibility requires matching task family, evaluator version, model and binary SHA. Non-root nodes have at most one child. Replay accepts workers 1..32 and rounds 1..256; objective coefficients finite/nonnegative. Policies see revealed nodes and available action IDs only.

## Task 2: Online execution

- [x] Create `test-tools/test_dream_online.py` covering parent restoration, objective checks, workflow checks, snapshot limits and failed attempts; confirm red before implementation.
- [x] Create `test-tools/dream_online.py` invoking the supplied binary in a fresh workspace with all sigil state paths explicit.
- [x] Score the fixed JSON task using exact value/type checks and preservation invariants; runtime failure or workflow/state violations score zero.
- [x] Store root and attempts with safe snapshot/session references and atomic manifests after each completion. Do not serialize provider stderr or keys.
- [x] Freeze execution configuration within a pool; online workers remain one. Use shared `select_batch` with no hidden tree data.
- [x] Run online tests and obtain spec/code-quality review.

## Task 3: CLI and recursive deployment

- [x] Create `test-tools/test_dream.py` testing no-execution plans, malformed inputs, offline zero subprocess calls and a two-round actual-binary fake-provider E2E.
- [x] Create `test-tools/dream.py` with `explore`, `replay`, `run` commands. Online commands plan by default and require `--execute`.
- [x] Save round comparisons, incumbent/selected fingerprints and selected policy locally. Feed the selected policy into the next online round; retain failed outcomes.
- [x] Support validated candidate files and a deterministic bounded policy sweep; preserve incumbent on ties.
- [x] Add matched README and README_cn examples, bounded scope and support limitations.
- [x] Review spec coverage, execute fake-provider E2E and a bounded live smoke if authentication is available. Keep simulated and live evidence separate.

## Final validation

- [x] `python3 -m unittest discover -s test-tools -p 'test_dream*.py' -v`
- [x] Existing usage, context and cost Python tests plus pipe session E2E as relevant.
- [x] `cargo test --locked`, `cargo test --locked --no-default-features`, `cargo fmt -- --check`, `cargo clippy --locked --all-targets -- -D warnings`, `cargo build --locked --release`.
- [x] Final independent review, `git diff --check`, local conventional commit. Push only when requested.

Implementation stays in the current checkout on a `codex/` feature branch, retaining the user's untracked `meta_analysis_output/` unchanged. No new Cargo dependencies, CI edits or network-default changes.

## Completed validation

31 DREAM tests passed with the actual-binary local fake-provider E2E enabled.
Existing context evaluator: 15 passed with E2E enabled; usage reporter: 8 passed;
cost tool: 8 passed; pipe-session actual-binary E2E: 1 passed. Rust default: 352
passed; containment: 350 passed; fmt, warnings-denied clippy and release build passed.
Python modules compile, diff checks pass, and deliverables passed a credential-pattern check.

Independent spec and code-quality reviews found and verified fixes for env-only
endpoint propagation, consistent termination classification, terminal-parent
lineage, source labels/pool mixing, preview budgets, offline persistence and
discarded subprocess output. Review also confirmed the recorded-support limitation
revealed by the live run.

Live smoke: two rounds, four attempts, all artifact/workflow checks passed with
GLM 5.3 Flash on the user-selected localhost:3001 endpoint. The first winning
policy changed the next tree from two root branches to one refined branch; the
second selection added success stopping. It did not establish fewer live attempts
or general coding improvement. See docs/evaluations/2026-10-12-dream-smoke.md/.json.
Keys were transient; original sessions/workspaces remain outside deliverables.

Implementation is kept on codex/dream in the existing checkout, not pushed.
