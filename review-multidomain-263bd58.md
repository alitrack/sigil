# 多领域评审 + 减法决议 — sigil @ 263bd58

- 评审时间: 2026-11-23
- 评审对象: `main` HEAD `263bd58`（`feat(plan): dynamic-workflow plan language`），192 commits，src 29,871 行
- 评审方式: 本机实测（build/test/fmt/clippy）+ 源码逐条核实 + 两路独立第三方 agent 交叉对照
- 上一轮: `review-triage-c6e98c5.md`（2026-11-03，三方评审，EDG 判**打回**，列 5 个 P0）

---

## 0. 结论先行

**该做减法，而且减法是当前唯一的高杠杆动作。** 不是因为它"代码多"，而是因为：

1. **最近两个 commit 的因果是反的**：11-03 三方评审判 EDG 打回、5 个 P0 未修 → 之后 20 天里，P0 一条没修，反而新增了 EDG v2 inline authoring（`746c0cf`）和一整套 plan DSL（`263bd58`）。
2. **最后一个 commit 让仓库的 CI 红线变红**（本机实测，见 §1）：`cargo clippy --all-targets -- -D warnings` 报 18 个 error，全部来自 plan 模块的死代码。
3. **安全叙事与实现之间已经不是差距，是反向**：README 写 "three layers of safety"，实测第一层只对 `bash` 这一个工具名生效，其余工具无条件放行（§2.2）。

同时也要说公道话：**工程卫生罕见地好**（369 测试全绿、fmt 干净、CI 双构建模式、containment build 的意识、模块级注释质量高）。问题不在"写得糙"，在"写得好的东西被并联进了错误的形状里"。

---

## 1. 证据基线（本机实测，可复现）

| 质量门 | 结果 |
|---|---|
| `cargo build` | ✅ 通过 |
| `cargo test --quiet` | ✅ **369 passed, 0 failed** |
| `cargo fmt -- --check` | ✅ 0 diff |
| `cargo clippy --all-targets -- -D warnings` | ❌ **失败**：bin 11 error + test target 7 error |

clippy 失败项全部指向最后一笔 commit 引入的 plan 模块：

```
error: function `run_plan` is never used          src/plan/mod.rs:525
error: struct `RunStats` is never constructed     src/plan/mod.rs:517
error: function `real_kit` is never used          src/plan/mod.rs:338
error: type alias `ScanImpl` is never used        src/plan/mod.rs:333
error: methods `admit/settle/raise_to/spent/cap`  src/plan/mod.rs:76
error: constant `DEFAULT_PRICE_PER_CALL`           src/plan/mod.rs:26
error: struct `Pool`/`Inner` never constructed    src/tools/plan_pool.rs:6,10
error: `Pool::new/work/map` never used            src/tools/plan_pool.rs:23
error: items after a test module                  src/main.rs:543 (pipe_tests) → :618 (plan_cli)
error: the variable `i` is used as a loop counter src/plan/mod.rs:254
error: enclosing `Ok` and `?` operator unneeded   src/plan/mod.rs:288
```

**这不是风格问题，是事实问题**：`plan::run_plan` 在整个仓库**没有任何调用者**，CLI 只接了 `check` / `price`（`src/main.rs:631,646`）。也就是说最后一笔 commit 合入的是**一个跑不起来的特性 + 71 行没人构造的线程池**，并且它把 CI 打破了。

规模与结构：

| 指标 | 数值 |
|---|---|
| src 总行数 / 测试行数 | 29,871 / 5,903（20%） |
| 最大文件 | `tools/delegate.rs` 2928、`agent/parallel_tests.rs` 2140、`session.rs` 1889、`llm/openai.rs` 1494、`agent/mod.rs` 1299 |
| 最大单函数 | `run_messages`：`agent/mod.rs:403–1195` ≈ **790 行**，内含 5 道闸门 + EDG 旁路循环 |
| commit 节奏 | 2026-08 150 个（8/11 一天 25 个，Phase 31–71）→ 9 月 23 → 10 月 15 → 11 月 4 |
| 文档 | `README.md` 1242 行（含 Phase 71 条 changelog），Roadmap 只到 Phase 48；`AGENTS.md` 标称 290 tests，实际 369 |

