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

> 状态：**Phase 69** — 嵌套深度上限（prime-agent `RLM_MAX_DEPTH`，默认 1）：
> 每个 `DelegateTool` 实例带 `depth`（0 = 顶层）；子代理的工具注册表拿到的是
> depth+1 副本（替换共享父实例），因此被委托的子代理的 spawn/delegate/resume
> 被拒（「nesting depth limit (1) reached」）。E2E：子代理两次尝试委托均被拒，
> 并通过 `delegate list` 核验无子子代理产生。
>
> 状态：**Phase 68** — per-agent condenser（OpenHands `definition.condenser`）：
> `spawn` 与 `delegate` 批量的每个 task 接受 `max_context_messages` /
> `max_context_tokens` 覆盖——该子代理的上下文预算替换父代理全局设置（resume
> 也携带）。E2E：以 `max_context_messages: 4` / `max_context_tokens: 3000`
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

任意 OpenAI 兼容端点都可通过 `SIGIL_BASE_URL` 使用——vLLM、Ollama、
llama.cpp、DeepSeek 等。配置文件 `~/.sigil/config.toml`（`[llm]`、
`[agent]`、`[[mcp.servers]]`）；优先级 CLI > 环境变量 > 文件 > 默认值。
另有 `--tools-dir`（Lua 工具，默认 `~/.sigil/tools/`）、`--harness-dir`
（system prompt + skills，默认 `~/.sigil/harness/`）、`--memory`
（remember/recall 存储，默认 `~/.sigil/memory.md`）。

## 能力

- **原生工具调用** — 流式补全 + 非流式重放拿完整 tool-call 负载
  （OpenAI function-calling；兼容任意后端）。
- **工具** — `bash`、`read_file`、`write_file`、`git_status/diff/log`，
  加上 **Lua 脚本化工具**（`~/.sigil/tools/*.lua`，文件名 = 工具名，
  `-- desc:` / `-- schema:` 头部注释）。
- **嵌入式 LuaJIT + FFI** — `mlua` + vendored LuaJIT。脚本可
  `require("ffi")` 调任意 C 库（zlib `crc32` 已端到端验证）。
- **自进化** — `create_lua_tool`：模型写 Lua → sigil 落盘并热注册 →
  下一轮即可调用，跨会话持久。
- **Harness 自我改进** — `improve_harness`：模型可编辑自己的
  `system_prompt.md`（`edit_prompt`）和新增技能（`add_skill`）——
  指令层从内部进化，免重编译。
- **Harness bundle** — `~/.sigil/harness/` 存放 `system_prompt.md` 和
  `skills/`；改文件（不改代码）即可改变 agent 行为。
- **Safety（三层）** — 审批门（破坏性命令非交互模式默认拒绝；REPL 里
  交互 `y/N`）、熔断器（连续失败 3 次熔断）、执行环（只读/工作区/
  harness 变更）。
- **Memory** — `remember` / `recall`：模型自己维护的跨会话持久事实。
- **MCP 客户端** — 配置 `[[mcp.servers]]` 拉起 stdio MCP server，其工具
  变成 `mcp_<server>_<tool>`。
- **MCP server** — `sigil mcp` 把 sigil 自己的工具暴露给任意 MCP 客户端
  （Claude Desktop、Cursor、其他 agent）。
- **ACP server** — `sigil acp` 说 Agent Client Protocol（stdio 上的
  JSON-RPC），任意 ACP 宿主（编辑器、其他 agent）都能驱动 sigil。
- **ACP client** — `acp_call` 工具 spawn 外部 ACP server（另一个
  sigil、codex、编辑器 agent）并派发任务——跨进程委托，协议双向。
- **交互式 REPL** — 多轮对话 + 破坏性命令人工审批。
- **真正多轮上下文** — REPL 跨轮维护真实消息列表；`--resume` 把会话记录
  作为真实 user/assistant 消息载入（模型记得对话，而非文本 blob）。
- **行编辑 + 历史** — rustyline：上下键历史、行内编辑；每会话
  `.history` 文件跨会话持久。
- **流式输出** — 管道与 REPL 模式实时流式显示 assistant 文本
  （`[using tools…]` 标记工具调用中断）。
- **可续会话** — REPL 每轮记录到 `~/.sigil/sessions/<id>.jsonl`；
  `--resume <id>` 继续对话。
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
  审批 → 执行 → 重复直到模型停止（或 `max_steps`）。
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
| **12** ✅ | ACP client + `acp_call` — 跨进程委托 |
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

### Phase 48 — 2026-08-10（外部调研落地：smolagents + OpenHands）
四项调研机制落地：
- **并行工具执行**（OpenHands ParallelToolExecutor）：模型一次返回
  多个 tool_call 时并发执行，结果按原调用顺序回填。未知工具错误
  现在作为工具输出反馈给模型而非中断整个 turn（与 ToolRegistry
  契约一致）。
- **反应式压缩**（OpenHands condenser）：provider 上下文超限/历史
  畸形错误触发一次压缩重试（转录减半）后再放弃——补充主动预算
  检查（单个巨大工具输出可能超过其保护）。
- **规划检查点**（smolagents planning_interval）：每 N 个 agent
  循环步让模型复述计划；过期检查点被清除避免历史反复应答。
  `AgentConfig.planning_interval`（0 = 关闭）。
