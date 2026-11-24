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

## 修复尝试失败记录（2026-11-23，已回滚）

第一次尝试：在 `PromptBudget::compact` 里加"压缩收益低于 25% 就不压"的门禁，并给收尾调用加 `compact_forced()`。结果 4 个测试变红，回滚。

失败原因（已用临时日志确认，不是猜的）：

| 测试 | 配置轴 | 日志 | 结论 |
|---|---|---|---|
| `request_budget_includes_tool_schemas…` | token=1000 | transcript=292, limit=128, tools=872, cand=277（只省 15 个 token） | 872 token 的 schema 已占掉预算，压不进去；测试只断言"消息数变少"（42→20），而 token 几乎没动、请求仍 1149>1000。**这条断言恰好固化了本报告要修的缺陷** |
| `reactive_compaction_retries…` | 消息数轴 | transcript=115, limit=0, cand=111（省 4） | 门禁挡掉了主动压缩，反应式那条只能把 21 条砍到 10 条，达不到 `<8` |
| `next_turn_request_retains…` | `max_context_messages=6` | transcript=206, limit=0 | 纯消息数配置。这里"消息数变少"本身就是目的，token 收益门禁不该适用 |
| `appended_tool_output…` | token=1000 | transcript=3550, limit=969, cand=3508（省 42） | 当前轮 10k 字符的工具输出不可切分，压不进去；但测试要求"当前工具输出必须触发一次压缩" |

结论：**一个纯"按 token 收益"的门禁无法同时满足这四条契约**，因为其中两条本身要求的正是"压不进去也要压"。

**下次正确的做法**（按这个顺序做，不要再试百分比门禁）：

1. 门禁只作用在 token 轴：`max_msgs > 0` 时完全不适用（消息数就是契约）。
2. token 轴改用**深压摊薄**：一次砍到预算的一半，而不是只蹭到阈值边缘。这样同一次重写买来多步余量，避免每步重复重写前缀（本次实测的病因就是每步重写）。
3. `request_budget_includes…` 那条断言要改写成 token 级契约（要么压到预算内，要么历史不动），并在提交信息里说明它原本钉住的是缺陷行为。
4. 改完必须重跑本报告的 12k 对照组，用同一任务对比 205,009 这个基线；如果深压没有改善，就承认这个修复不成立，只保留"默认 48k 别调低"这条结论。

## 第二轮：修复后重测（2026-11-23，结论：收益未证实）

按"深压 + 收益门禁"改完后，同一任务、同一 12k 阈值、同一端点，配对重跑：

| 臂 | prompt tokens | 任务结果 |
|---|---:|---|
| 压缩关 | 171,665 | OK |
| 压缩关 | 202,042 | **FAIL**（未写出 result.txt） |
| 压缩开 12k（修复后） | 207,478 | OK |
| 压缩开 12k（修复后） | 212,393 | OK |
| 压缩开 12k（修复后） | 169,316 | OK |

**结论：第一轮那个 +9.8% 处于同一配置的运行间噪声之内（±20%），不能作为"压缩变贵"的证据；修复也没有带来可测的节省。** 任务成功率两边都在抖（关压缩那臂也出现 1 次失败），与压缩无关。

因此本报告撤回"aggressive 压缩使成本上升约 10%"这个量化结论，只保留可复现的定性事实：

1. **阈值从来不是硬上限**——旧策略下请求尺寸连续 6 次超出配置 57–81%，且从未被压回预算内。
2. **小预算会静默退化**（4000 那组：72,575 vs 72,635，几乎无差别），因为工具 schema 已占满预算。
3. **很多小消息的会话压不动**——摘要按每条消息留片段并封顶 16k 字符，小消息多时摘要和原文一样大，这就是旧策略"压不下去却反复重写前缀"的机制原因。
4. **默认 48,000 基本不触发**，普通会话不受影响；风险来自把阈值调低。

保留的代码改动只有三条**可单测证明**的规则，不再宣称省 token：

- 历史本来就在预算内 → **绝不重写**（重写必然作废前缀缓存，收益为零）。
- 超出预算且可压 → **一次深压到预算内**（单测断言：压后必须低于预算，且一次至少减半），避免每步重复重写。
- 远超预算（≥2×）压不进去 → 仍然压（此时折掉旧历史是止损，不是优化）。
- 消息数轴（`max_context_messages`）**不套用收益门禁**：那条轴上消息数减少本身就是契约。