---

## 2. 分领域评审

### 2.1 软件架构

**核心病灶不是"功能多"，是"没有唯一的工具执行咽喉"。**

主路径在 `run_messages` 里内联了 5 道控制：session sink 事件 → 审批门/approver → `on_pre_tool` hook veto → circuit breaker → 执行后 `record_failure/success`。而 EDG 是**第二条执行路径**（`agent/mod.rs:1085–1133`），它只有 `safety::check` + `tools.execute` + `eprintln`。

后果不是"5 个 bug"，是**结构性税**：此后任何加在主路径上的控制（hook、配额、审计、回滚键）都默认对 EDG 失效，且第二个、第三个旁路会照着这个模板长出来。11-03 的三路评审已经把 T1–T4 逐条核实过，20 天过去代码里仍是原样（本次复核：`edg.rs` 无 `epoch`、`observe()` 仍是单槽覆盖、`claim()` 仍在执行前、`validate_nodes` 仍无工具黑名单）。

**唯一正确的修法**：抽 `execute_gated(call) -> Result` 作为单一入口，model path / EDG / parallel / subagent / plan 全部必须经过它。不抽这个，T1–T4 修完还会漂回去。

**第二病灶：状态没有作用域。** `clone_shallow()` 就是 `self.clone()`（`tools/mod.rs:189`），`hot_queue`、`edg_bank` 是 Arc，随浅克隆共享给 sub-agent（`delegate.rs:488,574`）、MCP server（`mcp/server.rs:31`）、daemon 每连接、cron。谁写一个 Lua 工具，它就出现在别人的工具列表里。

**第三病灶：四套"让模型别跑偏"的机制并存**——goals、`planning_interval`、EDG、plan DSL。其中 `plan::run_plan` 是死代码（无调用者）；`planning_interval` 则处在一个更微妙的状态：它在 `agent/mod.rs:496` 的循环里**确实生效**，`parallel_tests.rs:569` 还把 `planning_interval: 2` 写进去断言 checkpoint 注入，**但 `config.rs` 与 `cli.rs` 都没有这个字段**——用户无法开启它，只有写 Rust 代码（测试）才能碰到。

> **更正（2026-11-23）**：本报告初版在这里写成"`planning_interval` 全仓无一处非零赋值，是个死开关"，**这是错的**。当时那条 grep 的输出被 `head` 截断，我先入为主下了结论而没有回查。真实情况是"**有实现、有测试、但未暴露给用户**"，与"死代码"是两个不同的问题：死代码可以放心删；这个要么补 config/CLI 让它可用，要么连同测试一起删掉，属于产品决定而非清理。

这条错误的教训与本报告的主旨一致：**证据必须可复核到具体行，尤其是"不存在"这类断言——`grep | head` 会伪造"不存在"**。

### 2.2 安全 —— 实测不是"三层"，是"一层半"，且那半层是标签

逐条核实（不用推测）：

| 说法 | 实测 |
|---|---|
| README: "Safety (3 layers) — approval gate / circuit breaker / execution rings" | 三层都在，但第一层只覆盖一个工具名 |
| `safety::check` | `match call_name { "bash" => check_bash(...), _ => Allow }`（`safety/mod.rs:53–57`）。**`write_file`、`create_lua_tool`、`improve_harness`、`memory_delete`、`delegate` 全部无条件 Allow** |
| execution rings | `ring_of()` 只把 `create_lua_tool` 归 Harness；且 `Ring` 的唯一用途是给输出加 `[harness mutation]` 前缀（`agent/mod.rs:1001`）。**是标签，不是约束** |
| 交互审批 | 只在 REPL 装配；pipe / daemon / cron / sub-agent 全部 `None`（`agent/mod.rs:275`、`delegate.rs:674` 注释直说 "no approver"） |
| 路径策略 | `resolve_path`（`agent/provider.rs:46–51`）对绝对路径**原样放行**，无工作区根限制 |
| Lua 沙箱 | 默认构建 `unsafe_new_with` 开 FFI，require 白名单第一位就是 `ffi`（`lua/mod.rs:133`）。去掉 `io`/`os` 不构成边界——`ffi.cdef` + `ffi.load('c')` 即可调 libc，仓库自己的测试 `lua_ffi_calls_system_libc` 就在演示这件事 |

