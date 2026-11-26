# sigil

**一个 Rust + LuaJIT 的 coding agent，带自进化 harness。**

单二进制。任意模型。harness 自我进化。

```bash
# 一次性管道模式
echo "修复 src/main.rs 里的 bug" | sigil

# 交互式 REPL（TTY）：多轮对话 + 人工审批
sigil

# ACP server 模式（供编辑器/其他 agent 驱动 sigil）
sigil acp

# MCP server 模式（把 sigil 的工具暴露给 MCP 客户端）
sigil mcp
```

sigil 是用 Rust 从零开发的终端 coding agent。它说 OpenAI 兼容的 chat API，
所以 vLLM、Ollama、llama.cpp、DeepSeek 和任意 OpenAI 兼容端点开箱即用。
**LuaJIT 已嵌入二进制**作为进化引擎：模型可以在运行时编写自己的 Lua
工具、热注册、从内部成长 agent 能力——无需重新编译。

### 上下文预算与缓存诊断

活动目标提示词只展示目标和固定回合预算。不断变化的已用回合数保留在持久状态及 `manage_goals` 输出中，计数本身不再改写提示词前缀。目标新增、完成、暂停和预算变更仍会刷新上下文。

从事件历史重建系统提示词时，技能相关性与 `on_prompt_build` 使用最后一条用户消息。因此管道续接能按本轮任务选择技能，而不是沿用会话最早任务的输入。

技能相关性决定内联哪些正文；同分时及选中集合的最终输出均按名称排序。因此，仅相关性分数变化不会重排相同技能集合。技能集合变化、正文编辑及动态 hook 输出仍会改变提示词。

默认在估算的输入提示词达到约 48,000 tokens 时触发压缩，不再按消息条数触发。在 `[agent]` 中按实际模型配置：

```toml
[agent]
max_context_tokens = 48000
max_context_messages = 0
planning_interval = 0       # 每 N 步重述一次计划（0 = 关闭）
```

每个零值关闭对应触发条件；两个都为零则关闭主动压缩。估算包含消息封装和工具定义。真实 usage 用于校准当前请求，而不是复用上一次请求的绝对 token 数；新增工具输出和历史缩短都会重新计入。首轮粗估为非 ASCII 字符约两个 tokens、ASCII 约每四字符一个 token；它是启发式，不是模型 tokenizer。阈值不是请求大小的硬上限：固定系统指令、摘要开销和不能拆开的当前工具回合仍可能超出它。应按实际模型上下文窗口设置更低阈值，为输出预留空间。遇到服务端上下文错误时，仍允许一次被动压缩重试。

压缩将开头的系统指令和当前目标固定保留在历史摘要之外，使用有上限的多行摘录，保留已有摘要内容及工具调用/结果配对。它仍然有损：用户消息最多保留 2,000 字符、助手 1,000 字符、工具 500 字符、已有摘要 8,000 字符；合并摘要最多 16,000 字符，另附截断提示。被省略的细节应从原始会话日志查阅。压缩不会额外调用 LLM 生成摘要。

`SIGIL_DEBUG_CACHE=1` 观察实际流式请求、工具重放和最终总结请求，不发送诊断生成请求。流式请求主动要求 usage；在 `finish_reason` 后最多等待 250 毫秒、读取 32 个尾帧以收集 usage。缺失 usage 或缓存字段时明确报告不可用，不当成零命中。会话事件 sink 保存 `request_usage` 元数据，记录请求类别、模型和观测到的 usage。聚合缓存指标单独保留已报告缓存数据的 prompt token 数；旧记录仍可读取，但其覆盖范围视为未知。同一请求的多个 usage 快照只计一次，采用最后观测到的快照，尾帧读取超时也保留已取得的数据。仅按 token 触发压缩时，若摘要无法降低输入估算，则保留原历史。这是 token 观测，不是完整账单：未报告的 usage、缓存写入费率、提供方内部重试及价格都不会被推算。压缩可能使变化位置之后的缓存失效，增加首次请求费用；token 更少不能证明省钱，应在实际提供方上比较成功任务的总费用与质量。


记录管道回合，供后续比较：

```bash
sigil --sessions-dir ./evaluation-sessions prompt -p "Read src/cli.rs and describe its first line"
sigil --sessions-dir ./evaluation-sessions --resume latest prompt -p "Continue the task"
```

显式指定 `--sessions-dir` 时，管道模式保存历史和请求 usage；使用 `--resume` 时，新事件追加到被续接的会话。不使用这两个选项的普通管道运行仍不保存会话。会话 ID 输出到 stderr；管道 ID 必须是普通文件名。日志包含对话和工具内容，应将实验数据保存在合适的本地目录。

离线汇总指定会话日志（仅依赖 Python 标准库）：

