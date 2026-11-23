# Context compaction A/B — 2026-11-23 (local tool-using loop)

Trigger for this measurement: the 2026-10-12 A/B (`context-ab.md`) used a
synthetic, **tool-less** history with cache uncontrolled and found 36.5% fewer
input tokens. The question left open there is the one that decides whether the
compaction subsystem earns its keep: **does it still pay when the loop actually
calls tools?** Tool schemas are a fixed, incompressible prefix, and compaction
rewrites the prefix it sits in.

- Endpoint: `http://127.0.0.1:12345/v1` (MLX), model `Qwen3.8-Flash-Next-Sushi-2.6bpw`
- Binary: `target/release/sigil` @ `659ca52`
- Task (identical in both arms): read `f1.txt`…`f5.txt` in full (119 lines each,
  ~4 KB), report the max `value` per file into `result.txt`
- `--max-steps 12`, real tool calls, warm cache, `SIGIL_DEBUG_CACHE=1`
- Arms: `max_context_tokens = 0` (off) vs `12000` (aggressive; default is 48000)

## Result

| Observation | Compaction off | Compaction on (12k) | Δ |
|---|---:|---:|---:|
| Steps | 12 | 12 | 0 |
| **Prompt tokens (total)** | **186,690** | **205,009** | **+9.8%** |
| Cache-hit prompt tokens | 160,044 (85.7%) | 170,379 (83.1%) | −2.6 pt |
| **Peak single request** | **19,069** | **21,705** | **+13.8%** |
| Completion tokens | 677 | 791 | +17% |
| Task result | correct | correct | — |

Prompt growth per request with compaction **on**:
`7557 → 10268 → 10377 → 13071 → 15765 → 18459 → 21153 → 21298 → 21502 → 21583 → 21705`

That trace is the finding: the configured **12,000-token budget was exceeded by
57–81% on six consecutive requests**. The threshold was never a bound.

A third arm (`4000`, below the 20 tool schemas' own cost) produced 72,575 prompt
tokens vs 72,635 off — i.e. compaction silently became a no-op, because
`transcript_limit = max_tokens − tool_tokens` goes to its 1-token floor and the
safe-boundary walk keeps everything.

## Why compaction loses here

1. **Tool schemas are not compressible.** With 20 advertised tools the first
   request is already ~4.9k tokens of system prompt + schemas. A budget below
   that makes the transcript limit zero.
2. **The tail it must keep is indivisible.** A single turn's tool results are
   larger than the remaining budget, and the design (correctly) refuses to cut
   mid-turn — "over-retention is recoverable, silent result loss is not". So the
   request stays above threshold.
3. **Every compaction rewrites the prefix, which is exactly what the provider
   cache rewards.** Observed: cache-hit rate fell from 85.7% to 83.1% while total
   tokens rose. This is the cost the README already warns about — "compression
   can invalidate the changed prefix and increase first-request cost" — and it is
   large enough to swamp the saving in a tool loop.

## What this does and does not establish

Establishes: on this workload shape (multi-step, tool-heavy, warm cache), an
aggressive compaction setting made the run **strictly more expensive** at equal
quality, and did not bound request size. The mechanism is not free; it is
negative here.

Does not establish: behaviour on (a) genuinely long single conversations without
tool calls, where the 10-12 A/B did show savings; (b) a cold cache, where the
prefix rewrite costs nothing extra; (c) huge sessions that would otherwise
exceed the model window, where compaction is not an optimisation but the only
way to continue. n=1 per arm, one provider, one task family — directional
evidence, not a billing measurement.

## Recommendation

Do not delete the subsystem — compaction is still the only escape from a full
context window. Fix the economics instead:

1. **Never compact unless it clears the target with margin.** Right now the
   attempt runs, fails to reach the budget, and still pays for the rewrite.
   Gate on the *predicted post-compaction size*; skip when it would not land
   well under the threshold (e.g. ≥ 50%).
2. **Amortise: compact rarely and deeply.** Halve the transcript rather than
   shaving to the threshold, so the prefix is rewritten once per long run
   instead of repeatedly.
3. **Do not count tool schemas toward the transcript budget.** They are a fixed
   prefix; subtracting them (as now) makes small budgets meaningless.
4. **Keep the 48,000 default.** It is high enough to stay inert for ordinary
   sessions; the damage comes from users lowering it on the README's advice
   without this measurement.

Raw session data was produced in `/tmp/ctx-ab` and is not committed.