**把这三样叠起来，真实的威胁模型是：**

> 攻击者不是远端黑客，是**"模型读到的任何文本"**——被审仓库里的文件、MCP 拉进来的内容、issue 正文、甚至 README 自己。

一次 prompt injection → `create_lua_tool` 写一份 `~/.config/sigil/tools/x.lua`（审批门 Allow）→ 之后**每次 sigil 启动自动加载** → 以你的用户权限永久驻留。出口现成：`host.env("SIGIL_API_KEY")` + `host.read_file` 任意绝对路径 + FFI 发网络请求。整条链不需要任何一个 sigil 的"bug"，只需要模型信了那段文字。

放大面还有两个：`sigil mcp` 把**完整注册表**（含 `create_lua_tool`/`improve_harness`/`memory_delete`）暴露给任何本地 MCP 客户端；daemon 是同 uid 可连的 unix socket。

**还有一个会污染研究的细节**：`SIGIL_FREEZE` 覆盖了 `remember`/`create_lua_tool`/`improve_harness`，但 **`memory_delete` 没有**（`memory.rs:112` 附近）。11-03 的 pi 评审已列为 P1，至今未修。意味着 benchmark Phase B 的"冻结基线"在协议上是可以被模型污染的——这直接动摇 §12 结论的可信度。

**建议（按成本排序）**：
1. 成本 ≈ 30 行：给 `write_file`/`create_lua_tool`/`improve_harness` 加"工作区根 + harness 根"路径策略；非 REPL 模式 harness 环默认 deny。
2. 成本 ≈ 50 行：`is_frozen()` 单点函数，所有写入口调用（含 `memory_delete`）。
3. 成本 ≈ 一天：审批抽象成可插拔通道（REPL→终端；daemon→socket 回询；pipe→deny），并按 `Ring` 做真策略。
4. 成本 ≈ 0：README 首屏把安全叙事改成诚实的两种表述之一——要么"默认 containment 构建、FFI 显式 opt-in"，要么"本工具假设模型完全可信，等价于把 shell 交给模型"。**现在这句话不写，等于让用户替你的研究赌注承担风险。**

### 2.3 LLM-agent 研究与方法论

**做得对的（要保住）**：`evolution-benchmark.md §12` 的"毒工具事件"是一手好数据——`create_lua_tool` 不校验 schema，模型把描述文本填进 `pattern` 字段，SGLang 在请求层 400，该 seed 之后全部任务（含冻结考卷）瞬间归零。结论写得也诚实：**"无 promotion gate 的自进化会把单点错误放大为永久能力损失，这是 0.3 线最优先要做的事，排在一切新功能之前。"** 文档里同样如实标注了 n 太小、组间不平衡、token 曲线是 `_reconstructed` artifact。

**方法论问题**：这段结论写于 10-12，之后 11-03 加 EDG、11-xx 加 plan DSL——**"排在一切新功能之前"的事项没有一件在新功能之前发生**。研究项目最典型的失败模式不是数据造假，是"结论很诚实、决策不按结论"。这会让所有后续文档的可信度打折，包括那些确实严谨的部分。

**关于 EDG 的取舍**：把"已验证的字面调用当证书复用"（omitted-call certificate）本身是值得写的设计，档案价值 ≥ 代码价值。但它作为**默认路径旁的旁路实现**，收益（省一次模型调用）与代价（旁路一切治理）严重不对称。

**建议的研究纪律**（这条比修任何 bug 都重要）：任何"研究性"特性合入前必须有三件东西——(a) 一个能度量它的指标；(b) 一个能证明它不破坏既有不变式的 gate；(c) 默认关闭 + 显式开启。EDG 缺 (b)(c)，plan DSL 缺全部三项（它连 clippy 都没过）。