```bash
python3 test-tools/usage_report.py path/to/session.jsonl
python3 test-tools/usage_report.py --json path/to/session.jsonl
```

运行成对的合成任务状态保留实验（Python 3.11+）：

```bash
python3 test-tools/context_eval.py
python3 test-tools/context_eval.py --execute --output ./context-eval-run --model MODEL --base-url http://127.0.0.1:3001/v1
```

第一条命令只显示计划。第二条使用已在本机配置的 `SIGIL_API_KEY`；生成配置和汇总报告不包含密钥。输出目录必须是新目录。两种策略使用相同种子历史：一组关闭主动压缩，另一组默认以 8,000-token 阈值触发（可用 `--threshold` 调整）。默认两组重复实验交替执行顺序。`report.json` 包含严格答案正确率、请求数、观测 token、缓存覆盖率、策略汇总及二进制 SHA-256。失败尝试仍计入分母，中断或损坏观测保留用量不完整状态。默认样本只测试一个任务键的保留能力，不代表通用编码质量。服务端缓存状态不可控，不能据此判定冷/热缓存成本或金额节省。三步工具上限和 `--timeout` 限制执行，不是计费 token 上限；输出限制仍由模型服务控制。会话包含合成历史及真实模型/工具输出，应保存在本机。费用结论仍需核对服务商账单。

`write_file` 对任意目标均支持原样文本字符串；对于 `.json` 目标（扩展名不区分大小写），也接受 JSON 对象或数组并序列化为格式化 JSON。其他文件类型的结构化内容，以及 null、数字和布尔内容，均在文件操作前拒绝。

可用 `--case json-edit` 改为文件修改样本：

```bash
python3 test-tools/context_eval.py --case json-edit --execute --repetitions 1 --output ./context-json-run --model MODEL --base-url http://127.0.0.1:3001/v1
```

该样本要求模型读取并按历史指令修改 `settings.json`。成功需同时满足字段值与类型正确、无关嵌套字段和 Unicode 保留、受保护文件不变、进程成功退出，以及日志记录先 `read_file` 后 `write_file`。使用其他工具、访问其他路径或产生额外工作区变化均判失败；会话日志和运行时目标计数不纳入工作区变更检查。JSON 读取有大小限制，拒绝重复键、符号链接和非普通文件；检查器不执行生成代码。它是一个小型工具使用/文件成果检查，不代表通用编码能力。单次配对不平衡执行顺序，也不能证明质量或费用不劣。

不调用模型即可分析已有成对报告：

```bash
python3 test-tools/context_cost.py docs/evaluations/2026-10-12-context-json-repeat.json
```

未提供单价时，只输出观测 token 差额和费用公式，金额字段保持 null。需要估算时，必须一起提供 `--uncached-input-rate`、`--cached-input-rate`、`--output-rate`（每百万 token 的货币单位）及 `--currency`。单价须与实际计费模型/路由匹配，工具不会自动查询。价格须有限且非负（上限 1e12、28 位十进制数字、指数 -36 至 12）。用量不完整或缓存分类未知时不计算完整估算；缓存与未缓存输入单价相同是例外，因为分类不影响费用。失败尝试仍计入支出，零成功时每个成功任务的费用为 null。重复或缺失配对、样本版本不一致和混合模型使用同一单价均会拒绝。估算不包含未报告请求、缓存写入费及其他账单项目。缓存丢失的回归样本证明：在明确标注的假设价格下，更少输入 token 也可能更贵；它不代表当前服务商报价。

JSON 输出按模型和请求类型分组。缓存命中率只使用已报告缓存覆盖的输入 token 作分母；`cache_coverage_rate` 表示报告缓存数据的请求比例。缺失观测不计入 token 总和，通过请求计数和 `usage_complete` 明确显示；没有 `request_usage` 的旧日志无法还原用量。同一路径重复传入只统计一次，但重叠副本或分叉会话中重复的历史必须手动排除。无效记录以文件和行号报错，不打印记录内容。比较压缩策略时，应使用相同固定任务、模型、工具定义和输出限制，另行验证任务成功率，并同时比较服务商账单和观测 token。冷缓存与热缓存实验应分别统计。该脚本不发送模型请求，也不推断任务成功或金额。


## DREAM 探索与重放

`test-tools/dream.py` 在现有 sigil 二进制之外提供按需开启的闭环：
真实探索 → 历史重放 → 策略择优 → 下一轮部署。

