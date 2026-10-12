# DREAM closed-loop smoke

The opt-in discovery/replay/select/redeploy loop completed two real rounds using
the user's localhost:3001 endpoint and `z-ai/glm-5.3-flash`. All four attempts
passed the fixed JSON artifact and recorded tool-workflow checks. Authentication
was supplied only in transient process memory/environment; raw sessions and
workspaces remain in temporary experiment storage, not in this report.

| Round | Executed policy | Attempts | Passed | Incumbent / selected replay objective |
| --- | --- | --- | --- | --- |
| 1 | Two branches, no success stop | 2 | 2 | 0.980 / 0.990 |
| 2 | One branch, no success stop | 2 | 2 | 0.985 / 0.990 |

The first selection changed the actual second-round exploration: the first
world opened two root branches, whereas the second refined its first branch.
The second selection added stopping at score 1 for the next deployment.
The saved policy and snapshots demonstrate deployment, rather than only an
offline comparison. Per-world stop reasons in the accompanying JSON were
recomputed offline after fixing support-exhaustion classification.

An important limit appeared in this small run: the first selected policy scored
well because its historical leaf had no recorded child to continue into.
Online, that leaf could be refined, so the second round still used two attempts.
Replay improvement therefore does not guarantee fewer future executions. The
newly collected world exposed that continuation and changed the next selection.

The objective uses best normalized artifact score minus 0.01 per represented
attempt, with no batching bonus. It counts recorded discovery attempts, not API
requests. Online execution is sequential. Search is over bounded declarative
policy parameters, not arbitrary model-written policy code.

A separate transport check without authentication retained three failed,
zero-scored nodes; it is excluded from the successful artifact smoke. This
confirms failed attempts are retained rather than disappearing from history.

The actual-binary fake-provider regression separately verifies a two-round case
where the selected success-stop policy reduces attempts from three to one.
Those provider outcomes are simulated and are labelled separately in generated
worlds and reports. This is orchestration validation, not a general coding
benchmark or a reproduction of DREAM-RSI's published discovery results.

Machine-readable evidence: [2026-10-12-dream-smoke.json](2026-10-12-dream-smoke.json).