- **子 agent 报告模板**（smolagents managed agents）：spawn 的子
  agent 被要求以 `<final_answer>`/`<key_steps>` 标签结尾；
  `delegate result` 标注解析结构。
- 125 测试；ratchet 12/12；E2E：并行 turn 写两个文件、delegate
  报告携带子 agent 答案与步骤。
- **Daemon 压力测试轮**（c3）：16 并发发送、断线重连保持会话上下文、
  多轮 RSS 平稳——并抓到一个真实死锁：daemon 每连接 delta 写入任务
  在 tokio task 内用阻塞式 std mpsc `recv()`，每连接占用一个 worker，
  连接数超过 worker 数时整个 daemon 冻结（8 核机器上 N=8 复现，
  N≤6 正常）。修复：改用 `tokio::sync::mpsc::unbounded`
  （异步 `recv().await`）；N=16 现 16/16 全过。

### Phase 69 — 2026-08-11（嵌套深度上限——prime-agent RLM_MAX_DEPTH）
- prime-agent 的 `RLM_MAX_DEPTH` 默认为 1：顶层 agent 可以委托，但被委托的
  子代理不能再委托。sigil 此前无嵌套限制——子代理的工具注册表共享父的
  `DelegateTool` 实例，委托可任意深度嵌套。
- 现在每个 `DelegateTool` 带 `depth`（0 = 顶层）。子代理的注册表拿到 depth+1
  副本（`with_depth`）替换共享实例；`depth >= MAX_SUBAGENT_DEPTH` 时
  spawn/delegate/resume 拒绝（`check_depth`，「nesting depth limit (1)
  reached」）。只读 action（status/result/list/metrics/delete/send）任意深度
  可用。
- 测试 173 → 174（depth-0 spawn 通过；depth-1 spawn/delegate/resume 全被拒）。
  E2E：子代理两次尝试委托均被拒，并通过 `delegate list` 核验无子子代理产生。

### Phase 68 — 2026-08-11（per-agent condenser——OpenHands definition.condenser）
- `spawn` 与 `delegate` 批量的每个 task 现在接受可选 `max_context_messages` /
  `max_context_tokens`：该子代理的上下文预算（OpenHands per-definition
  `condenser` 的 sigil 对应物）只对该子代理覆盖父代理全局设置；`resume` 也携带
  覆盖。
- 管线：`spawn_subagent` 增加两个可选参数（现 9 参数——私有函数加
  `#[allow(clippy::too_many_arguments)]`），schema 在顶层与每个 task 对象内
  都说明。
- 测试 172 → 173（spawn 与批量带 condenser 覆盖均通过）。E2E：模型以
  `max_context_messages: 4` / `max_context_tokens: 3000` 派发子代理、轮询、
  收集结果——紧预算下正确回答。
- 至此 OpenHands 派生候选全部收口：批量并发（P65）、per-delegate metrics
  （P66）、max_children（P67）、per-agent condenser（P68）全部落地。

### Phase 67 — 2026-08-11（max_children 上限——OpenHands=5）
- 同时最多 5 个子代理在跑（`MAX_ACTIVE_SUBAGENTS`，镜像 OpenHands
  `max_children`）。`delegate spawn` 与阻塞 `delegate` 批量都在前置检查上限：
  running 子代理占用额度，finished（completed/failed）释放额度；超限批量在任何
  spawn 发生前即被拒，并提示「拆小批」。
- 批量的合并检查：`tasks.len() + 已在跑 <= 5`——无 running 时 3 任务批量可行，
  但已有 3 个子代理在跑时被拒。
- 测试 170 → 172（spawn 上限含 completed 释放额度；批量上限含 running 占用
  额度）。E2E：模型先试 6 任务批量被拒（`delegate batch has 6 tasks but at
  most 5 sub-agents may run at once; split into smaller batches`），重新规划
  为 3 任务批量并全部正确完成。

### Phase 66 — 2026-08-11（delegate metrics + loopback 代理修复）
- 新增 `delegate` `action=metrics`（OpenHands `usage_to_metrics` 视图）：
  每个子代理一行（自己的 prompt/completion 用量）+ 总计——父代理一眼看清
  全部委托账单。用量在完成时记录一次（替换而非合并），resume 句柄永不重复
  计数；无用量记录的 running 子代理跳过而非误计。
- 修复新测试暴露的真实 bug：reqwest client（LLM provider 与 `sigil doctor`
  探测）现设 `no_proxy()`。此前设置了 `http_proxy` 环境变量时，到本地端点的
  loopback 请求被路由进代理、代理回 502——破坏本地 LLM 端点与 fallback 测试
  （代理环境变量一出现，恰好 3 个测试以这种方式 flake）。
- 测试 169 → 170（metrics 聚合，含 running 子代理排除）。E2E：模型调用
  `action=metrics` 报告真实总计（21,128 prompt = 上轮 E2E 两个子代理
  10560+10568 之和）。