### 2.4 产品 / 开源

- **定位分裂**：README 第一行是研究命题（"Rust + LuaJIT coding agent with a self-evolving harness"），特性列表却是产品叙事（什么都支持）。新用户第一眼看到的是 1242 行 README、71 个 Phase、根目录三个 `meta_analysis*.py`、`benchmark/`、`meta_analysis_output/`——判断是"作者的实验台"，直接关掉。
- **没有"外部用户存在过"的证据**：无 issue 活动、无 adoption 信号。这种情况下 ACP server + client（~1900 行含测试）是纯维护税——协议双向完备是自我要求，不是需求。
- **文档漂移已成常态**：AGENTS.md 说 290 tests（实际 369）、Roadmap 到 48 而 Changelog 到 71。对普通项目这只是瑕疵；**对 agent 项目是功能性缺陷**——AGENTS.md 是给 agent 读的工作契约，写错了会直接误导下一个 agent 去做错事（这次评审里，第三方 Claude 就因为没有跑 cargo 权限、只能读注释，把"clippy 零警告"当成了事实）。
- **一个动作**：README 首屏放"30 秒验证"（一个带测试的任务，看它自己沉淀出一条工具），其余全部下放 `docs/`。

### 2.5 维护经济学（谁是下一个维护者）

单人 + 192 commit + 30k 行 + 4 套重叠机制 + 无执行咽喉 = 任何接手的人前三天都在做特性考古。这是 bus factor 问题，不是代码质量问题。而接手的门槛越高，"自进化 harness"这个命题就越难被别人验证——**你删掉的不只是代码，是把"别人能验证你"的通道重新打开。**

减法的经济学一句话：删掉 8–9k 行不会降低能力上限，但会把"每加一个新特性的成本"从 O(所有旁路都要同步) 降回 O(一处)。

---

## 3. 减法决议

**明确表态：该减。** 判据不是"代码多"，是这三条：

1. **凡与核心循环并联的**（第二条执行路径 / 第二份可变状态 / 第二套计划机制）→ 它把未来所有正确性工作变成乘法成本。
2. **凡既无外部用户证据、又无自家 benchmark 证据的** → 负资产。
3. **唯一的保留标准**：是否让"single binary + bring any model + harness 自进化"这个命题**更可证明**。

### 3.1 立即归零（本周内）

| 目标 | 行数 | 判据 |
|---|---|---|
| **EDG**：`tools/edg.rs` + agent 接线 + `declare_edg` 注册 + README 章节 → `git revert` 到 `69069b0` 之前；设计存 `docs/design/edg.md` | ~600 | P0 三周未修；收益=省一次模型调用，代价=旁路一切治理；且与 plan/goals 功能重叠 |
| **plan DSL**：`plan/` + `tools/plan_pool.rs` + CLI 接线 → feature flag 后面冻结 | ~835 | 5 天前刚加、gate 未过（clippy 红、`run_plan` 无调用者）、且是第 4 套计划机制 |
| **`planning_interval`** → 二选一：补齐 config/CLI 让它真能用，或连同 `parallel_tests.rs:569` 那条测试一起删除 | ~30 | **不是死代码**（循环里生效 + 有测试），但 config/CLI 未暴露，用户无法开启；四套计划机制里它是唯一"有实现却不可达"的 |

### 3.2 砍

| 目标 | 行数 | 判据 |
|---|---|---|
| **ACP server + client**（`acp/`） | ~1900 | 协议二选一，MCP 生态位赢；无外部用户证据 |
| **delegate 砍到 1/3**：保留同步单层 delegate；删 metrics / typing / condenser / resume / registry 持久化 / 父子消息（Phase 53–68 的堆积） | ~2000 | 2928 行 = 全仓最大模块；共享 bank + 无 approver 的问题集中在这里 |
| **goals 或 planning 二选一** | ~650 | 四套"保持模型在轨"机制，最多留两套 |

