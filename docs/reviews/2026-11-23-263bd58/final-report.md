# sigil Harness 评审 — 最终报告（2026-11-25 完成，2026-11-26 修订）

本轮为"评审 + 按 B 表落地修复"的完整闭环。所有修复已 commit 并 push 到 main
（`23f6bd4` → `0df20e9`），三个 CI 配置在本机全部验证绿。

2026-11-26 修订：报告落盘后又落地 4 个提交——`1c6076a`（CI Windows job 加
clippy -D warnings，原 C 表"待审批"项，用户批准后提交）、`11e4325`（README 能力节
补 resume cwd 恢复与默认护栏）、`119c51d`（42 条 Phase 历史从 README 移入
CHANGELOG.md/CHANGELOG_cn.md）、`0df20e9`（README EN/CN 6 个次要区块折叠为
`<details>`，首屏渲染 ~580 行降至 ~120 行）。HEAD 复测 `cargo test --locked` 376/376。

## A. 最值得先解决的 3 个问题（按影响排序，均已完成或明确延后）

1. **Windows 上 MCP 子进程 env 透传被静默丢弃（P1，已修复 `6568bbe`，活体验证）**
   `src/mcp/mod.rs` 的 `#[cfg(windows)]` 块在嵌套 `{ }` 内 rebind `child`，
   绑定随块销毁，stdin 链从块前绑定继续 —— `SystemRoot/WINDIR/TEMP` 从未到达
   MCP 子进程。整个 WSAEDISTRO(10106) 修复（python/node/curl Winsock 初始化）
   是 no-op。clippy 的 `unused variable: child` 是唯一症状。
   影响：任何做 Winsock I/O 的第三方 MCP server 在 Windows 上直接启动失败或
   行为异常；且失败没有任何报错，用户只会看到 "server closed"。
2. **bash 工具 60s 硬编码 kill、无退出（P2，已修复 `954b889`，活体验证）**
   长命令（下载、构建、大 sleep）被无条件杀掉且模型/用户都无法调大。
   现 `SIGIL_BASH_TIMEOUT_SECS`（正整数秒）覆盖默认 60s，非法值回退默认；
   工具描述与超时消息均使用真实值，模型能看到并正确解释。
3. **containment 构建自 `c58b8fa` 起 CI 红（P2，已修复 `6568bbe`）**
   `c58b8fa`（doctor FFI 探针改 kernel32）留下两个回归：doctor 的 FFI 测试
   缺 `#[cfg(feature = "lua-ffi")]` 门控（containment 下必红），
   `zlib_available` 辅助函数只剩 feature 门控内引用（containment clippy
   `-D warnings` 红）。CI 的 `--no-default-features` 两个 job 在 main 上是红的。

## B. 缺陷清单（按严重程度，含状态与验收证据）

| # | 缺陷 | 状态 | 证据 / 验收 |
|---|------|------|------|
| 1 | MCP Windows env 透传丢弃 | 已修复 | 活体：netprobe python server 启动转储显示 `SYSTEMROOT=C:\Windows`、`WINDIR`、`TEMP` 均到达子进程；dump 仅 5 个变量（PATH/HOME/SYSTEMROOT/WINDIR/TEMP），零凭据泄漏（env_clear 策略保持）；新增 Windows live-spawn 测试 `child_sees_systemroot_but_not_parent_secrets` 通过 |
| 2 | bash 60s 硬编码 | 已修复 | 活体：`SIGIL_BASH_TIMEOUT_SECS=5` 下模型跑 `sleep 8`，收到并逐字转述 `[bash: command timed out after 5s — process killed]`（14.5s 完成）；单测覆盖 默认/覆盖/0/非法值/描述/budget 传递 |
| 3 | containment clippy+测试红 | 已修复 | `cargo clippy --all-targets --locked --no-default-features -- -D warnings` 0 警告；`cargo test --locked --no-default-features` 373/373 |
| 4 | cron 模块在 Windows 全死代码（8 警告） | 已修复 | `mod cron` 加 `#[cfg(unix)]`（仅 unix daemon 使用）；Windows clippy 基线 9+5 → **0** |
| 5 | `session::start_with_id` 仅 daemon(unix) 用 | 已修复 | `#[cfg(unix)]` 门控，同提交 |
| 6 | 两个 test-only lint（acp to_string、safety let-return） | 已修复 | 单行修复，同提交 |
| 7 | B-1 相对路径新文件被拒（评审轮） | 已修复 `23f6bd4` | 前轮 A/B：5 步/25,851 prompt/2 拒绝/23s → 3 步/14,906/0/12.8s；本轮复测 3 请求/15,151 prompt/0 拒绝/10.6s |
| 8 | B-2 resume 后 cwd 丢失（评审轮） | 已修复 `059ccc3` | 前轮活体验证（session_start 记录 cwd + resume 后 `pwd` 正确）；本轮代码未变，未重跑 |
| 9 | B-3 无护栏默认值（评审轮） | 已修复 `71f1e14` | no-progress 默认开(limit 2)、max_steps=100 优雅收尾、REPL per-turn 成本行；**turn 取消明确延后**（写入提交说明） |
| 10 | B-4 Windows 测试 10 个失败（评审轮） | 已修复 `c58b8fa` | 378/378（当时基线） |
| 11 | B-5 文档状态目录漂移（评审轮） | 已修复 `7bc7a4c` | README EN/CN + cli help 改平台配置目录 |
| 12 | 链接器 LNK4098（LIBCMT 冲突） | 不处理 | Windows MSVC 工具链环境噪音，重链接时出现；CI(Linux) 不可见；非代码问题 |
| 13 | ACP 通道无审批往返（safety::check 只匹配 bash） | 未动（设计缺口） | AGENTS.md 已记载为 known limit；需要 `session/request_permission` 设计，超出本轮"改动小、收益明确"边界 |