### Phase 65 — 2026-08-11（delegate 阻塞批量——OpenHands delegate）
- 新增 `delegate` action（OpenHands 阻塞 `delegate` 语义）：传 `tasks` 数组
  `[{task, name?, subagent_type?, model?}, ...]`，工具并发 spawn 所有子代理
  （每个跑在独立 tokio 任务里，真正并行）并阻塞等全部完成，一次调用返回聚合
  结果：`- d00001 [completed] math-agent (2 steps) (… tokens) — <final_answer>`。
- 可选 `timeout`（秒，默认 600）：超时未完成者报告为 `[running]` 而非报错——
  后台任务继续跑，稍后可用 `action=result` 收集；父代理的 turn 永不被挂死的
  子代理卡住（优于 OpenHands 的无界 join）。
- 每个 task 可单独覆盖 `name`（≤64 字符、trim，与 spawn 一致）、`subagent_type`、
  `model`（rlm.find_models 式按子任务选模型）。
- 测试 167 → 169（全完成聚合 + 超时部分返回）。E2E：模型一次 `delegate` 调用
  派发两个任务并报告两个回答（1+1=2 / 北京）；两个子代理均落盘 Completed，
  各自 name/steps/tokens 齐全。

### Phase 64 — 2026-08-11（daemon 多会话——herdr pane 模型）
- daemon 现在并行承载多个会话（herdr 术语 pane）：`DaemonState` 从单一消息
  列表升级为 `HashMap<id, SessionState>`；每会话独立历史，独立持久化到
  `sessions_dir`。
- `sigil send --session <id>` / `sigil attach --session <id>` 指定会话；
  未知 id 自动开新会话（以 `<id>.jsonl` 落盘，新增 `session::start_with_id`），
  后续 send 持续追加——侧线对话无需 `--resume` 切换。
- 新 socket 帧：`{"type":"session","action":"list"}` 回复
  `{"type":"sessions","sessions":[{id,turns,preview},...]}`；
  `{"type":"session","action":"new"}` 开新 pane 并回复
  `{"type":"session_created","id":...}`。
- 跨会话 turn 仍串行（单一 `loop_lock`，herdr 单运行时模型）——不并发跑
  多个 agent 循环。
- 测试 165 → 167（session 帧定向 + resolve_session 回退/新建）。
  E2E：pane-math（1+1=2，记得自己会话的上下文）与 pane-geo（巴黎）互不
  干扰；`list` 显示全部三个会话。

### Phase 63 — 2026-08-11（delegate spawn name 校验）
- `delegate spawn` 新增可选 `name` 参数（rlm `session_name` 语义，镜像
  prime-agent 的 `normalizeRequestedRlmSubagentSessionName`）：必须是字符串、
  trim 后非空、≤64 字符。trim 后的值记录在 handle 上，`resume` 时保留
  （续轮沿用原名，不再按新 task 重生成）。
- 默认名仍为 task 前 40 字符。
- 测试 164 → 165：拒绝空名/65 字符名、trim + 落盘、name 跨 resume 保留。
  E2E：模型按 description 传 `name` 并落盘 meta.json。

### Phase 62 — 2026-08-11（压缩由真实 token 用量驱动）
- `compact_messages_with_budget` 新增 `recent_prompt_tokens` 参数：最近一次
  LLM 调用的真实 `prompt_tokens` 用量（prime-agent 用实际 `usage.totalTokens`，
  无则回退 chars/4 估算）。作用：(a) 驱动超限触发；(b) 用真实/估算比值校准
  保留尾部的逐条估算——chars/4 对中文密集转录低估约 4 倍，未校准的 walk
  会保留过少。
- 预算检查每轮迭代都跑：第一步用估算、之后每步用实测用量。
- 测试 162 → 164：真实用量触发（估算在预算内、实测超限）、校准砍更多。
  E2E：多工具运行正常完成、usage 流经整个循环。

### Phase 61 — 2026-08-11（token 用量计量）
- 解析 provider 的 `usage` 块：流式 SSE 末块 + 工具回放 completion 两条路径，
  跨整次运行累计进 `RunResult.tokens`（`TokenUsage { prompt, completion }`）。
- 持久化到 delegate handle（meta.json）并在 `delegate status/list` 显示
  （`N prompt + M completion tokens`）。E2E 实证：子代理运行报告
  10598 prompt + 328 completion tokens 全链路。
- 「metrics 缺口」实为解析缺口而非上游缺口——OpenAI 兼容端点本就在返回
  `usage`，sigil 只是把它丢了。

### Phase 60 — 2026-08-11（父→子消息）
- `delegate send`（id, text）把消息排入子代理 inbox（`DelegateState.inbox`，
  限 50 条、持久化到 meta.json）——rlm `agent_message.send` 的另一半。
- 每个子代理获得 `check_parent_messages` 工具（一次性消费队列、旧的在前）
  + 任务里注入 COMMUNICATION RULES 段（提示每步开始先检查）。
- `resume` 把排队消息转移到新句柄（`transfer_inbox`）：延续轮继续看到
  父消息，旧记录不会重复消费。
- 2 个新测试：队列/持久化/一次性消费 + send 动作；resume inbox 转移
  （两个 meta.json 都更新）。

### Phase 59 — 2026-08-11（子→父消息）
- 每个子代理工具集注入 `child_message` 工具（rlm `agent_message.send`
  语义——最后一个语义缺口）：消息写入 `DelegateState.messages`，限最近
  50 条并持久化到 meta.json；父通过 `delegate status` 读取
  （"messages from sub-agent"）。