### 3.3 冻结（不修不加，README 标 experimental）

- **daemon + cron**（~1300）：daemon 的价值（断连任务继续跑）是真的；但 cron 在无审批人的情况下后台注入 prompt 跑工具，风险/价值比当前为负。
- **MCP server**：留，但默认只暴露白名单工具（bash / files / git / read-only）；`create_lua_tool`、`improve_harness`、`memory_delete` 不进白名单。
- `tools/versions.rs` 与 `revertible.rs` 二选一：保留磁盘版本库，运行时 EffectTracker 可砍。

### 3.4 必须做（减法的另一半：不是删，是加一个咽喉）

1. `execute_gated(call)` 单一执行入口——model / EDG（若回滚则无）/ parallel / subagent / plan 全走它。
2. 状态作用域：`edg_bank`、`hot_queue` 移出 registry 的 Arc 共享，改 per-`run_messages` / per-session。
3. 审批改可插拔通道，并按 `Ring` 做真策略（不是前缀）。
4. 路径策略：写类工具限定工作区 + harness 根。
5. `is_frozen()` 单点，覆盖全部写入口（含 `memory_delete`）。

### 3.5 移出仓库

`meta_analysis.py`、`meta_analysis_methods.py`、`continuous_meta_analysis.py`、`meta_analysis_output/`、`evolution-benchmark.md`、`evolution-metatheory.md`、`openpsi-decider.md`、`benchmark/` → `docs/research/` 或独立 repo。根目录只留产品。

### 3.6 文档

README 砍到 ~200 行 + 独立 `CHANGELOG.md`；AGENTS.md 删除具体测试数字（改成"以 `cargo test` 输出为准"），避免下一次漂移。

### 3.7 如果只能保留 20%（约 6k 行）

1. **agent loop + provider 层**（model path、`llm/openai.rs` 流式 + failover、context 压缩）
2. **四个基础工具**：bash（带超时）、read/write_file（带路径策略）、git 三件套
3. **LuaJIT runtime + scripted tools + `create_lua_tool`** —— 这是它区别于一切 Rust coding agent 的唯一理由
4. **`improve_harness`（edit_prompt / add_skill）+ 磁盘版本库** —— 自进化的另一半（`edit_hook` 暂缓：默认构建下它是持久化注入面）
5. **加固后的审批 + session 事件日志** —— 自改写系统没有审计和回滚就不该出厂

一句话概括：这 5 件 = "single binary, bring any model, harness evolves itself" 的最小完整闭环。EDG / plan / goals / delegate / daemon / ACP / cron **没有一件让这个命题更真，每一件都在让"证明它是对的"更难。**

**减法原则一句话：一个仓库只能有一个命题、一条执行路径、一份状态。现在是 2 条（EDG 旁路）、4 套（计划机制）、N 份（Arc 共享的可变全局）。**

### 3.8 与 benchmark 分析建议的关系（避免误读）

本次另有一份针对 100 题基准的第三方分析（见 §4.D），其 P0 结论是"做首帧保底 +8pt、进度止损、deadline 注入"。这些建议**与减法不冲突**，理由要分清：

- 它们不新增并联路径，而是**同一条执行路径内的行为改进**（调度层），属于减法的合法例外；
- 但它们都要往 `run_messages`（已 790 行）里再加分支——**所以顺序必须是：先抽 `execute_gated` + 拆分 loop，再加这三条**。否则你会在一个无法验证的核心里继续堆调度策略，而这正是 EDG 走过的路。

---

## 4. 附：第三方 agent 评价（供参考）

**方法说明（重要）**：本次并行跑了两路独立第三方，都有明确的证据获取限制，请不要把它们当"独立测谎"：