```bash
# 预览具体任务与执行上限，不调用模型。
python3 test-tools/dream.py run --output ./dream-run --rounds 2 --decision-rounds 3 --attempts 3 --model MODEL --base-url http://127.0.0.1:3001/v1
# 收集新探索树，把胜出策略用于下一轮。
python3 test-tools/dream.py run --execute --output ./dream-run --rounds 2 --decision-rounds 3 --attempts 3 --model MODEL --base-url http://127.0.0.1:3001/v1
# 只使用已记录结果比较策略，不执行 agent 或评估器。
python3 test-tools/dream.py replay --world ./dream-run/round-1/world.json --world ./dream-run/round-2/world.json --policy ./dream-run/selected-policy.json --output ./dream-replay
```

在线输出目录必须是新目录，认证从环境变量 `SIGIL_API_KEY` 读取。每次尝试
使用独立的任务和 agent 状态目录，防止状态串扰；这些目录不是操作系统沙箱。
首个任务族是 JSON 文件编辑，用机器检查值和类型、无关字段、受保护文件以及
先读后写的工具流程。失败尝试保留在树中，每次接续都恢复经过指纹校验的父快照。

首版策略是受限 JSON 参数，由固定解释器和参数搜索控制分支宽度、已观测评分
或深度排序、停滞截止、成功停止和尝试上限；尚不支持模型自由编写策略代码。
`--candidate policy.json` 可以加入额外的有效配置。在线执行使用单 worker，
`replay --workers N` 支持历史批次重放。目标函数是最佳归一化质量减去
`--beta1` 乘以尝试数，再加上 `--beta2` 乘以每个决策轮的尝试数。
现有策略始终参与比较，同分保留现有策略；不退步只针对已记录池中的重放评分，
不保证未见任务的效果。重放不能虚构未探索分支，也不会在线补跑缺失结果。

`run.json`、每轮的 `world.json`/`replay.json` 和策略文件保存部署与回滚证据。
通过 `--policy` 指定某轮的 `incumbent-policy.json`，可从旧策略开始新实验。
不带 `--output` 的 replay 仅打印报告。模拟接口必须标记 `--source simulated`，
模拟世界与在线世界不能混入同一个择优池。确定性测试：
`python3 -m unittest discover -s test-tools -p 'test_dream*.py'`；设置
`SIGIL_DREAM_E2E=1` 可运行真实二进制与本地模拟接口的闭环测试。
此任务验证编排正确性，不代表通用编码能力提升，也不是论文基准复现。

## 证据：harness 真的在进化

核心命题——*能修改自己 harness 的 agent 会随时间变强*——不是路线图上的
愿景，而是已经用可复现实验验证过的（2026-08-19，8 个 epoch，
`benchmark/` + [`evolution-benchmark.md`](evolution-benchmark.md)）：

**实验设置。** 固定任务池，机器可判的客观断言（文件内容、退出码——
不靠 LLM 自评）。进化组：sigil，允许跨 epoch 创建工具/技能/钩子。
对照组：dsh，能力面**冻结**。同一模型（本地 SGLang 端点上的
Qwen3.8-27B）、同一任务、同一套断言。

**结果（每 epoch 通过率）。**

| Epoch | 1 | 2 | 3 | 4 | 5 | 6 | 7 | 8 |
|---|---|---|---|---|---|---|---|---|
| sigil（进化组，全部 3 任务） | 1/3 | 2/3 | 3/3 | 1/3 | 2/3 | 3/3 | 3/3 | 2/3 |
| dsh（冻结对照，T1 系列） | 0/2 | 0/2 | 0/2 | 0/2 | 0/2 | 0/2 | 0/2 | 0/2 |

- 进化组呈清晰的「投资期 → 爬坡 → 高位平台」曲线（T1 系列按任务族
  6/8 与 4/8；T2 达 7/8，harness 路径事实跨 epoch 被召回）。冻结对照
  组 T1 系列 **0/16**——同模型、同任务。
- 能力面 8 个 epoch 内从 **7 → 39 个资产单调增长**。工具不只是被创建，
  还被*迭代*：同一个求和工具历经 3 个版本、保留回滚快照；后期 epoch
  复用既有工具，耗时显著缩短。
- 所有自建工具走宿主沙箱（`host.*`），全程零安全告警——但实验同时给出
  了安全栈最有力的存在理由：进化过程自发长出了包装 `DeleteFileA` 的
  FFI 工具。评审门/执行环把它兜住了——这正是它们是硬需求的证明。

**诚实的局限。** 单模型、单 seed、8 个 epoch、尚无 held-out 新任务族
——支持命题，但还不能排除 benchmark 过拟合。下一轮：多 seed、更大
模型、held-out 任务，以及 candidate → evaluate → promote → rollback
全自动流水线（零件——版本化回滚、refinement 门、可逆效应——都已就位）。

