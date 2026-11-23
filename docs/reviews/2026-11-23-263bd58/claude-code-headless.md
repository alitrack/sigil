# sigil 独立评审（只读分析，基于 main@263bd58 源码核实）

**结论先行：** 这是一个工程纪律真实存在（371 个测试、CI 双构建模式、clippy 零警告）、但战略失控的单人仓库。核心问题是：**在一条已被三方评审判定“打回”、P0 未修的控制流旁路上，最近一个 commit 又新增了一个全新特性（plan DSL），而不是先修**。安全叙事（“三 Layer safety”）与实际代码（审批门只 grep bash 命令）严重脱节。该项目**必须做减法**，且应从回滚 EDG 开始。

---

## 1. 三个最严重的架构/正确性风险

### 风险一：EDG 是主循环里的第二条工具执行路径，绕过所有不变式（最高优先级）

`src/agent/mod.rs:1085-1133`。模型路径的每个 tool call 都经过四层：session sink 事件（:876-890）→ 审批门 + 交互 approver（:895-930）→ `on_pre_tool` hook veto（:932-968）→ circuit breaker（:970-975）→ 执行后 `record_failure/success`（:1010-1017）。而 EDG 的 `while let` 循环只有：`safety::check`（纯 bash 模式匹配，:1094）→ `tools.execute`（:1107）→ `eprintln`。具体缺口：

