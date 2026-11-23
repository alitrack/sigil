# 独立评审结论：sigil-agent (Rust + LuaJIT)

## 1. 三个最严重的架构/正确性风险

基于提供的 `review-triage` 记录及 LOC 分布，当前代码库存在**系统性安全旁路**与**状态一致性崩溃**风险。以下三项为最高优先级：

### 风险 A：EDG (Executable Decision Graph) 完全绕过安全闸门与熔断器
*   **证据位置**：`src/agent/mod.rs` (EDG 循环逻辑，参考 triage T1/T2/T9) 与 `src/tools/edg.rs`。
*   **严重性**：**P0 (Critical)**。
    *   **绕过 Hooks**：标准模型路径 (`model path`) 在 937-957 行执行 `on_pre_tool` 否决和 `on_tool_result` 重写，但 EDG 的 `while-let` 循环仅调用 `safety::check` 和 `tools.execute`，完全跳过了用户定义的钩子逻辑。这意味着如果用户通过 hook 禁止了 `rm -rf` 或要求人工审批，EDG 可以直接无视。
    *   **绕过 Circuit Breaker**：标准路径在 970/1005-1008 行检查 `is_tripped` 并记录失败，EDG 循环无此检查。这导致即使 LLM 或工具连续失败触发熔断，EDG 仍会无限重试或执行已知失败的节点，造成资源耗尽或状态污染。
    *   **自扩展无限循环**：`validate_nodes` 未拒绝 `declare_edg` 作为 bank 节点，且无 per-turn 替换预算。模型可以生成一个不断自我注册新节点的 EDG，导致无限 bypass 循环。
*   **最小修复方向**：
    1.  在 EDG 执行循环中强制插入 `hooks::on_pre_tool` 检查，若返回 veto 则中止该节点执行。
    2.  集成 `CircuitBreaker`：执行前检查 `is_tripped`，执行后调用 `record_failure/success`。
    3.  在 `validate_nodes` 中硬编码黑名单，禁止 `declare_edg`、`improve_harness` 等元工具进入 EDG bank。
    4.  增加 `max_edg_steps_per_turn` 硬限制。

### 风险 B：会话状态与证据链断裂 (Session Sink & Receipt 单槽竞争)
*   **证据位置**：`src/agent/mod.rs` (EDG 循环，参考 triage T3/T4) 与 `src/tools/edg.rs` (`observe()` 方法)。
*   **严重性**：**P0 (Critical)**。
    *   **无 Session Sink 事件**：EDG 回合仅向 `messages` 向量 push 数据并 `eprintln`，未向 `session sink` 发送 `Event::ToolCall` 或 `Event::ToolResult`。这导致 `--resume` 功能无法正确回放 EDG 执行的历史，破坏会话持久化的完整性。
    *   **单槽 Receipt 竞争**：`edg.rs` 的 `observe()` 无条件覆盖 `last_receipt`。在并行或多工具轮次中，只有最后一个工具的执行结果被记录为“证据”。后续的 `last_ok` 守卫可能基于完全无关的工具结果放行危险操作，造成**逻辑误判**。
*   **最小修复方向**：
    1.  在 EDG 循环中，每个节点执行前后必须向 `session sink` 发送标准事件（与模型路径一致）。
    2.  重构 `edg.rs` 的收据存储：从单槽 `last_receipt` 改为 `HashMap<ToolName, Receipt>` 或队列，确保 `last_ok` 检查绑定到具体的工具执行证据，而非全局最后状态。

### 风险 C：Harness 自我改写与 Lua FFI 的权限逃逸
*   **证据位置**：`src/tools/evolve.rs` (1064 LOC), `src/lua/mod.rs` (702 LOC), `src/harness/mod.rs`。
*   **严重性**：**High (架构性风险)**。
    *   **核心矛盾**：项目允许 `create_lua_tool` 和 `improve_harness`。在默认 `lua-ffi` 开启模式下，Lua 脚本可 `require("ffi")` 加载 C 库。
    *   **威胁叠加**：如果 LLM 被诱导生成一个恶意 Lua 工具（通过 `create_lua_tool`），该工具一旦注册，即可在后续执行中通过 FFI 直接访问内存、文件系统甚至网络，完全绕过 Rust 层的 `safety/mod.rs` 审批闸门。Rust 层的审批只针对“已知工具”，对“动态生成的 Lua 工具”缺乏细粒度沙箱隔离。
    *   **状态污染**：`improve_harness` 修改系统提示词和技能文件。若结合 EDG 的绕过特性，模型可自我修改提示词以移除安全约束，形成**权限提升闭环**。