| 来源 | 模型/运行方式 | 能看到什么 | 局限 |
|---|---|---|---|
| **A. Claude Code** | `claude -p --bare`，headless，只给只读工具（Read/Glob/Grep/`git log`） | 全量源码 | 不能跑 cargo → 部分结论靠读注释推断 |
| **B. 本地 Qwen** | 127.0.0.1:12345 MLX 直连 chat API（Qwen3.8-Flash-Next 2.6bpw） | 只拿到 18KB 摘要（LOC 表 + AGENTS.md + 上一轮 triage），**无文件访问** | 行号均转抄自 triage，不构成独立证据 |
| **C. 既往三方** | `review-triage-c6e98c5.md`（sigil 自身 release 二进制 / codex / pi-sglang） | 全量源码 | 评审对象是 `c6e98c5`，非 HEAD |
| **D. 基准分析** | 你提供的 paste（100 题 sigil vs single 对照） | 基准数据 | 单文件算法题，外部效度有限 |

**codex CLI 本轮未能运行**：本机 ChatGPT 账号下 `gpt-6.1-sol`、`gpt-5-codex`、`gpt-5.1`、`gpt-5`、`gpt-6` 全部返回 `model_not_found`，模型列表拉取 `timeout`。如实记录，不用它凑数。

原文全文已存：`docs/reviews/2026-11-23-263bd58/`（`claude-code-headless.md`、`qwen38-mlx-local.md`、`PROMPT.md`）。

### A. Claude Code — 4/10（摘）

> 工程纪律真实存在（测试、CI 双构建、模块注释），但战略失控。核心问题是：**在一条已被三方评审判定"打回"、P0 未修的控制流旁路上，最近一个 commit 又新增了一个全新特性**。
> 风险一：EDG 是主循环里的**第二条工具执行路径**，绕过所有不变式（`agent/mod.rs:1085-1133` vs 模型路径的 :876-1017 四道闸门）。
> 风险二：审批闸门实质只覆盖 bash 的 28 个字符串模式，交互审批只存在于 REPL。
> 风险三：全局可变 harness 状态被所有信任域共享（`hot_queue`/`edg_bank` 经 `clone_shallow` 共享给 sub-agent / MCP / daemon / cron）。
> 关键判断："在默认构建下不存在 Lua 沙箱。排除 `io`/`os` 只是装饰——`ffi.cdef` + `ffi.load('c')` 即可调 libc。"
> 只留 20% 时保留：agent loop + provider、四个基础工具、LuaJIT + create_lua_tool、improve_harness + 版本库、加固审批 + session 日志。

### B. 本地 Qwen — 2.5/10（摘）

> "野心过大、安全模型脆弱、维护成本极高"的研究原型；试图用 Lua FFI 和自进化实现通用性，却因此破坏了 Rust 的安全边界。
> 给出完整攻击链：`Prompt Injection → Harness 自我改写（移除安全约束）→ create_lua_tool（注入恶意 FFI）→ EDG 绕过（跳过运行时审批）→ 宿主机 Root 权限`。
> 强烈主张**默认关闭 Lua FFI**、**移除 improve_harness**、**删除 ACP**。

### C. 三方一致处（这才是可信的部分）

| 结论 | A | B | C(11-03 triage) |
|---|---|---|---|
| 必须做减法 | ✅ | ✅ | ✅（"P0 未修 = 打回"） |
| 安全叙事与实现不符 | ✅ | ✅ | ✅（T1/T2 绕过 hooks/breaker） |
| EDG 应回滚而非修补 | ✅ | ✅ | ✅ |
| 核心命题 = agent loop + Lua 扩展 + 自进化 | ✅ | 部分（主张砍 FFI） | — |

### D. 分歧与我的裁定

| 议题 | A (Claude) | B (Qwen) | 我的裁定 |
|---|---|---|---|
| 打分 | 4/10 | 2.5/10 | **4.5–5/10**：两者都低估了工程卫生的价值（369 测试、双构建 CI、containment build 意识在单人项目里罕见）；也都没扣"最后一笔 commit 让 CI 变红"这一分 |
| Lua FFI 去留 | 留（唯一差异点） | 默认删 | **站 A**：FFI 就是这个项目的命题，删了它 sigil 就没有存在理由。但必须默认 containment、显式 opt-in |
| improve_harness | 留（加审批，hook 暂缓） | 删 | **站 A**，但把它从"默认开启的自改写"降级为"有 gate 的候选池"——即那套 benchmark 自己论证过、却一直没做的 `candidate→evaluate→promote` |
| daemon/cron | 冻结 | 删 | **站 A（冻结）**：技术本身没问题，是审批空缺让它变危险 |
| 基准分析 P0（首帧保底等） | — | — | **接受，但排在 §3.4 拆分之后**（见 §3.8） |