> 状态：**薄化 P-C（v0.2.0）** — Lua 策略层已上线：agent loop 钩子
> （`on_turn_start` / `on_tool_result` / `on_prompt_build`）在沙箱中运行
> `~/.sigil/hooks/` 下的 Lua 文件，每次使用从磁盘重载；`improve_harness`
> 新增 `edit_hook` / `delete_hook` / `list_hooks`（`return function` 契约门 +
> 评审门 + kind=hooks 版本化回滚）——模型可在会话中途自写策略钩子，
> 下一轮即生效，无需重启。另：MCP 客户端 Streamable HTTP 传输。
>
> 状态：**Phase 71** — cron 定时任务（prime-agent `cron-jobs.ts` 最小版）：
> daemon 帧 `{"type":"cron","action":"add|list|pause|resume|remove",...}`
> 管理持久化作业（`<sessions_dir>/cron.json`），`interval`（每 N 秒重触发）与
> `once`（unix 时间点触发）两种调度；30s 调度循环把到期作业像用户 turn 一样
> 注入目标会话。E2E：interval-5s 作业添加后被调度器触发，prompt/回答轮次落入
> 会话文件，run_count/next_run_at 正确跟踪。
>
> 状态：**Phase 70** — 旁路提问（prime-agent `side-question.ts`）：daemon 帧
> `{"type":"side_question","text":...,"id":...}` 基于会话上下文的克隆回答，
> **无工具**、单轮预算，流式 `side_delta` + 回 `side_answer`——会话历史不受
> 任何影响。E2E：daemon 正确回答关于自身对话的旁路问题，磁盘会话只保留主
> 会话轮次。
>
> 状态：**Phase 69** — 嵌套深度上限（prime-agent `RLM_MAX_DEPTH`，默认 1）：
> 每个 `DelegateTool` 实例带 `depth`（0 = 顶层）；子代理的工具注册表拿到的是
> depth+1 副本（替换共享父实例），因此被委托的子代理的 spawn/delegate
> 被拒（「nesting depth limit (1) reached」）。E2E：子代理两次尝试委托均被拒，
> 并通过 `delegate list` 核验无子子代理产生。
>
> 状态：**Phase 68** — per-agent condenser（OpenHands `definition.condenser`）：
> `spawn` 与 `delegate` 批量的每个 task 接受 `max_context_messages` /
> `max_context_tokens` 覆盖——该子代理的上下文预算替换父代理全局设置。E2E：以 `max_context_messages: 4` / `max_context_tokens: 3000`
> 派发的子代理在紧预算下正常完成。
>
> 状态：**Phase 67** — max_children 上限（OpenHands=5）：同时最多 5 个子代理
> 在跑——`delegate spawn` 与 `delegate` 批量都在前置检查上限（running 占用
> 额度，finished 释放额度；超限批量被拒并提示「拆小批」）。E2E：模型先试
> 6 任务批量被拒，重新规划为 3 任务批量。
>
> 状态：**Phase 66** — `delegate` metrics（OpenHands `usage_to_metrics`
> 视图）：`action=metrics` 列出每个子代理自己的 prompt/completion 用量与
> 总计——委托账单一眼尽览（用量在完成时记录一次，替换而非合并，resume 永不
> 重复计数）。另：reqwest client 现设 `no_proxy()`——`http_proxy` 环境变量
> 曾劫持 loopback 端点（502 破坏了本地 LLM 端点与 fallback 测试）。
>
> 状态：**Phase 65** — `delegate` 阻塞批量（OpenHands `delegate` 语义）：
> `action=delegate` 传 `tasks` 数组并发 spawn 所有子代理并阻塞等全部完成，
> 一次调用返回聚合结果（可选 `timeout`，默认 600s；未完成者如实报告而非
> 报错）。
>
> 状态：**Phase 64** — daemon 多会话（herdr pane 模型）：一个 daemon
> 并行承载多个会话；`sigil send --session foo` 首次使用时开新会话并持续
> 追加；`{"type":"session","action":"list"|"new"}` 列出/新建会话。每会话
> 独立历史、独立持久化。
>
> 状态：**Phase 63** — `delegate spawn` 接受可选 `name` 参数（rlm
> `session_name` 校验：trim、非空、≤64 字符——镜像
> `normalizeRequestedRlmSubagentSessionName`），记录在 handle 上并在
> `resume` 时保留。
>
> 状态：**Phase 62** — 压缩由真实 token 用量驱动：
> `compact_messages_with_budget` 现在接收最近一次 LLM 调用的真实
> `prompt_tokens`（prime-agent 用实际 `usage.totalTokens` 而非 chars/4
> 估算）：驱动超限触发 + 用真实/估算比值校准保留尾部的逐条估算
> （chars/4 对中文密集转录低估约 4 倍）。预算检查每轮迭代都跑——
> 第一步用估算、之后每步用实测用量。
>
> 状态：**Phase 61** — token 用量计量：解析 provider 的 `usage` 块
> （流式 SSE 末块 + 工具回放 completion 两条路径），跨整次运行累计进
> `RunResult.tokens`，持久化到 delegate handle 并在 `delegate status/list`
> 显示（如 `10598 prompt + 328 completion tokens`）。rlm/OpenHands 对照的
> metrics 缺口由此关闭——不需要 provider 改动，只差解析。
>
> 状态：**Phase 60** — 父→子消息（rlm `agent_message.send` 的另一半）：
> `delegate send` 把消息排入子代理 inbox（限长、持久化到 meta.json），
> 每个子代理获得 `check_parent_messages` 工具 + 注入的
> COMMUNICATION RULES 段（提示每步开始先检查）；`resume` 时排队消息
> 转移到新句柄（transfer_inbox——延续轮继续可见，旧记录不会重复消费）。
> rlm 的消息语义现已完整覆盖。此前：Phase 59 子→父消息、Phase 58
> 按子任务选模型（rlm.find_models 最小版）、Phase 57 `delegate delete`
> （rlm registry 语义补全）、Phase 56 子代理步数可见性（metrics 第一片）、
> Phase 55 refine 审计 id + 合并历史（prime-agent `mergeRefinementHistory`
> 模式）、Phase 54 子 agent 类型化（AgentDefinition 模式）、Phase 53
> 子任务 resume + delegate 注册表持久化、Phase 52 scope 策略、Phase 51
> 双轨本身（--local-harness-dir 合并视图 + scope 参数）、Phase 50 Lua
> 沙箱加固、Phase 49 结构化运行结果（RunResult + provide_final_answer）、
> Phase 48 外部调研落地（并行工具执行、反应式压缩重试、规划检查点、
> 子 agent 报告模板）+ 48r daemon 死锁修复、Phase 47 CLI 一致性、
> Phase 46 daemon 体验打磨、Phase 45 ratchet 进 CI + goal turn 预算。
> Phase 44 daemon、Phase 43 异步委托、Phase 42 持久目标、Phase 41
> 改进门控 + token 预算压缩、Phase 39
> 记忆写安全、Phase 38 LLM 故障转移、Phase 36 会话续接与 Phase 35
> 跨实例知识同步。管道模式、
> 带真实 FFI 和 host helpers 的脚本化 Lua 工具（运行时创建/更新/删除/列出）、
> **带声明式依赖的技能动态选择**（`> depends_on:` 头部——依赖被
> 传递性内联，环安全）**和环检测**（add_skill 在写入形成依赖环时
> 警告模型；`list_skills` 显示每个技能的描述、依赖和环警告）、
> **带版本历史、会话内热回滚、GC 和实时
> prompt 状态**（harness 编辑下一轮生效，无需重启）的自进化闭环
> （每个模型可写资产）、三层 safety、MCP 客户端**和 server**（工具、
> 进度通知、双向 resources **和 prompts**）、ACP server **和 client**
> （跨进程委托）、持久 memory（**remember / recall / memory_list /
> memory_delete**）、可续会话（含 **JSON 导出/导入**）、子代理委托、
> 流式输出、**10 任务 ratchet 回归电池**（files/bash/git、记忆生命周期、
> 技能依赖、live prompt 编辑、工具创建、MCP server——全部对真实
> 二进制和 LLM 运行）、模型驱动 harness 改进、技能内容内联、
> TUI 彩色输出、`/tools` 搜索、`sigil doctor` 环境自检、shell 补全
> ——全部端到端可用。