- 在子代理类型的 allowlist 过滤**之后**注册，因此任何类型都保留该工具。
- 1 个新测试：队列限长（60 次 push → 保留 50、丢弃最旧）、持久化、
  未知 id 忽略、status 显示消息。

### Phase 58 — 2026-08-11（按子任务选模型）
- `delegate spawn`（和 `resume`）接受可选 `model` 覆盖——rlm.find_models
  思路按子任务应用。由于 `ChatRequest.model` 是请求级字段，子代理的
  config 运行前覆盖即可（无需 provider 管道改动）。
- handle 记录实际运行的模型：`DelegateState.model`
  （`#[serde(default)]`，旧 meta.json 仍可加载），`delegate list`
  显示 `(model: …)`。
- 测试扩展：model 覆盖持久化到 meta.json、旧文件默认 None、list
  在有时显示模型。

### Phase 57 — 2026-08-11（delegate delete：rlm registry 补全）
- `delegate delete`（id）：删除已完成的子代理记录、释放 handle——prime-agent
  `rlm` registry 语义的最后缺口（spawn/status/result/resume/list
  此前已对齐）。
- `DelegateRegistry::remove` 同时删内存条目**和**磁盘 `<dir>/<id>/`
  目录，内存与磁盘保持同步；未知 id 报错清晰，running 中的子代理
  拒绝删除（后台任务仍在写该条目）。
- 1 个新测试：delete 删除记录与磁盘目录、未知 id 报错、running
  存活。

### Phase 56 — 2026-08-11（子代理步数可见性）
- delegate 注册表记录每个子代理消耗的步数：`DelegateState.steps:
  Option<usize>`（`#[serde(default)]`，旧 meta.json 无此字段也能加载）。
- 子代理改走 `run_messages`（不再用 `run_turn`）以捕获 RunResult
  元数据——完成后把步数写入注册表；`delegate list` 对已完成的子代理
  显示 `(N steps)`。metrics 积压项的第一片。
- 2 个新测试：steps 持久化 + 旧文件默认 None；list 仅在有时显示步数。

### Phase 55 — 2026-08-11（refine 审计 id + 合并历史）
- refinements.jsonl 每条事件带稳定 id：`action:FNV-1a(内容指纹)`
  （用 FNV-1a 而非 std 的 DefaultHasher——后者每进程随机种子，id 必须
  跨重启、跨 local/global 文件一致）。刻意不含 scope：同一改进从
  local 提升到 global 时保持同一 id。
- 新增 `sigil refinements` 命令打印审计日志（时间戳、scope、action、
  id、reason）；`--merged` 经 `load_merged_refinements` 合并会话局部
  历史（prime-agent `mergeRefinementHistory` 模式）：按 id 去重、
  local 胜出、按首次出现排序；无 id 的历史行用内容派生兜底 id 保持
  独立不互相覆盖。
- 5 个新测试：id 稳定性/scope 无关性、坏行跳过、local 胜出去重、
  排序、历史行保留。

### Phase 54 — 2026-08-11（子 agent 类型化：AgentDefinition 模式）
- 新增 `src/tools/subagent.rs`：子代理类型 = Markdown 文件 + 可选
  TOML frontmatter（`description`、可选 `tools` 白名单；用 TOML 替代
  OpenHands 的 YAML——零新依赖）；文件 body 即系统提示词后缀。
- 类型从 `<harness-dir>/agents/`（项目级，优先级高）与
  `~/.sigil/agents/`（用户级兜底）发现；同目录 first-wins，
  README.md 跳过。
- `delegate spawn` 支持 `subagent_type`：body 追加到父 prompt 状态的
  **深拷贝**上（共享 Arc 永不改动），白名单外的工具从子代理注册表
  剔除。未知类型报错并列出可用类型；`resume` 同样支持。
- delegate 工具描述动态列出可用类型（OpenHands get_factory_info
  模式）：名字、用途、各类型的工具白名单。
- 8 个新测试：frontmatter 解析（含缺失/畸形/空列表）、项目覆盖用户、
  README 跳过、未知类型报错、应用逻辑（prompt 后缀、父不受污染、
  工具过滤）。

### Phase 53 — 2026-08-11（子任务 resume + delegate 注册表持久化）
- 子代理状态落盘为 `~/.sigil/delegates/<id>/meta.json`（或
  `--delegates-dir`；原子写 tmp+rename），每次 insert/update 都持久化，
  启动时从磁盘恢复——子代理跨进程存活，新 handle id 重新索引到已恢复
  id 之上。
- `delegate` 新增 `resume` 动作（id + task）：用新指令延续已结束/失败
  的子代理；原任务 + 上次运行的最终报告/错误作为上下文回喂
  （OpenHands partial-result 模式）。运行中的子代理拒绝 resume；
  未知 id 清晰报错。`list` 显示血缘（"resumed from <id>"）。
- 3 个新测试：meta.json 持久化；磁盘恢复 + handle 重新索引；resume
  拒绝运行中/未知 id。