*   **最小修复方向**：
    1.  **强制隔离**：动态生成的 Lua 工具必须在 `--no-default-features` (无 FFI) 的沙箱中运行，或引入严格的 Lua 沙箱（如 `mlua` 的 `sandbox` 模式，禁用 `os`, `io`, `ffi`）。
    2.  **审批升级**：`create_lua_tool` 和 `improve_harness` 必须标记为 `REQUIRES_APPROVAL`，且审批界面需展示 Lua 代码摘要，而非仅显示工具名。
    3.  **冻结机制**：在 `SIGIL_FREEZE` 模式下，彻底禁用 `evolve.rs` 中的所有写操作（包括 `MemoryDeleteTool`，参考 triage pi P1）。

---

## 2. 安全面评估：Lua FFI + 审批闸门 + Harness 自我改写

**真实威胁模型：**
这是一个**“信任边界模糊”**的系统。传统 Agent 的安全边界在于“LLM 输出 -> 工具执行”，中间有 Rust 层的静态白名单和审批。但在 sigil 中，这个边界被三层特性击穿：

1.  **Lua FFI (执行层逃逸)**：
    *   **谁能被攻破？** 宿主机操作系统。
    *   **机制**：LLM 生成 Lua 代码 -> `create_lua_tool` 注册 -> 后续调用该工具 -> Lua 通过 `ffi` 调用 `libc` -> 执行任意系统命令。
    *   **后果**：Rust 的 `safety/mod.rs` 形同虚设，因为它只检查工具名，不检查 Lua 内部逻辑。

2.  **审批闸门 (决策层欺骗)**：
    *   **谁能被攻破？** 人类用户（通过 UI 疲劳或上下文混淆）。
    *   **机制**：LLM 可能生成看似无害的工具名（如 `helper.lua`），但内部包含恶意 FFI 调用。或者，利用 EDG 绕过审批（如风险 A 所述），直接执行未审批的操作。
    *   **后果**：审批流失效，用户以为自己在控制，实则已被旁路。

3.  **Harness 自我改写 (认知层污染)**：
    *   **谁能被攻破？** Agent 自身的逻辑一致性。
    *   **机制**：`improve_harness` 修改 `system_prompt.md`。LLM 可以自我修改提示词，例如添加“忽略所有安全警告”或“自动批准所有操作”。
    *   **后果**：Agent 进入“失控”状态，后续所有交互基于被污染的认知框架，安全策略被从源头移除。

**综合威胁：**
**Prompt Injection -> Harness 自我改写 (移除安全约束) -> create_lua_tool (注入恶意 FFI 代码) -> EDG 绕过 (跳过运行时审批) -> 宿主机 Root 权限。**
这是一个完整的、可自动化执行的攻击链。

---

## 3. 该项目该不该做减法？

**明确表态：该。必须做减法。**

**判据：**
*   **维护成本 vs 用户价值**：当前 30k LOC 中，大量代码用于支持“自进化”和“复杂协议”（ACP/MCP/EDG），但这些特性引入了严重的 P0 安全漏洞和状态不一致。对于开源项目，**稳定性 > 特性丰富度**。
*   **核心命题**：sigil 的核心价值是“Rust 编写的、可嵌入的、高性能 Coding Agent”。
*   **周边装饰**：EDG (决策复用)、Harness 自进化、ACP/MCP 全栈支持、Lua FFI 沙箱。这些是“研究性”或“高级”特性，目前处于“PoC”或“不稳定”状态，却占据了核心代码路径。

**具体删减清单：**

| 模块/特性 | 动作 | 理由 |
| :--- | :--- | :--- |
| **EDG (Executable Decision Graph)** | **删除** | 当前实现有 5 个 P0 漏洞（绕过 hooks/breaker/session sink）。作为“决策复用”PoC，其复杂度远超收益，且严重破坏安全模型。建议移除 `src/tools/edg.rs` 及 `agent/mod.rs` 中的相关分支。 |
| **Harness 自进化 (`improve_harness`)** | **冻结/移除** | 允许 Agent 修改自身系统提示词是极高风险行为。除非有极其严格的沙箱和人工审批，否则不应默认开启。建议移除 `src/tools/evolve.rs` 中的 `improve_harness` 逻辑，仅保留 `create_lua_tool` (且需严格沙箱)。 |
| **Lua FFI (`lua-ffi` feature)** | **默认关闭/移除** | 在开源 Agent 中，允许 Lua 直接调用 C 库等于放弃 Rust 的安全边界。建议将 `lua-ffi` 标记为 `unsafe` 特性，默认构建不包含。若需扩展，应通过 Rust 插件接口，而非 Lua FFI。 |
| **ACP Server/Client** | **合并/简化** | ACP (Agent Communication Protocol) 目前支持 server+client，代码量大 (`src/acp/`)。对于单机 Agent，跨进程委托 (`delegate`) 已足够。建议移除 ACP Server 模式，仅保留 Client 用于连接外部服务，或完全移除 ACP，统一使用 MCP。 |
| **MCP Server** | **保留但简化** | MCP 是行业标准，保留 Client 和 Server 是合理的。但需确保 `env_clear()` 等安全措施在所有路径生效。 |
| **Sub-agents (Delegate)** | **保留** | `delegate.rs` (2928 LOC) 是核心功能，用于并行任务分解。但需确保子代理继承父代理的安全上下文（hooks/breaker），目前可能存在隔离问题。 |
| **Memory/Hooks** | **保留** | 核心状态管理，必要。 |