## 快速开始

```bash
cargo build --release

# 管道模式（环境变量配置；默认走 cc-switch 代理 + deepseek-v4-flash）
echo "这个仓库里有哪些文件？" | ./target/release/sigil

# 交互模式
./target/release/sigil
#   /tools    列出可用工具
#   /session  显示当前会话 id
#   /quit     退出

# 可续会话
./target/release/sigil --list-sessions
./target/release/sigil --resume <会话id>
```

### 配置

| 环境变量 | 默认值 | 用途 |
|---------|--------|------|
| `SIGIL_MODEL` | `deepseek-v4-flash` | 模型名 |
| `SIGIL_BASE_URL` | `http://127.0.0.1:15722/v1` | OpenAI 兼容端点（cc-switch 本地代理） |
| `SIGIL_API_KEY` | `EMPTY` | API key（bearer token） |
| `SIGIL_BASH_TIMEOUT_SECS` | `60` | 单条 `bash` 命令的墙钟预算（秒）；超时后进程被杀 |
| `SIGIL_NO_PROGRESS` | `2` | 无进展守卫：连续 N 轮无进展后收尾停止（`0` 关闭） |

任意 OpenAI 兼容端点都可通过 `SIGIL_BASE_URL` 使用——vLLM、Ollama、
llama.cpp、DeepSeek 等。配置文件位于平台配置目录
（Linux `~/.config/sigil/config.toml`、Windows `%APPDATA%\sigil\config.toml`；
`[llm]`、`[agent]`、`[[mcp.servers]]`）；优先级 CLI > 环境变量 > 文件 > 默认值。
另有 `--tools-dir`（Lua 工具，默认 `配置目录/sigil/tools/`）、`--harness-dir`
（system prompt + skills，默认 `配置目录/sigil/harness/`）、`--memory`
（remember/recall 存储，默认 `配置目录/sigil/memory.md`）。