### Phase 52 — 2026-08-11（local/global 双轨第二步：scope 策略）
- 配置了会话局部层（--local-harness-dir）时，improve_harness
  **默认 scope=local**：实验自动落在会话层；scope=global 仍可用但
  成为显式的跨会话决策。无 local 层时默认保持 global（向后兼容，
  ratchet 的 live_prompt_edit 不受影响）。
- 工具描述/参数写明策略：global 是经过评审的跨会话表面——留给持久
  通用改进。
- 2 个新测试：有 local 层时默认 scope=local；显式 global 绕过 local
  默认（local 保持不动）。

### Phase 51 — 2026-08-11（improve_harness local/global 双轨第一步）
- **`--local-harness-dir`**：加载全局 + 会话局部两层 harness 的合并
  视图——local 的 `system_prompt.md` 覆盖全局；local 技能**遮蔽同名**
  全局技能（其余合并）。`Harness::load_with_local` 承担 merge。
- **`improve_harness` 新增 `scope` 参数**（`global` 默认 | `local`）：
  local 写会话局部目录（独立目录锁 + 独立 refinements.jsonl 审计，
  审计事件带 scope 字段），全局 harness 零污染——模型可跑会话实验，
  借鉴 prime-agent Continual Harness 的 local/global 双轨。
- PromptState 写入后刷新为**合并视图**（load_with_local 重载），
  下一轮立即生效。
- 6 个新测试：merge 语义（prompt 覆盖/skills 遮蔽/无 local 回退）+
  scope=local 写入与隔离 + 无 local 目录拒绝 + 刷新合并视图。

### Phase 50 — 2026-08-11（Lua 沙箱加固：authorized_imports 白名单）
- **`io`/`os` 标准库不再暴露给脚本工具**——此前 stdlib 开着
  `S::IO | S::OS`（注释声称 "NOT exposed" 但实际可用），脚本可绕过
  host 边界直接 `io.open` 任意文件 / `os.execute` 任意命令。现在
  文件/环境/时间全部走 `host` 辅助函数。
- **`require` 白名单**（smolagents `authorized_imports` 同款语义）：
  只放行内建模块 `ffi/string/table/math/bit/jit/coroutine`；未授权
  模块（如 `require('io')`）报明确错误，不再可能从文件系统加载 Lua
  文件（原 `require` 可走 loaders 读任意 .lua——文件读取侧信道）。
- **新增 `host.date(fmt)`** 替代 `os.date`：civil-date 算法（Howard
  Hinnant，无 chrono 依赖），支持 `%Y %m %d %H %M %S` 占位符；
  `test-tools/whoami.lua` 已迁移。
- `create_lua_tool` 的工具描述同步告知模型沙箱边界。
- 测试 +4：`io_and_os_not_exposed`、`require_allowlist_blocks_dangerous_modules`、
  `require_allowlist_still_loads_ffi`、`host_date_formats_civil_date`。

### Phase 49 — 2026-08-11（结构化运行结果 + UTF-8 日志修复）
- `run_messages` 现返回 `RunResult { output, steps, state }`
  （借鉴 smolagents 的 `RunResult`）而非裸 String——调用方能直接
  区分正常回答与预算用尽，无需解析错误文本。
- **步数预算不再中止回合**（smolagents `provide_final_answer`）：
  `max_steps` 用尽时循环追加一次**无工具** LLM 调用索取收尾总结，
  以 `RunState::StepLimitReached` 标记返回。此前是 `bail!`——
  REPL 显示错误、daemon 的 `done` 帧从不发送。
- daemon 的 `done` 帧新增 `state` 字段（`"ok"` / `"step_limit"`）；
  REPL 在答案是步数总结时打印黄色提示。
- 修复 daemon 多字节日志截断崩溃：「user turn」/「turn done」
  预览用 `&s[..len.min(40)]` 字节切片，40 落在 UTF-8 字符中间时
  panic（中文文本）。改用 `truncate_log` 字符安全截断。
- 126 测试；clippy 0；fmt 干净；ratchet 12/12；E2E：中文 daemon
  send 不再崩溃、答案完整往返。

### Phase 47 — 2026-08-10（CLI 一致性）
- **pipe 模式支持 `--resume`**：`sigil prompt --resume latest "…"`
  加载最近会话的完整转录作为上下文并追加新 prompt（此前 pipe 模式
  静默忽略 `--resume`）。`latest` 解析与 REPL/daemon 一致；恢复的
  会话 id 与轮数打印到 stderr。
- **`session list` 显示最后活动时间戳**（YYYY-DDD-HHMMSS，最新在
  前），连同轮数与首条消息预览——挑选要续接的会话一目了然。
- `turns_to_messages` 移到 session.rs（REPL/daemon/pipe 共享）；
  删除已无调用点的 `run_turn_with_approver` 包装。
- 120 测试；ratchet 12/12；E2E：daemon 写会话 → pipe
  `--resume latest` 从该上下文回答（42），无 resume 则答不出。

### Phase 46 — 2026-08-10（daemon 体验打磨）
- **`attach` 现在是持久会话**：与 daemon 多轮对话——每轮结束后
  提示符回来。`/quit`（或 Ctrl-D）退出，`/help` 列出命令；本地
  命令不会发给 daemon（此前 `/quit` 会被当成 user 消息）。
- **`send --timeout <秒>`**：限制等待 daemon 回复的时长（默认
  300，0 = 永久）；超时错误提示「turn 仍在 daemon 中运行，
  可再 attach/send 查看」。