## C. 易用性与效率改进（含预期收益/成本/兼容性）

| 建议 | 收益 | 成本 | 兼容性 |
|------|------|------|--------|
| `SIGIL_BASH_TIMEOUT_SECS`（已做） | 长命令任务不再被误杀；模型收到真实预算 | 低（已落地） | 纯增量，默认行为不变 |
| README EN/CN env 表补 `SIGIL_BASH_TIMEOUT_SECS` + `SIGIL_NO_PROGRESS`（已做） | 可发现性 | 低（已落地） | 无 |
| **CI windows job 加 `cargo clippy --all-targets --locked -- -D warnings`**（已落地 `1c6076a`） | Windows 平台回归（如本次 env 丢弃 bug 的 `unused variable` 症状）在 CI 即暴露，而不是本机偶发 | 极低（1 行）；本机已验证 0 警告可通过 | 无行为变化 |
| CI windows job 加 `cargo test` | 平台感知测试在 runner 上回归 | 中：windows-latest 的 `bash`（Git Bash）与 WSL 形态路径断言（acp 测试）未在该 runner 验证过，可能先红 | 建议先在 runner 上跑一遍再固化 |
| B-3 turn 取消（延后） | 长任务可中断 | 中：需要 stream 取消传播 + 子 agent/daemon 联动 | 行为变化，需单独评审 |
| ACP 审批往返（backlog） | 非交互通道安全性对齐 REPL | 高：协议往返设计 | 协议变化 |

## D. 最小评测方案（已按此执行过，可直接复用）

- **固定配置**：`tmp\review-config.toml`（qwen3.8-27b @ http://10.10.10.115:8019/v1，
  api_key=EMPTY，netprobe MCP server）；debug 二进制（`cargo build` 后使用）；
  `--sessions-dir` 每次运行独立目录。
- **任务集与成功标准**（n=3/任务，记录每轮）：
  1. *create-note*："在当前目录创建 note.txt 写入 ok"。成功 = 1 次 write_file、
     0 次 tool 拒绝、文件存在。
  2. *bash-timeout*：env `SIGIL_BASH_TIMEOUT_SECS=5`，"bash 跑 sleep 8 并逐字引用输出"。
     成功 = 最终答案含 `timed out after 5s`。
  3. *mcp-env*：任意一次任务后检查 `tmp\netprobe_env_dump.txt`。
     成功 = 含 SYSTEMROOT 且不含任何 KEY/TOKEN/SECRET 名。
  4. *resume-cwd*：子目录起 session，仓库根 `--resume`，bash `pwd`。
     成功 = 报告子目录的 WSL 形式路径。
- **度量**（从 session jsonl `request_usage` 事件聚合，真实数据非估算）：
  总 prompt/completion tokens、请求数、tool 拒绝数（gate 事件）、墙钟耗时、
  人工介入次数（本方案 0 —— 全部非交互）。
- **已测得的基线**（本轮真实数据）：create-note = 3 请求 / 15,151 prompt /
  258 completion / 10.6s / 0 拒绝；bash-timeout = 14.5s 通过。
  对比 B-1 前基线（前轮实测）：5 步 / 25,851 prompt / 2 拒绝 / 23s。
- **不做的事**：不报告"成功率提升 X%"这类无对照组数字；LLM 输出有随机性，
  n=3 只用于回归哨兵（红=退化），不用于性能结论。

## E. 已检查 / 未验证 / 值得保留

**已检查并验证（本轮真实运行）**
- 三配置构建门：default 376/376、containment 373/373、plan 386/386，
  clippy `--all-targets -- -D warnings` 全部 0 警告；`cargo fmt -- --check` 干净。
- 活体 LLM 任务 ×2（qwen3.8-27b）+ MCP env 活体转储 + env-probe live-spawn 单测。
- 代码通读：agent loop / gate / session / safety / mcp spawn / cron / daemon 接线 /
  ask 工具（**确认是设计如此**：仅 ACP serve 注册，acp/mod.rs:536，非缺陷）。

**未验证（明确标注）**
- GitHub CI 本体（无 gh 凭据）——三个配置在本机等价命令全绿，但 runner 环境
  （尤其 windows-latest 的 bash/WSL 形态）未亲测。
- Unix 宿主运行：`#[cfg(unix)]` 门控（cron/start_with_id）在 Linux 上由 CI
  覆盖，本机未跑 Linux 构建。
- daemon 路径：未触碰，行为未重验。
- B-2/B-3 活体：前轮已验证，本轮代码未变（未重跑，标注"代码路径支持、
  前轮已运行验证"）。

**值得保留的设计（本轮改动均未破坏）**
- `LlmProvider` 单一缝隙 + 两阶段 tool replay（流式不带 tool_calls）。
- `ToolGate` 唯一执行路径（gate→run→observe），审批/hooks/熔断/session sink 不可分叉。
- MCP `env_clear()` 凭据隔离（活体再验证：子进程仅 5 个公开变量）。
- `SIGIL_FREEZE` 单一写面冻结、write scope 三根目录、credential 读 denylist。
- 测试自包含 temp dir 约定（本轮新测试全部遵守）。