## 能力

- **原生工具调用** — 流式补全 + 非流式重放拿完整 tool-call 负载
  （OpenAI function-calling；兼容任意后端）。
- **工具** — `bash`、`read_file`、`write_file`、`git_status/diff/log`，
  加上 **Lua 脚本化工具**（`配置目录/sigil/tools/*.lua`，文件名 = 工具名，
  `-- desc:` / `-- schema:` 头部注释）。
- **嵌入式 LuaJIT + FFI** — `mlua` + vendored LuaJIT。脚本可
  `require("ffi")` 调任意 C 库（zlib `crc32` 已端到端验证）。
- **Lua 执行限制** — Lua 初始化代码和导出函数受 5 秒期限及 1000 万条
  指令预算限制。禁用 JIT 执行以保证指令钩子能终止循环；钩子无法抢占
  阻塞的宿主调用或原生 FFI 调用。`--no-default-features` 同时禁用 FFI
  和原生库加载器。
- **自进化** — `create_lua_tool`：模型写 Lua → sigil 落盘并热注册 →
  下一轮即可调用，跨会话持久。
- **Harness 自我改进** — `improve_harness`：模型可编辑自己的
  `system_prompt.md`（`edit_prompt`）和新增技能（`add_skill`）——
  指令层从内部进化，免重编译。
- **Harness bundle** — `配置目录/sigil/harness/` 存放 `system_prompt.md` 和
  `skills/`；改文件（不改代码）即可改变 agent 行为。
- **Safety（三层）** — 审批门（破坏性命令非交互模式默认拒绝；REPL 里
  交互 `y/N`）、熔断器（连续失败 3 次熔断）、执行环（只读/工作区/
  harness 变更）。未知 Git 子命令、自定义 alias 和外部 Git helper
  同样需要审批。
  **请如实理解这些限制**：审批门只对 `bash` 做模式匹配 —— 其他工具一律
  放行；交互式 `y/N` 只存在于 REPL（pipe、daemon、cron、子代理都没有
  审批人）。真正约束其余工具的是写作用域（`write_file` 限定在工作区 /
  临时目录 / sigil 状态目录，`SIGIL_WRITE_ROOTS` 可扩展）、凭据读取黑名单
  （`read_file`、Lua `host.read_file`、`host.env` 拒绝钥匙材料——`~/.ssh`、
  `~/.aws`、`~/.netrc`、代理的 `*.db`，以及任何名字像密钥的环境变量；
  `SIGIL_READ_ALLOW` 可显式豁免某个路径）与冻结开关
  （`SIGIL_FREEZE=1` 拒绝一切对进化面的写入）。默认构建开启 LuaJIT
  **FFI**，模型自写的脚本工具可以调用任意 C 库 —— 需要隔离请用
  `--no-default-features` 构建。一句话：sigil 假设模型被信任、可代表你
  行动；不要在开启 FFI 的情况下把不可信内容喂给它。
- **Memory** — `remember` / `recall`：模型自己维护的跨会话持久事实。
- **MCP 客户端** — 配置 `[[mcp.servers]]` 拉起 stdio MCP server，其工具
  变成 `mcp_<server>_<tool>`。
- **MCP server** — `sigil mcp` 把 sigil 自己的工具暴露给任意 MCP 客户端
  （Claude Desktop、Cursor、其他 agent）。
- **ACP server** — `sigil acp` 说 Agent Client Protocol（stdio 上的
  JSON-RPC），任意 ACP 宿主（编辑器、其他 agent）都能驱动 sigil。
- **交互式 REPL** — 多轮对话 + 破坏性命令人工审批。
- **真正多轮上下文** — REPL 跨轮维护真实消息列表，包括工具调用、结果和
  压缩摘要；`--resume` 从持久化对话事件恢复上下文（模型记得对话，
  而非文本 blob）。
- **行编辑 + 历史** — rustyline：上下键历史、行内编辑；每会话
  `.history` 文件跨会话持久。