- **daemon 启动横幅**显示 pid、socket 路径与可直接复制的
  attach/send 命令；连接死 socket 时提示「is the daemon running?
  start it with `sigil daemon …`」。
- 120 测试；ratchet 12/12；E2E：attach 多轮（42 → 43）、/quit
  干净退出、daemon 关闭后的友好错误——全绿。

### Phase 45 — 2026-08-10
- **ratchet 进 CI**：`ratchet.py --check-only` 是确定性、无 LLM 的
  校验（任务 schema + 源码锚点漂移检测）——挂入 CI 流水线。可拦截
  P44 那类「源码行变化导致文件锚点断言静默失效」的事故。
- **goal turn 预算**：`manage_goals add` 支持可选 `turn_budget`
  （一次 LLM 调用 = 一 turn）。active goal 在注入块和
  `manage_goals list` 中显示 `(turns N/M)`；预算用尽自动 pause 并
  停止注入。旧版 goals.jsonl 无缝加载（serde 默认值）。
- 120 测试；ratchet 12/12；E2E：goal 预算流程（add → 2 turns →
  自动 pause 落盘 → 不再注入）全绿。

### Phase 44r — 2026-08-10（review 修复）
对 Phase 41–44（压缩、改进门控、持久目标、异步委托、daemon）做
系统化 review，发现并修复 3 个真实 bug + 1 处文档修正：
- **goals 每轮重复注入**：注入在 agent 循环内无条件执行，多工具
  轮每迭代堆叠一条 `## Active goals` system 消息。现以
  `GOALS_MARKER` 为键——重复注入**原位替换**前一块
  （`inject_goals_block`，已加回归测试）。
- **delegate 注册表静默丢更新**：`insert`/`update`/`snapshot` 用
  `try_lock()` 且锁竞争时丢弃结果——子 agent 完成恰逢主 agent
  轮询时会永久丢失结果。改为 `lock().await`（async 方法，
  8 并发子 agent 更新回归测试）。
- **daemon `--resume latest` 未解析**：daemon 直接加载名为
  "latest" 的会话（报错）。现解析为最近活动的会话，与 REPL
  一致（E2E：daemon#1 → 42，`--resume latest` 重启 → 43）。
- `record_refinement` 文档修正（O_APPEND 追加而非目录锁）。
- 117 测试；ratchet 12/12；E2E：goals 流程、delegate 流程、
  daemon resume——全绿。

### Phase 44 — 2026-08-10
- **后台 daemon**（herdr 启发："the runtime your coding agents live
  on"）：`sigil daemon` 在 Unix socket（默认 `~/.sigil/daemon.sock`）
  上提供 agent 循环；`sigil attach` 是交互 client，`sigil send
  "message"` 是一次性 client——CLI 与 socket 说同一套 JSON 行协议。
  daemon 拥有循环：client 中途断开任务照跑；多轮上下文跨连接保持；
  每轮追加到 session 文件（daemon 重启可用 `--resume <id>` 恢复）。
  循环锁串行化 turn（并发 client FIFO）；delta 经 channel 流式发给
  发起连接的 client。
  - 顺带修复：REPL 从不发 system prompt（只有多轮历史）；daemon
    总是发 `system_prompt_for(user)` + 历史 + 新轮。
  - 测试：共 115——角色映射、一次性协议解析（delta+done）、
    daemon 生命周期 E2E。
  - E2E：daemon 起来 → `sigil send` 回 "42" → 第二次 `send` 回
    "43"（上下文跨连接延续）→ kill daemon → session 文件 4 轮
    （user/assistant × 2）。

### Phase 43 — 2026-08-10
- **异步子 agent 委托**（对标 prime-agent 的 `rlm(...)` handle
  语义）：`delegate` 引入注册表——`action=spawn` 启动子 agent 并
  立即返回 handle（`d00000`），它在后台（tokio task）继续跑，主
  agent 同时干别的活。`action=status` 轮询 running/completed/failed，
  `action=result` 收集最终摘要，`action=list` 枚举所有子 agent。
  结果仍不污染主对话——只有摘要文本进入。
  - 测试：共 113——delegate schema（4 个 action）、注册表生命周期。
  - E2E：模型 spawn 子 agent → 立即拿到 handle；下一轮轮询 status
    并收集结果（bash echo）。

### Phase 42 — 2026-08-10
- **持久目标**（借鉴 prime-agent 的 `GoalState`）：`manage_goals`
  让模型声明长期目标（add/complete/pause/resume/list/clear）。active
  目标每轮注入系统上下文（紧跟第一条 system 消息之后，不会被压缩
  折叠），并持久化到 `harness_dir/goals.jsonl`（目录锁内原子
  tmp+rename），重启不丢、多实例共享。完成的目标停止注入。
  - 质量门：objective ≥ 8 字符（一句清晰的话）；未知 id/action 拒绝；
    损坏的 jsonl 行优雅降级。
  - 测试：共 112——goals 往返、active 块格式、add/list/complete/
    pause/resume 流程、噪音拒绝。
  - E2E：模型 add goal → `goals.jsonl` 落盘；全新 sigil 进程在其
    上下文中看到注入；complete 后停止注入。

