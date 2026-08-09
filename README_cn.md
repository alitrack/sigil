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

> 状态：**Phase 16** — `sigil doctor` 环境自检。管道模式、带真实 FFI 的
> 脚本化 Lua 工具、自进化闭环、三层 safety、MCP 客户端**和 server**、
> ACP server **和 client**（跨进程委托）、持久 memory、真正多轮上下文
> 的交互式 REPL、可续会话、子代理委托、流式输出、ratchet 回归电池、
> 模型驱动 harness 改进、技能内容内联、TUI 彩色输出、`/tools` 搜索、
> 诊断子命令 `doctor`——全部端到端可用。

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