- **流式输出** — 管道与 REPL 模式实时流式显示 assistant 文本
  （`[using tools…]` 标记工具调用中断）。
- **可续会话** — REPL 每轮记录到 `~/.sigil/sessions/<id>.jsonl`；
  `--resume <id>` 继续对话，并恢复到会话开始时的原始工作目录
  （会话开始时记录），即使从别的目录续接，相对路径解析方式也不变。
- **子代理** — `delegate` 工具：主 agent 派发独立子代理（独立对话、
  独立工具循环），收回摘要。
- **Ratchet 测试** — `test-tools/ratchet.py` 用真实二进制 + 真实 LLM
  跑固定任务电池，断言核心行为不退化。

## 工作原理

```
stdin / REPL / ACP ──► agent 循环 ──► LlmProvider (OpenAI 兼容流式)
                          │  ▲
                          ▼  │
                    ToolRegistry (bash, files, git, Lua, MCP, memory)
                          │
                    safety (gate / breaker / ring)
```

- **`llm::LlmProvider`** — provider 契约；所有后端实现它，agent 循环与
  provider 无关。
- **`agent::run_turn(_with_approver)`** — 循环：流式 → 重放拿工具调用 →
  审批 → 执行 → 重复直到模型停止。默认带护栏：无进展守卫停止
  精确重复轮次（`SIGIL_NO_PROGRESS=0` 关闭）；步数预算（`max_steps`，
  默认 100）用尽时以一次无工具收尾调用结束，而非硬截断。
- **`tools::ToolRegistry`** — 所有工具统一 lookup；每轮刷新 specs，
  热注册的新工具立刻对模型可见。
- **`lua::LuaRuntime`** — vendored LuaJIT 开启 `ffi`；`host` 辅助表
  （`read_file` / `write_file` / `env`）是脚本↔宿主的安全边界。

## 与 Claude Desktop 集成（MCP）

`sigil mcp` 是标准 stdio MCP server，Claude Desktop 可以直接使用 sigil
的全部工具（bash、文件、git、Lua 脚本工具、memory、delegate 等）。
在 Claude Desktop 配置（`claude_desktop_config.json`）中添加：

```json
{
  "mcpServers": {
    "sigil": {
      "command": "/绝对路径/sigil",
      "args": ["mcp"],
      "env": {
        "SIGIL_MODEL": "deepseek-v4-flash",
        "SIGIL_BASE_URL": "http://127.0.0.1:15722/v1"
      }
    }
  }
}
```

`command` 指向 `cargo build --release` 产物，`env` 填你的端点，重启
Claude Desktop 后 sigil 工具即出现在 MCP 工具列表。

## 路线图

