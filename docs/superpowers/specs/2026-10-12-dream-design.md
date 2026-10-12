# DREAM exploration and replay for sigil

## Objective

Implement a working, opt-in exploration loop around the existing sigil binary:
collect evaluated discovery trees, replay candidate exploration policies without
agent or evaluator execution, retain the incumbent in selection, and deploy the
winner into the next online round. Stop expanding billing analysis.

This is a bounded implementation inspired by DREAM-RSI, not a reproduction of
the paper's scientific benchmarks or its unrestricted policy-code search.

## Evidence and alternatives

Primary sources inspected on 2026-10-12:

- https://www.dream-rsi.com/assets/dream-rsi.pdf, section 3.
- https://github.com/zhengkid/Dream-RSI, README and release plan.

The upstream repository currently lists the full codebase and reproduction
scripts as being prepared. Its publication is not a dependency for this work.

Three implementation choices:

1. **External orchestration, recommended:** dependency-free Python modules next
   to the existing evaluation tools. Invoke the unchanged sigil release binary
   for online continuations. Keep discovery policy separate from agent execution.
   This is the smallest way to build the full loop without enlarging Rust's
   runtime surface.
2. **Native Rust/Lua orchestration:** integrate commands, policy VM, storage and
   scheduling into the binary. Better packaging, but a larger core and more
   coupling before discovery semantics have been validated.
3. **Replay only:** import prepared trees and compare policies. Useful as an
   internal milestone, but insufficient as the final deliverable because it
   cannot collect new worlds or redeploy a winner.

## Initial scope

The final deliverable includes online exploration and offline replay. Start
with one fixed, machine-checked JSON artifact task family, using patterns from
`context_eval.py`: field values and types, preserved unrelated data, protected
files, bounded reads, and recorded read-before-write workflow. Partial correctness
can provide a normalized score in [0,1]; full success still requires every check.
Process failure, timeout, invalid observations or unauthorized workspace changes
score zero and remain recorded. Generated code is not run by the checker.

Record task/evaluator versions and binary fingerprints. A history pool must
contain comparable task/evaluator/model settings; do not mix incompatible
versions in policy selection. Each recursive round uses a fresh instance of the
same task protocol, with its concrete target saved in that world's metadata.

This fixture proves orchestration correctness. It does not establish improvements
on general coding tasks. General-purpose trusted evaluators and unrestricted
executable policy revisions are later extensions.

## Discovery tree

Each world has a versioned manifest, task definition and root node. Every attempt
records a unique ID, primary parent, generation order, objective score,
diagnostics, process status, relative snapshot/session references, fingerprints,
and observed request counts when available. Root score is measured by the same
evaluator on the initial artifact.

An online `CONTINUE(parent)` restores that parent's workspace snapshot into a
fresh local workspace, carries the task and ancestor diagnostics into the prompt,
runs sigil, evaluates the artifact and saves a new child. Initial policy context
uses bounded ancestor diagnostics and artifacts; conversation transcripts are
stored for audit, rather than blindly transplanting absolute-path tool calls
between directories. All branches use the same frozen harness and tool surface.
Candidate attempts change task artifacts, not the deployed harness.

The root remains eligible for new branches. A non-root node is eligible only
while it is a leaf in the currently revealed tree. Every recorded non-root node
has exactly one existing parent; only the root can have multiple recorded
children. Root children are ordered by recorded generation ordinal.

Snapshots are bounded regular files with safe relative paths; reject symlinks,
special files, path traversal and files outside the task manifest. Store a failed
attempt even when its workspace cannot be snapshotted; such a node is terminal
and cannot be resumed. Persistence is atomic, retains completed nodes if a later
attempt fails, and rejects malformed trees rather than repairing lineage silently.

Separate harness, hooks, tools, memory, sessions and delegates per online attempt.
These directories prevent state contamination; they are not an OS security
sandbox. Preserve existing approval gates and record checker violations.
Global sigil state and the user's repository are not deployment targets.

## Shared policy interface

Policies consume only completed, revealed observations and eligible continuation
IDs. They cannot inspect unrevealed scores, diagnostics or artifacts. Replay
may communicate that an eligible node has no remaining recorded continuation;
it cannot invent one or silently fall back to online execution.