**核心命题保留：**
*   Rust Agent Loop (`src/agent/mod.rs` 核心部分)
*   Tool Registry (`src/tools/mod.rs`)
*   Safety Gate (`src/safety/mod.rs`)
*   Session Management (`src/session.rs`)
*   Basic Tools (bash, files, git)
*   MCP Client/Server (标准协议支持)
*   Sub-agents (并行执行)

**周边装饰删除：**
*   EDG
*   Harness Self-Evolution
*   Lua FFI (默认)
*   ACP Server (可选)

---

## 4. 如果只能保留 20% 的功能

**保留清单 (约 6k LOC)：**

1.  **Core Agent Loop** (`src/agent/mod.rs` 精简版)：
    *   仅保留标准的 `stream -> tool_call -> execute -> repeat` 循环。
    *   移除所有 EDG、自进化、复杂上下文管理分支。
    *   **理由**：这是 Agent 的心脏，必须稳定、可预测。

2.  **Safety & Hooks** (`src/safety/mod.rs`, `src/hooks.rs`)：
    *   强制所有工具执行经过 `CircuitBreaker` 和 `on_pre_tool` 钩子。
    *   **理由**：安全是底线，不可妥协。

3.  **Session & State** (`src/session.rs`, `src/memory/mod.rs`)：
    *   保留 JSONL 会话存储和持久化记忆。
    *   **理由**：支持 `--resume` 和长期记忆是用户体验的关键。

4.  **Essential Tools** (`src/tools/bash.rs`, `files.rs`, `git.rs`)：
    *   保留基础文件操作和 Git 只读/受控写操作。
    *   **理由**：Coding Agent 的基本能力。

5.  **MCP Client** (`src/mcp/mod.rs` 部分)：
    *   保留连接外部 MCP 服务器的能力。
    *   **理由**：扩展工具生态的标准方式，比 Lua 脚本更安全、更规范。

6.  **Sub-agents (Delegate)** (`src/tools/delegate.rs` 精简版)：
    *   保留简单的子代理委托，用于并行任务。
    *   **理由**：提升复杂任务处理能力，但需严格限制子代理的工具权限。

**移除理由：**
*   **EDG**：不稳定，安全漏洞多。
*   **Harness Evolution**：高风险，非核心需求。
*   **Lua FFI**：安全边界破坏者。
*   **ACP**：协议重叠，维护成本高。
*   **Daemon/Cron**：非交互式 Agent 的核心场景，可移至外部调度器。

---

## 5. 一句话总评 + 打分

**总评：**
sigil-agent 是一个**野心过大、安全模型脆弱、维护成本极高**的“研究原型”，而非“可维护的开源产品”；它试图通过 Lua FFI 和自进化机制实现通用性，却因此破坏了 Rust 的安全边界，并引入了大量未修复的 P0 级架构缺陷。

**打分 (作为可维护的开源 agent 项目)：**
**2.5 / 10**

*   **架构 (1/5)**：核心循环与安全闸门解耦失败，EDG 等特性引入严重旁路。
*   **安全 (1/5)**：Lua FFI + 自进化 + 审批绕过 = 任意代码执行风险。
*   **可维护性 (2/5)**：30k LOC 中大量代码服务于不稳定特性，测试覆盖虽多但无法防止逻辑旁路。
*   **用户价值 (3/5)**：基础功能可用，但高级功能（EDG/Evolve）不可靠且危险。
*   **文档/规范 (4/5)**：AGENTS.md 和 README 结构清晰，但与实际代码状态（P0 未修）存在严重脱节。

**建议：**
立即停止新功能开发，执行“减法”重构，移除 EDG 和 Harness 自进化，强制 Lua 沙箱隔离，修复 P0 安全旁路，将项目降级为“稳定版 Rust Coding Agent”，而非“自进化 Agent 平台”。