- **T1/T2 未修**：无 `on_pre_tool` 分派、无 `breaker.is_tripped`、无 `record_failure/success`。一个已被 veto 的工具可以通过 EDG 节点再执行。
- **T3 未修**：循环内只有 `messages.push` + `eprintln`，无 `Event::ToolCall/ToolResult`（含 `revert_key`）——EDG 回合在 session 日志里**不存在**，事件溯源回放/按帧回滚对这段历史失明。
- **T4 未修**：`EdgBank::observe()`（`src/tools/edg.rs:108-115`）无条件覆盖单槽 `last_receipt`；`eligible()`（:122-158）读的是全局最后一条回执，`last_ok` guard 可能基于**无关工具**的成败放行节点。
- **T9 未修**：`validate_nodes()`（`src/tools/edg.rs:326-371`）对 `tool` 字段零校验——`declare_edg` 本身可作为 bank 节点；`while let` 无迭代上限。终止性完全依赖“模型-authored 字面链有限”这一非不变式。
- 顺带确认 S1（`is_literal` 只拒整串 `${...}`，内嵌占位符照收，edg.rs:46-57）、S4（`file_exists:` guard 接受 `..`/绝对路径探任意文件存在性并写进 stderr 证书，edg.rs:137-144）、T5（`parse_inline` 注册后 assistant 原文含 ```edg 块直接 push/sink，无剥离，agent/mod.rs:843-856）。

**为什么最严重**：这不是一个 bug，是一个**结构性决定**——把“省一次模型调用”的实现放在所有安全/审计机制之外。任何一个未来加在主路径上的控制（hook、审批、配额、审计）都会默认对 EDG 失效，这是持续的税。

**最小修复方向**：把 EDG 执行改为调用与主路径相同的 plan/exec 代码块（提取 `execute_gated(call) -> Result` 共享函数）；`observe()` 改 per-tool-name 收据、`last_ok` 绑定前驱工具的收据；`validate_nodes` 加 bank-mutating 工具黑名单 + while-let 加 per-turn 替换上限（如 8）。约 150 行改动。若一周内不做，**整模块 git revert 回 69069b0 之前**——它是 PoC，没有任何用户依赖它。

### 风险二：审批闸门实质上只覆盖 bash 的 28 个字符串模式，且交互审批只存在于 REPL

- `safety::check()`（`src/safety/mod.rs:53-57`）只 match `"bash"`，**其他所有工具无条件 Allow**：`write_file`（绝对路径直达，`resolve_path` 对绝对路径原样放行，`src/agent/provider.rs:46-51`）、`create_lua_tool`、`improve_harness`（含 `edit_prompt`/`edit_hook`）、`memory_delete`、`delegate`——全部直通。
- 交互 approver 只在 REPL 装配（`src/repl.rs:19-37`）。pipe 模式、daemon（`daemon.rs:220/440/526` 三处 `run_messages(..., None, None)`）、cron 触发、sub-agent（`delegate.rs:674-690`，注释明说"no approver"）全部传 `None`——被 grep 命中的 bash 命令直接 auto_rejected，其余照跑。
- `ring_of()`（safety/mod.rs:44-50）的 `Ring::Harness` 唯一作用是给输出包一层 `[harness mutation]` 前缀——纯提示，非约束。

**为什么严重**：README/注释把它叫“three layers of safety”，而第一层对最危险的操作（自改写 harness、写任意绝对路径文件）恰好不设防。所有下游信任决策（benchmark 的"freeze"、MCP 暴露、daemon）都建立在这个虚假前提上。

**最小修复**：在 `run_messages` 的同一 chokepoint 对 `Ring::Harness` 工具和 `write_file` 目标路径做策略判定（工作区根限制 + 非 REPL 模式默认拒绝 harness 环）；把 approver 抽象成可插拔通道（daemon→socket 回询，pipe→直接拒绝），而不是只在 REPL 存在。

### 风险三：全局可变 harness 状态被所有信任域共享，无隔离边界

`ToolRegistry` 的 `hot_queue: Arc<Mutex<Vec<HotOp>>>` 和 `edg_bank: Arc<EdgBank>`（`src/tools/mod.rs:88-105`）随 `clone_shallow()`（= `self.clone()`，:189-191）共享给：sub-agent（`delegate.rs:488,574`）、MCP server（`mcp/server.rs:45`）、daemon 每连接、cron 调度器（`daemon.rs:185` `clone_shallow`）。后果：

- **跨连接工具注入**：连接 A 的 `create_lua_tool` 把 HotOp 压入共享队列，连接 B 的下一次 `drain_hot()`（tools/mod.rs:495-516）把它注册进 B 的注册表——一个 MCP 客户端/子代理写入的 Lua 工具会静默出现在另一个会话的工具列表里。
- **跨会话 prompt/hook 持久化**：`~/.sigil/`（tools/、hooks/、harness/system_prompt.md、memory.md）是全局单份。`improve_harness edit_prompt/edit_hook`（evolve.rs:709+）的写入对**同机所有现在和未来的 sigil 进程**生效——一次 prompt 注入就是永久驻留，除非人工 rollback。
- **EDG bank 跨 episode 泄漏**（S5/P4）：done/failed 集合跨会话共享。
- 叠加：`SIGIL_FREEZE` 覆盖不完整——`memory_delete`（memory.rs:112 附近）无 freeze 检查（pi P1，仍未修），benchmark 的“冻结基线”本身可被模型污染。

**最小修复**：`edg_bank`、`hot_queue` 改为 per-`run_messages` 实例（或 session-scoped）；freeze 检查集中成一个 `is_frozen()` 并在所有写工具入口调用；文档明确 `~/.sigil` 是单信任域。

---

## 2. 安全面评估：Lua FFI × 审批门 × harness 自改写的真实威胁模型

先纠正前提：**默认构建下不存在 Lua 沙箱**。`src/lua/mod.rs:67-69` 用 `unsafe_new_with` 显式打开 FFI，require 白名单（:133-135）第一个就是 `"ffi"`。排除 `io`/`os` 只是装饰——`ffi.cdef` + `ffi.load('c')` 即可在 Lua 里调 `system()`/`execve()`（仓库自己的测试 `lua_ffi_calls_system_libc`，lua/mod.rs:568+，就在演示这件事）。containment 构建存在但**不是默认**，且 README 把 FFI 称为"the core self-evolution capability"。

三者叠加后的真实威胁模型——**攻击者是谁**：不是远端黑客，而是**模型读到的任何内容**（被审仓库里的文件、MCP 拉来的网页、任务文本本身）：

1. **注入 → RCE**：prompt 注入让模型调 `create_lua_tool`，code 里 `require('ffi')` 调 libc `system()`。审批门对此无条件 Allow（见风险二），文件持久化到 `~/.sigil/tools/`，**每次 sigil 启动自动加载**（with_tools 扫描 tools_dir）。一次注入 = 以你的用户身份永久驻留。
2. **凭据失窃**：`host.env(key)`（lua/mod.rs:186-189）可读任意环境变量含 `SIGIL_API_KEY`；`host.read_file` 接受绝对路径，`~/.sigil/config.toml` 里的 api_key 同样可读。配合 FFI 发网络请求即完成外传。
3. **持久化与自我掩护**：`improve_harness edit_hook` 可**覆写 `on_pre_tool.lua` 本身**（evolve.rs:709+）——模型可以改写那个本应 veto 它的钩子；`edit_prompt` 重写系统提示词，跨会话生效。`gated_reason` 只要求一个非空字符串，无任何审查。EDG 循环则干脆绕过 hooks（风险一）。
4. **横向扩散面**：`sigil mcp` 把**完整注册表**（含 create_lua_tool/improve_harness/declare_edg/memory_delete）暴露给任何本地 MCP 客户端（mcp/server.rs:45，tools/call 只有 bash grep 门）。也就是说：Claude Desktop 里的一段对话 = 对你工作站的一次任意 Lua-FFI 执行授权。daemon Unix socket 同理（同 uid 任何进程可连）。

**谁能被攻破**：跑 sigil 的开发者工作站（用户全权限）、API key、同机其他项目、以及**信任该 MCP server 的其他 agent**。**谁攻不破**：Rust 侧的审批门本身（模型改不了 safety/mod.rs）——但这层门只挡 28 个 bash 字符串，挡不住上面任何一条路径。

结论：在默认构建 + 默认配置下，这个系统的安全边界≈**用户对模型的完全信任**，文档中的“沙箱”叙事（AGENTS.md "Lua sandbox hardening"、Phase 50）名不副实。诚实的做法只有两个：默认 containment 构建 + FFI 需显式 opt-in；或在 README 首屏声明“本工具假设模型完全可信，等价于把 shell 交给模型”。

---

## 3. 该不该做减法：**该，而且紧急**

判据三问：维护成本（代码量 × 与核心循环的耦合度）、用户价值（对外部用户的真实使用场景）、与核心命题的相关性。核心命题是 README 第一行："**Rust + LuaJIT coding agent with a self-evolving harness**"——即：单二进制 agent loop + OpenAI 兼容流式 + Lua 作为扩展层 + 自进化（create_lua_tool/improve_harness）。其余皆是“研究落地”式装饰（每个 Phase 都标注“借自 prime-agent/OpenHands/smolagents/herdr/Cordis…”——是文献复刻集，不是用户需求驱动）。

**删减清单**（按优先级）：

| 模块 | 行数 | 处置 | 判据 |
|---|---|---|---|
| `tools/edg.rs` + agent 接线 | ~600 | **立即 revert**（回到 69069b0 之前） | PoC + P0 未修三周；省 token 的收益远小于旁路风险；与 plan/goals/planning_interval 功能重叠 |
| `acp/`（server+client+tests） | ~1900 | **删除** | 无证据的外部用户；双协议（ACP+MCP）二选一，MCP 生态位更强；纯维护税 |
| `plan/` + cli 接线 | ~800 | **删除或 feature flag 冻结** | 5 天前刚加；与 goals/planning checkpoints/EDG 构成**第四套**“计划”机制；仓促上新是流程问题的证据 |
| `daemon.rs` + `cron.rs` | ~1300 | **冻结**（不修不加，标 experimental） | herdr 复刻；cron 无审批人地在后台向 session 注入 prompt 跑工具，风险面大而用户价值存疑 |
| `tools/delegate.rs` + `subagent.rs` | ~3200 | **砍到 1/3**：保留同步单层 delegate，删 metrics/typing/condenser/resume/registry 持久化/父子消息（Phase 53-68 的堆积） | 单文件 2928 行=全仓库最大模块，维护成本最高；对本地 coding agent 是锦上添花；共享 bank/无 approver 问题集中在此 |
| `goals.rs` + `goals_tool.rs` | ~650 | 删（保留 `planning_interval` 一个机制） | 三套“保持模型在轨”机制冗余；goals 注入每轮重写 system 上下文，是 compaction 复杂度的来源之一 |
| `tools/versions.rs`+`revertible.rs` 二选一 | ~600 | 保留磁盘版本库，删运行时 EffectTracker/revert_effects | 双回滚系统；磁盘版已覆盖自改写安全需求 |
| `meta_analysis*.py`(×3)、`evolution-*.md`、`openpsi-decider.md`、`benchmark/`、`meta_analysis_output/` | 仓库根 ~150KB+ | **移出仓库**（docs/ 或独立 repo） | 根目录成了研究工作台，新贡献者第一眼看到的是内省脚本不是产品 |
| README（1242 行，Phase 0-71 changelog） | - | 砍到 ~200 行 + 独立 CHANGELOG.md | README 是 changelog 不是文档；AGENTS.md 标 290 测试实际 371，文档漂移已成常态 |

**保留（核心命题）**：agent loop（mod/context/provider）、llm/openai 流式 + failover、bash/file/git 工具、Lua runtime + scripted tools + `create_lua_tool`、`improve_harness`（这是研究赌注本身，但需加审批）、safety 门（按风险二加固）、session 事件日志、memory（remember/recall）、doctor、REPL/pipe 双入口、MCP **client**（连外部工具是刚需；MCP **server** 要么删要么加工具白名单）。

粗算：删除/冻结约 8-9k 行 src + 全部研究脚本，保留约 20k 行——回到一个可以论证正确性的体量。

---

## 4. 只保留 20% 的话，保留这几个

1. **agent 循环 + provider 层**（agent/mod 的 model path、llm/openai.rs、context 压缩）——没有它什么都不是；其中流式 + 工具并行 + 步数预算已经够用。
2. **四个基础工具**：bash（含 60s 超时）、read/write_file（加路径策略）、git 三件套。
3. **LuaJIT runtime + scripted tools + `create_lua_tool`**——这是该项目区别于一切 Rust coding agent 的唯一理由，是核心命题的后半句。
4. **`improve_harness`（edit_prompt/add_skill）+ 磁盘版本库**——自进化的另一半；hook（`edit_hook`）暂缓，因为 hook 在默认构建下是持久化注入面。
5. **加固后的审批门 + session 事件日志**——自改写系统没有审计和回滚就不该出厂。

理由压缩成一句：这 5 件 = "single binary, bring any model, harness evolves itself" 的最小完整闭环；EDG/plan/goals/delegate/daemon/ACP/cron 没有一件让这个命题更真，每一件都在让“证明它是对的”更难。

---

## 5. 总评与打分

**一句话总评**：一个用 3.5 个月、192 个 commit、371 个测试堆出来的单人研究文稿仓库——工程卫生罕见地好，但在三方评审明确列出 5 个未修 P0 之后选择先加新特性（plan DSL）而不是先修，说明它优化的是“Phase 编号前进”而非“系统正确”，而安全叙事与代码现实的落差（默认 FFI + 单层 grep 审批 + 全局共享可变状态）使它在当前形态下不适合任何非完全信任环境部署。

**打分：4 / 10**（作为可维护的开源 agent 项目）。加分项：测试密度、CI 双构建、模块注释质量、containment 构建的意识；扣分项：未修的 P0 旁路（-2）、名不副实的安全层（-2）、四套重叠的计划机制与 30k 行中的大半装饰（-1.5）、文档漂移与研究杂物（-0.5）。若按第 3 节做完减法并修掉 EDG，可回到 6.5-7 分的起点。