The first policy family is a bounded declarative JSON configuration interpreted
by fixed code. Parameters control branch width, ranking by observed score/depth,
stagnation cutoff, success stopping and attempt limits. Independent branches can
be grouped in batches of at most W workers; a batch cannot repeat the same parent.
Tie-breaking is deterministic. Enforce finite numeric values and explicit bounds.

This intentionally limits search to the implemented policy family. Candidate
files are data, not executable Python/Lua. Include baseline breadth/refine
configurations and an offline parameter sweep; importing validated candidate
configurations allows later model-generated revisions without changing replay.

Online and replay share the same action selector and termination checks. Online
worker count is one in the first implementation; replay accepts batching so its
semantics can be tested before adding concurrent online execution. Record this
constraint explicitly instead of claiming parallel live discovery.

## Replay and selection

Replay starts each policy/world pair from the root, revealing outcomes only
after selecting a continuation. Continuing the root reveals its next unused
recorded child; continuing a recorded leaf reveals its child if one exists.
Each batch reveals its children together before the next decision. Stop at an
empty batch, exhausted recorded support, or the configured round/attempt limit.

Report reached node IDs, decision batches, best observed quality, represented
attempt count N, completed decision rounds K, and the stopping reason. No sigil
subprocess, evaluator or network call is permitted in this code path.

Use the paper's fixed replay objective:

`best_quality - beta1 * N + beta2 * N / max(1, K)`

Coefficients are finite and nonnegative, configured explicitly and identical for
every policy on the same pool. N represents historical generation/evaluation
attempts; it is not an API-request count or a monetary estimate. Defaults and
quality normalization are saved with the report, rather than implicit.

Evaluate every candidate, including the incumbent, on every world in the fixed
pool. A candidate with validation/replay failure on any world is ineligible.
Average objectives across the pool, prefer the incumbent on ties, and promote
only a strict improvement. Save all comparison results and the selected policy
atomically in local experiment state. The guarantee is limited to replay score
on this pool; no guarantee is made about unseen online discoveries.

## User interface and deployment

Provide a `test-tools/dream.py` command with three operations:

- `explore`: collect one bounded tree using a supplied policy and the sigil binary.
- `replay`: compare validated policies on explicit recorded worlds, entirely offline.
- `run`: repeat explore, append world, replay/select and redeploy for a configured
  number of rounds; the new policy affects the next round's exploration only.

Online commands require `--execute`; without it they show the exact budgets,
task, binary, model and output paths. Offline replay never requires this flag.
Require a new output directory for online runs, finite attempt/round/timeout
limits and an explicit model/base URL or normal environment configuration.
Keys remain in process environment and are never written to manifests or reports.
Do not record raw stderr in summary files, where provider errors may echo secrets.

Use the user-selected 3001 endpoint only for a final bounded online smoke run
after deterministic tests. If credentials are unavailable, preserve the working
implementation and report that limitation; do not ask about prices or bills.

Promotion writes a policy file in the experiment directory. Rollback means
selecting the previously saved incumbent; every round retains both fingerprints
and the comparison that justified its selection. No global harness installation,
automatic publishing, new daemon, Cargo dependency or network default change.

## Validation and completion criteria

- Synthetic branching trees distinguish breadth, refinement and early stopping.
- Hidden high-scoring descendants never affect a decision before revelation.
- Replay never invokes a subprocess, evaluator or network client.
- A child is exposed at most once; non-root internal nodes cannot be continued.
- Exhausted support terminates with an explicit reason; no fabricated outcome.
- Multiple worlds, batching and objective arithmetic match hand-calculated cases.
- Incumbent survives ties, worse candidates and invalid candidates.
- Malformed lineage, duplicate IDs, non-finite scores, excessive inputs and unsafe
  snapshot paths are rejected with errors that do not print raw records.
- Local fake-provider E2E uses the actual release binary for at least two recursive
  rounds, restores parents faithfully, persists failures and deploys the selected
  policy into the next round. Simulated outcomes are labelled as such.
- Bounded live smoke, when credentials permit, is reported separately from fake
  validation and does not claim general policy improvement from a tiny sample.
- README and README_cn explain usage and the constrained initial policy family.
- Existing Rust checks and relevant Python evaluation tests remain green.

Design self-review: the final scope has a real online continuation path and
redeployment, not only a static replay report. Search is limited explicitly to
recorded support and a declarative policy family. Fees are outside this work.