### Phase 41 — 2026-08-10
- **harness 改进门控**（借鉴 prime-agent 的 `/refine`）：
  `improve_harness` 写入（`edit_prompt`/`add_skill`）现在必须有
  `reason` 字段——一句话说明该改动为何是持久、有普遍价值的改进——
  并过确定性质量门：空内容、与当前相同的内容、过短的 prompt
  （< 100 字符）、无头技能、重复技能都在写入前被拒。每次被接受的
  编辑追加到 `harness_dir/refinements.jsonl`（ts/action/name/reason/
  result）审计，在既有目录锁内。
- **token 预算压缩**。`compact_messages_with_budget` 在消息数预算
  之上新增 chars/4 token 预算（默认 64k）：保留尾部从最新往回走、
  累加 token 估算确定。摘要现在还记录 `Files read:`/`Files
  modified:`（从 `read_file`/`write_file` 工具调用提取），压缩后
  模型仍知道碰过哪些文件（prime-agent 用 `CompactionEntry.details`
  做同样的事）。
  - 测试：共 107——token 预算压缩、文件操作追踪、门控（缺 reason/
    noop 重写/噪音形状/审计日志）。
  - E2E：模型无 `reason` 调 `improve_harness` 被拒且 prompt 文件
    原样；带 `reason` 时 skill 写入且 `refinements.jsonl` 增长。

### Phase 40 — 2026-08-10
- **工具超时**。`bash`/`git` 原来跑 `tokio::process::Command` 无
  墙上时钟预算——模型要 `sleep 10000`（或卡死的 git fetch）会永久
  挂死 agent 循环。两者现在都走 `run_with_timeout`（60s 预算，
  `kill_on_drop` 防孤儿进程），超时作为普通工具输出报告给模型
  （`[bash: command timed out after 60s — process killed]`），循环
  继续。脚本 Lua 工具本有类似超时；此改动补齐原生工具缺口。
- **上下文压缩**。REPL 会话 transcript 无限增长——每轮都重发整个
  历史。`compact_messages` 在 transcript 超过
  `max_context_messages`（默认 40，`0` 禁用）时把最老轮次折叠成
  一条确定性摘要 `system` 消息（带 role 标记的单行摘录，不调
  LLM）。切割只发生在安全边界（`user` 或无 tool_calls 的
  `assistant` 消息），保留区不会出现孤儿 `tool` 结果。
- **原子会话导出**。`session export` 原本用裸 `fs::write`——中途
  崩溃留下截断文件。现在先写 `<name>.json.tmp` 再 rename 覆盖目标
  （同文件系统原子），出错时清理临时文件。
  - 测试：`run_with_timeout_returns_output_for_fast_command`、
    `run_with_timeout_kills_slow_command`、`compact_*` ×4、导出
    roundtrip 加 tmp 无泄漏断言。共 100 项。
  - E2E：bash `sleep 120` ~60s 返回超时提示且不挂死；导出 roundtrip
    合法 JSON、无 `.tmp` 残留。

### Phase 39 — 2026-08-10
- **记忆写安全**。`remember` 和 `delete` 原本是裸的读-改-写
  （`read_to_string` → 编辑 → 整体 `write`）：两个并发 sigil 实例
  追加事实时可能互相覆盖（last-writer-wins）。两者现在把完整的
  读-查-追加-写 / 读-改-写 序列置于 Phase 32 跨进程目录锁下
  （`with_dir_lock` 锁 memory 文件父目录；裸文件名回退 `.`）。
  读操作（`recall`、`list`）保持无锁。
  - 测试：`concurrent_remembers_lose_nothing`（2×25 交错追加，
    50 条事实全存活）、`concurrent_delete_and_remember_are_consistent`
    （追加 vs 删除竞争下文件保持有效）。共 94 项。
  - E2E：两个真实管道模式 sigil 进程各并发 remember 10 条——
    20 条全部在场，零丢失。

### Phase 38 — 2026-08-10
- **LLM 故障转移**。OpenAI 兼容 provider 现在持端点链：主端点 +
  可选 fallback。瞬时故障（连接错误、5xx、429）带小退避（300ms×序号）
  重试下一端点；4xx 客户端错误（坏 schema、认证失败、未知模型）立即
  中止——重试另一端点无济于事。配置：config.toml 的
  `[llm] fallbacks = [{ base_url, api_key }]` 或
  `SIGIL_FALLBACK_BASE_URL`（逗号分隔）。
  - `OpenAiProvider::with_fallbacks(config, fallbacks)`（取代 `new`）；
    `PostError::{Fatal, Transient}` 分类；`post_with_fallback` 链循环
    带逐端点 stderr 提示。
  - 测试：`falls_back_to_second_endpoint_when_primary_is_down`、
    `falls_back_on_server_error_but_not_on_client_error`、
    `all_endpoints_down_reports_each_failure`（本地 TCP 假端点，
    零网络）。共 92 项。
  - E2E：主端点死 + cc-switch fallback → 模型正常回答且 stderr 出现
    `[sigil] primary endpoint failed`；主端点活 → 无提示。