| 阶段 | 范围 |
|------|------|
| **0** ✅ | 脚手架、`LlmProvider`、ReAct 循环、bash 工具、管道模式、原生工具调用 |
| **1** ✅ | 文件/git 工具、审批门、TOML 配置 |
| **2** ✅ | **嵌入式 LuaJIT（mlua vendored）+ FFI 工具面** |
| **3** ✅ | Harness bundle + 自进化闭环（AHE）+ safety 三层 |
| **4** ✅ | MCP 客户端、ACP server、持久 memory |
| **5** ✅ | 交互式 REPL + 人工审批 |
| **6** ✅ | 会话持久化（`--resume`）、子代理（`delegate`） |
| **7** ✅ | MCP server 模式（`sigil mcp`） |
| **8** ✅ | 流式输出、ratchet 回归电池 |
| **9** ✅ | 真正多轮上下文、rustyline 行编辑 + 历史 |
| **10** ✅ | `improve_harness` — 指令层自进化 |
| **11** ✅ | REPL 内会话切换（`/sessions`、`/session <id>`） |
| **12** ✅ | ACP client + `acp_call` — **2026-11-23 移除**：与 `delegate` 功能重复，且让 sigil 变成"驱动其他 agent 的 agent"，多出一条无法审计的信任路径 |
| **13** ✅ | 技能内容内联（小技能直接嵌入 prompt） |
| **14** ✅ | TUI 彩色输出 + `/tools <查询>` 搜索 |
| **15** ✅ | `sigil doctor` 环境自检 |
| **16** ✅ | doctor LuaJIT/FFI 检查；子命令后全局参数 |
| **17** ✅ | shell 补全；v0.1.0 发布（5 平台二进制） |
| **18** ✅ | 技能动态选择；MCP 进度通知 |
| **19** ✅ | 脚本工具 host helpers；MCP resources（harness） |
| **20** ✅ | 工具生命周期（创建/更新/删除/列出）；MCP 资源读取 |
| **21** ✅ | MCP prompts（双向模板） |
| **22** ✅ | 版本库——可逆自进化（回滚） |
| **23** ✅ | 回滚热加载；版本 GC；MCP 热队列排空 |
| **24** ✅ | 会话导出/导入（可移植 JSON 转录） |
| **25** ✅ | 实时 harness 状态（编辑下一轮生效，无需重启） |
| **26** ✅ | 记忆生命周期（remember / recall / memory_list / memory_delete） |
| **27** ✅ | 技能依赖（`> depends_on:` 头部，闭包内联） |
| **28** ✅ | ratchet 电池 4 → 10（记忆、技能依赖、live prompt、MCP） |
| **29** ✅ | 依赖环检测（add_skill 对环发出警告） |
| **30** ✅ | 技能图视图（list_skills：描述、依赖、环警告） |
| **31** ✅ | 轮次时间戳 + `session export --since`（按时间过滤转录） |
| **32** ✅ | 跨进程目录锁（自进化写入加 advisory `.sigil.lock`） |
| **33** ✅ | Ratchet +2（ACP e2e、会话导出/导入）+ 脚本化工具 schema 加固 |
| **34** ✅ | 技能按需加载（紧凑常驻索引，正文按需读取） |
| **35** ✅ | 跨实例知识同步（每轮 harness 指纹检查 → 热重载） |
| **36** ✅ | 会话续接（`session list` 摘要 + `--resume latest`） |
| **37** ✅ | ratchet LLM 方差容忍（任务级重试 + retry_prompt 强制话术） |
| **38** ✅ | LLM 故障转移（端点链：瞬时故障重试下一端点，4xx 中止） |
| **39** ✅ | 记忆写安全（remember/delete 置于跨进程锁下） |
| **40** ✅ | 健壮性（工具超时 + 上下文压缩 + 原子导出） |
| **41** ✅ | harness 改进门控（reason 门控编辑 + 审计日志；token 预算压缩） |
| **42** ✅ | 持久目标（manage_goals 工具 + 每轮上下文注入 + goals.jsonl） |
| **43** ✅ | 异步委托（spawn → handle → status/result/list 注册表） |
| **44** ✅ | 后台 daemon（Unix socket agent 循环 + attach/send + 会话持久化） |
| **45** ✅ | ratchet 进 CI（--check-only）+ goal turn 预算（自动 pause） |
| **46** ✅ | daemon 体验打磨（attach 持久会话 + /quit、send --timeout、友好错误） |
| **47** ✅ | CLI 一致性（pipe --resume latest 生效、session list 时间戳） |
| **48** ✅ | 调研落地（并行工具调用、反应式压缩、规划检查点、delegate 报告） |

## 更新日志

完整的逐阶段历史见 [CHANGELOG_cn.md](CHANGELOG_cn.md)（条目为冻结历史——行为演进不改写）。

### v0.2.0 — 2026-08-13
委托与子代理、daemon + cron、健壮性、以及薄化路线（Lua 策略层）。要点：

- **子代理委托**：类型化代理（TOML frontmatter、工具白名单）、
  spawn / resume / delete / status、按子代理独立模型与上下文预算、
  父子双向消息、token 用量统计、嵌套深度上限 1。
- **Daemon + cron**：`sigil daemon` attach/send 会话持久化；
  cron 任务（`once` / 每 N 秒）；旁路提问。
- **健壮性**：并行工具执行、反应式压缩、规划检查点、
  LLM 故障转移链、版本化 harness 回滚、local/global 双轨 harness
  （prime-agent 风格）。
- **Lua 沙箱加固**：stdlib IO/OS 移除、只走 host helpers、require 白名单。
- **MCP**：MCP 客户端 Streamable HTTP 传输。
- **薄化路线（P-A/P-B/P-C）**：agent loop 钩子
  （`on_turn_start` / `on_tool_result` / `on_prompt_build`）、
  `improve_harness` 模型自写策略钩子 + 版本化回滚——模型可在运行时
  无重启地进化自身指令层。
- 194 个单元与集成测试。

### v0.1.0 — 2026-08-09
首个正式发布版本。完整 agent 核心：管道 / REPL / ACP / MCP 模式、
vendored LuaJIT 真实 FFI、自进化（Lua 工具 + harness 指令层）、
三层 safety、真正多轮上下文的会话、子代理委托、流式输出、
ratchet 测试、`sigil doctor`、shell 补全、48 个单元测试。

## 开发

Rust 2024，单二进制。`tokio`、`reqwest`（rustls — 无 OpenSSL 链接依赖）、
`clap`、`serde`、`mlua`（LuaJIT + vendored）。

```bash
cargo fmt -- --check
cargo clippy --all-targets -- -D warnings
cargo test
```

## 许可证

MIT。见 [LICENSE](LICENSE)。Copyright (c) 2026 alitrack。

English README: [README.md](README.md).