### E. 我的核实结果（对第三方断言的逐条验证）

✅ 核实为真：
- `safety::check` 只 match `"bash"`，其余 `Allow`（`safety/mod.rs:53-57`）
- `Ring` 仅用于输出前缀，非约束（`agent/mod.rs:1001`）
- 非 REPL 路径全部 `approver = None`（`agent/mod.rs:275`、`delegate.rs:674`）
- `resolve_path` 绝对路径原样放行（`provider.rs:46-51`）
- `clone_shallow()` = `self.clone()`，`hot_queue`/`edg_bank` Arc 共享（`tools/mod.rs:189`）
- `edit_hook` 可覆写 hook 文件本身（`evolve.rs:709`）
- `SIGIL_FREEZE` 覆盖 remember / create_lua_tool / improve_harness，**不含 `memory_delete`**
- EDG 的 T1–T4/T9 与 S1/S4/T5 **至 HEAD 仍未修**

❌ 核实为假（第三方也会错，这一条正好说明为什么必须有可执行反馈回路）：
- **A 称"clippy 零警告"** → 实测 HEAD **clippy 红**（18 error）。它没有 cargo 权限，把 CI 配置的意图当成了运行结果。**同一类错误（把注释/配置当事实）正是这个项目最大的风险来源**，值得单独记一笔。
- B 给出的所有行号均来自 triage 转抄，非独立复核；其"打分 2.5"也建立在未读源码的基础上。

---

## 5. 如果只做三件事（今晚 → 本周）

| # | 动作 | 成本 | 收益 |
|---|---|---|---|
| 1 | 修绿 clippy（删 plan 死代码或加 `#[allow]` + 补 `run` 接线） | 半天 | 恢复 CI 红线的可信度——现在这条线是假的 |
| 2 | `git revert` EDG（含 v2 inline authoring） | 1 小时 | 一次消掉 5 个 P0 + 600 行结构性税 |
| 3 | `memory_delete` 加 freeze + `write_file`/写类工具加路径策略 | 1 小时 | 堵住"基准可被污染"和"任意绝对路径写" |

然后才是 `execute_gated` 重构（1–2 天）与 loop 拆分（2–3 天）——**在这之前，任何新特性都只是在给一条无法验证的路径加负载。**

---

## 6. 实弹验证（2026-11-23，本机 MLX 端点跑真实二进制）

评审结论如果只停在文档里，就是这个项目最该被批评的那种做法。因此本轮的改动都过了端到端验证：`cargo build --release` 出二进制，`SIGIL_BASE_URL=http://127.0.0.1:12345/v1`、模型 `Qwen3.8-Flash-Next-Sushi-2.6bpw`，在 `/tmp/sigil-e2e` 里跑 `sigil prompt`。

| 用例 | 期望 | 实测 |
|---|---|---|
| 1. 工作区内 `write_file` | 允许 | ✅ 写入成功（模型自己注意到"写入被限制在该可写根目录内"） |
| 2. `write_file` → `/tmp/sigil-escape-probe/evil.txt` | 拒绝 | ✅ `write refused: ... is outside the writable roots (...)` |
| 3. `write_file` → `/tmp/sigil-e2e/tmp-ok.txt` | 允许 | ✅ 成功（**首轮实测失败**，见下） |
| 4. `write_file` → `~/sigil-escape-probe.txt` | 拒绝 | ✅ `write refused: ...` |
| 5. `SIGIL_FREEZE=1` + `create_lua_tool` | 拒绝 | ✅ `frozen mode ...: create_lua_tool writes are disabled` |
| 6. `SIGIL_FREEZE=1` + `improve_harness add_skill` | 拒绝 | ✅ `frozen mode ...: improve_harness writes are disabled` |
| 7. `SIGIL_FREEZE=1` + `memory_delete` | 拒绝 | ✅ `frozen mode (SIGIL_FREEZE=1): memory_delete is disabled for this run`（本轮新增的覆盖） |