### Phase 37 — 2026-08-10
- **ratchet LLM 方差容忍**。`live_prompt_edit` 偶发失败：模型口头说
  完成但没调工具（采样随机性，Phase 34/36 各遇一次）。不放松断言，
  改为 LLM 驱动任务支持按任务重试：`retries: N` + `retry_prompt`
  （显式「你必须实际调用工具」话术）。主循环断言失败或超时才重试；
  live 任务每次重试前恢复 baseline `system_prompt.md`。模型一次通过
  时无感；采样方差时自动兜住。

### Phase 36 — 2026-08-10
- **会话续接**。`session list` 现在为每个会话渲染摘要（轮数、首条
  用户消息的空白折叠预览、最后活动时间戳），按最新活动排序；
  `--resume latest` 与 `/session latest` 直接续接最近活动的会话，
  不用再手抄纳秒 id。Phase 31 之前的 legacy 会话（无 `ts`）回退到
  id 排序，永不破坏排序。
  - `session::SessionSummary` + `session::summaries(dir)`（按最后
    轮次 `ts` 最新在前，id 兜底）+ `session::latest(dir)`。
  - 测试：`summaries_order_by_last_activity_and_preview`、
    `summaries_fallback_to_id_order_for_legacy_sessions`（手工写
    JSONL——`append` 会自动盖章，legacy 行必须绕过它）、
    `summaries_empty_dir`。共 89 项。

### Phase 35 — 2026-08-10
- **跨实例知识同步**。运行中的 agent 循环每轮对 harness 目录做指纹
  检查（仅 stat：`system_prompt.md` 与各 `skills/*.md` 的 mtime+大小，
  排序保证顺序无关），检测到另一个 sigil 进程改动后自动重载实时提示词
  状态——并发 REPL、MCP server 或管道任务的 `improve_harness` /
  `skill_rollback` 在下一轮立即可见，无需重启。记忆本就共享（每次调用
  读文件）。
  - `harness::harness_fingerprint(dir) -> u64`（廉价、确定性、顺序无关）+
    `harness::sync_prompt_state(&RwLock<PromptState>, dir) ->
    io::Result<bool>`（仅变化时重载）。
  - `AgentConfig.sync_dir` 接入 `run_messages` 循环开头；main 从
    `--harness-dir` 注入。
  - 测试：`sync_prompt_state_reloads_after_external_edit`（提示词编辑 +
    新增技能被感知；空闲不重载）、`harness_fingerprint_changes_with_content`
    （内容/新增技能改变指纹；相同状态稳定）。共 86 项。

### Phase 34 — 2026-08-10
- **技能按需加载**。系统提示词现在常驻全量技能的紧凑索引（名+描述，
  不含 body），模型知道全库存在、按需用 `read_file` / `list_skills`
  读取任意正文。每轮按相关性 top-N 内联正文的逻辑不变。索引随技能库
  增长保持廉价——正文只在实际被内联或显式读取时才加载。
  - `Harness::skills_index_only_from(&[Skill])` 渲染索引（无技能时为空）；
    接入 `AgentConfig::system_prompt_for`。
  - 测试：`skills_index_only_lists_names_no_bodies`（索引只有名+描述、
    永不含正文）、`empty_skills_index_is_empty`，per-prompt 测试现在
    断言索引始终存在而只有相关正文被内联。

### Phase 33 — 2026-08-10
- **Ratchet 电池扩到 12 项**：新增 `acp_server_tool_use`（按真实协议驱动
  `sigil acp`——init → session/new → prompt → 工具调用 → close，断言
  模型回复）和 `session_export_import`（确定性 CLI 路径：手工构造带时间戳
  JSONL → 完整导出 → `--since` 过滤 → 导入 → 列表；所有断言必须全中，
  不涉 LLM）。
- **Ratchet tools-dir 隔离**：电池现在用独立的 `--tools-dir
  /tmp/sigil-ratchet-tools` 而非真实 `~/.sigil/tools`。之前模型在早期
  会话创建的工具会泄漏进每次运行——一个坏 schema 会 400 掉所有 LLM 调用
  导致整批失败。
- **脚本化工具 schema 加固**（`LuaTool::new`）：模型写的 `-- schema:`
  头缺顶层 `"type":"object"` 时回退安全默认值，不再原样广告（否则 LLM
  提供商会拒绝所有带工具请求）。由 `malformed_schema_falls_back_to_default`
  + `valid_schema_is_kept` 覆盖。

### Phase 32 — 2026-08-10
- **跨进程目录锁**：并发 sigil 实例（REPL + MCP server + 管道任务）现在
  可以安全写同一 harness/tools 目录——`create_lua_tool`、`improve_harness`
  和 `skill_rollback`/`versions_gc` 把「读旧→归档→写新」序列包在 advisory
  `<dir>/.sigil.lock` 里（fd-lock；Unix flock / Windows LockFileEx），两个
  写者永不交错。读不加锁；list 类动作在锁外。

### Phase 31 — 2026-08-10
- **轮次时间戳**：写入会话 JSONL 的每一轮现在带纳秒级 `ts` 字段（append
  时自动盖章；旧的无时间戳行经 `#[serde(default)]` 仍可正常加载）。
- **`session export --since <cutoff>`**：只导出 cutoff 及之后写入的轮次
  （格式与轮次时间戳相同）。无时间戳的旧轮次始终保留；过滤后为空则干净报错。

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