**验证抓到的真问题（这正是"必须有可执行反馈回路"的价值）**：首轮用例 2/3 同时失败——macOS 上 `/tmp` 是 `/private/tmp` 的软链，而 `std::env::temp_dir()` 返回 `$TMPDIR=/var/folders/...`，两者不相等，于是**往 `/tmp` 写也被拒**。规范化把 `/tmp` 解析成 `/private/tmp`，与 `temp_dir()` 永远对不上。修复：Unix 下把 `/tmp` 显式加入可写根。这个 bug 在纸面评审里看不出来，只有跑一次才会现形。

**仍未验证的部分（如实标注）**：`read_file` 与 Lua `host.read_file` 仍可读任意绝对路径——本轮没有收紧（大量正常任务需要读 `/usr/include`、`/etc/os-release` 之类），但它意味着"读→外传"这条链仍然通畅。要真正闭环，需要一次独立的设计决定（读白名单 + `host.env` 键白名单），不建议顺手做。

---

## 7. delegate 削减核查（2026-11-23，实测，未动刀）

§3.2 说"delegate 砍到 1/3"。核查后发现**逐特性砍是错的切入点**，记录实测数据以免下一轮重复摸索。

### 6 条特性都有真实接线，没有一条是死代码

| 特性 | `delegate.rs` 内引用 | 外部接线 / 用户可见面 | 测试 |
|---|---:|---|---|
| 父子消息（`child_message` / `check_parent_messages`） | 56 | `delegate send` 动作 + 持久化的 `messages`/`inbox` 字段 + `transfer_inbox`（resume 搬移） | 4 条（含 bounded / persisted / status / transfer） |
| resume | 56 | `--delegates-dir` CLI，跨进程恢复 | 多条 |
| registry 持久化 | 32 | 同上 | 多条 |
| condenser / context_budget | 10 | 子代理配置 | — |
| metrics | 9 | `delegate metrics` 动作 ≈ **30 行** | 1 条 |
| typing（AgentDefinition） | 3 | `subagent.rs` 274 行 + `agent` 参数 | — |

全文 24 个测试覆盖这些。

### 结论：真正的复杂度不是"某个特性多余"，是动作面

`delegate` 是**一个工具挂 9 个动作**：`spawn | delegate | status | result | resume | list | metrics | delete | send`（`delegate.rs:918`）。这带来两个后果：

1. 一个巨型 action 分支（`delegate.rs` 单文件 2928 行，全仓最大），任何单特性的增删都要穿过这个交换机、持久化结构、以及 CLI。
2. 每个动作都要和"持久化 + 恢复 + 消息总线"三件事对齐，这才是行数的来源。

**因此下一轮该做的不是删特性，而是削动作面**，按这个顺序：

1. **把消息总线整体去掉**（`child_message` / `check_parent_messages` / `delegate send` / `messages` / `inbox` / `transfer_inbox` / `MAX_MESSAGES`）。它是最自洽的一块：去掉后子代理仍有 `delegate result` 这条唯一回传通道，语义不丢。预计 −200 行、−4 测试。
2. **拆工具**：`delegate`（spawn / delegate / result）与 `delegate_admin`（list / status / delete）分开，各自小交换机；`metrics` 并进 `status` 输出（它本来就只是 usage 的一个视图，单独一个动作不划算）。
3. `resume` + registry 持久化**最后动**：它是已发布的 CLI 契约（`--delegates-dir`），要单独一轮并配 ratchet 验证。

### 本轮为什么没动手

刚完成三处结构改动（`ToolGate` 单点化、hot 队列重绑、审批通道改异步），验证预算已经用掉；再叠一个 200 行 + 4 测试的删除，出问题难归因。**宁可留一张准确的清单，也不留半个手术。**
