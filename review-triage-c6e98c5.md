# 多领域专家评审记录 — sigil c6e98c5 (1f6da30..c6e98c5, 5 commits)

- 评审时间: 2026-11-03
- 评审范围: EDG (Executable Decision Graph, HarnessMM 风格 guarded decision reuse) + SIGIL_FREEZE + stream usage/[stats]
- 评审者 (3 路并行, 独立模型/端点):
  1. **sigil** (本仓库 release 二进制, MLX 127.0.0.1:12345) → 22 findings, VERDICT **打回**
  2. **codex** (ChatGPT 官方 CLI, 独立模型族) → 13 findings, VERDICT **打回**
  3. **pi/sglang-local** (10.10.10.115:8014 qwen3.8-27b 直连 API, 953s, finish=stop) → 14 findings, VERDICT **有条件放行**

## 质量门 (本机实测)
- `cargo build --release`: OK (8.4s)
- `cargo fmt -- --check`: 0 diff
- `cargo clippy --all-targets -- -D warnings`: 0 warnings
- `cargo test`: **359 passed, 0 failed** (AGENTS.md 标称 290, 实际含 EDG 7 tests 已升至 359)

## 双源收敛 P0/P1 (两路独立给出, 已逐条源码核实)

| ID | 来源 | 核实结果 | 严重级 |
|---|---|---|---|
| T1 EDG while-let 绕过 hooks (on_pre_tool veto / on_tool_result rewrite) | sigil 2.5 + codex F1 | **属实**: model path 937-957 有 hooks 分派, EDG 循环 (1085-1133) 仅 `safety::check`+`tools.execute` | P0 |
| T2 EDG 绕过 circuit breaker (is_tripped / record_failure/success) | sigil 2.1 + codex F1 | **属实**: model path 970/1005-1008 有 breaker, EDG 循环没有 | P0 |
| T3 EDG 回合无 session sink 事件 (Event::ToolCall/ToolResult 缺失) | sigil 2.4 + codex F2 | **属实**: 循环内仅 messages.push + eprintln, 无 sink | P0 |
| T4 `last_receipt` 单槽: 并行多 tool 轮只有最后一个 receipt 可见, last_ok guard 可能基于无关工具的证据放行 | sigil 1.4 + codex F3 | **属实**: edg.rs observe() 无条件覆盖; 无 per-tool 收据 | P0 |
| T5 inline ```edg 块注册后原样进入 assistant message/session log (不剥离) → 上下文自模仿污染 | sigil 3.1 + codex F8(部分) | **属实**: 842-846 注册, 847-856 原文 push/sink, 无任何 strip | P1 |
| T6 parse_inline 失败完全静默 (.ok()? 无诊断) | sigil 3.2 + codex F8 | **属实**: 检测块后 JSON/校验失败均返回 None, 无 eprintln | P1 |
| T7 replace() done 按 label 保留: 新 bank 复用旧 label 则节点永久 ineligible; 依赖链跨 bank 错配 | sigil 1.1 + codex F5 | **属实**: edg.rs replace()/eligible() done 无 epoch | P1 |
| T8 claim() 先记 done 再执行: 执行失败也被计为 certified bypass (bypasses 统计虚高) | sigil 1.5 + codex F7 | **属实**: 循环内 claim 在 execute 前 | P1 |
| T9 无 per-turn 替换预算; `declare_edg` 本身可作为 bank node (validate_nodes 不拒) → 自扩展 bank 的无限 bypass 循环 | sigil 2.2 + codex(隐含) | **属实**: validate_nodes 无 tool 黑名单, while-let 无计数上限 | P0 |

## 单源 finding (已核实, 未双源收敛)

| ID | 来源 | 核实结果 | 严重级 |
|---|---|---|---|
| S1 is_literal 只拒"整串是 ${...}/{{...}}"的字符串; 内嵌占位符 (如 `python3 ${script}.py`) 通过闭包检查, 原样执行并写入证书 | sigil 1.2 + codex F9 | **属实**: edg.rs is_literal 仅 starts_with+ends_with 整串判定 | P1 |
| S2 SIGIL_FREEZE 的 Err 返回 → loop 计算 ok=false → 下游 last_ok guard 正确拒绝 (方向对); 但 freeze 是否覆盖 memory_delete / add_hook / edit_hook 删除等全部写表面未核实 | sigil 4.3 | 部分属实 (机制方向对), 覆盖面待查 | P2 |
| S3 stream_options.include_usage 无条件发送: 对严格校验 body 的老版 vLLM/部分 proxy 可能 400 | sigil 5.1 | **属实** (无条件写入, openai.rs:383); 本机 8014/12345/3001 均兼容, 风险限未知第三方端点 | P2 |
| S4 file_exists guard 路径不沙箱化 (可 `../` 或绝对路径探任意外存性, 泄漏进 stderr 证书账本) | sigil 1.3 | **属实**: eligible() 仅按前缀判相对/绝对, 无 `..` 拒绝 | P2 |
| S5 bank 为 registry 级共享 Arc: 子代理与父代理共享同一 bank, 跨 episode done 泄漏 | sigil 6.1 + codex F13 | **属实**: default_registry 持 edg_bank, clone_shallow 共享 | P2 (单进程内多会话场景) |
| S6 SIGIL_EDG=off 时 declare_edg 仍被广告, 模型写银行但永不执行 (无 abstentions 计数) | sigil 6.2 | **属实** | P2 |
| S7 abstentions 只在 gate-block 分支 +1, 其余回退路径 (无 eligible / unknown guard / unclosed) 不计 → stats 误导 | sigil 2.7 + codex F7 | **属实** | P2 |
| S8 harness mutation ring 包装 `[harness mutation] {result}` 发生在 breaker 记录之前, 包装串改变 ok 判定输入 (若 result 恰以 "error:" 开头) | codex F4 | 方向属实但触发条件苛刻 (工具输出本身以 "error:" 开头) | P2 |

## 已排除的误报
- "while-let 对已 claim 节点可再次 eligible" — 两路均确认 single-fire 对静态 bank 成立 (done 集合阻断), 仅 T9 自扩展路径是真正的无限向量
- "lock 跨 await 持有/死锁" — 两路均确认无跨 await 持锁, 无嵌套取锁
- "assistant→tool 配对断裂" — id 匹配 (edg_{seq}), OpenAI-compat 下配对成立

## pi 独立新发现 (sigil/codex 未覆盖, 已核实)

| ID | 核实结果 | 严重级 |
|---|---|---|
| P1 MemoryDeleteTool 无 SIGIL_FREEZE 保护 (memory.rs:112, 只有 RememberTool:41 有) — freeze 下模型仍可删长期记忆 | **属实** | P1 |
| P2 cwd() 注释声称"session cwd header (same source the bash tool trusts)" 但实现仅 std::env::current_dir() — 文档失真 | **属实** | P2 |
| P3 record_failure 只删直接后继, A→B→C 中 A 失败则 C 永久滞留 (transitive leak) | **属实** | P2 |
| P4 ToolRegistry clone_shallow 共享 edg_bank Arc — 浅克隆多会话跨 session 触发对方 bank (与 S5 同族) | **属实** | P2 |

## 裁决 (三路 verdict 分歧: sigil 打回 / codex 打回 / pi 有条件放行)
按 triage 纪律: 裁决依据"所有 P0 是否已有具体修复方案"。当前 commit 的 P0 (T1/T2/T3/T4/T9) 均**未修**, 故裁决 = **打回**, 与较严的两路一致。pi 的"有条件放行"只覆盖它自己看到的 5 条 (未含 hooks/breaker 绕过与自扩展循环两个 P0)。

## 开工前必须改的 10 条 (三源锚定, 全部经本机源码核实)
1. [P0] EDG while-let 并入 model path 完整闸门: on_pre_tool veto + breaker is_tripped 检查 + execute 后 record_failure/success (T1+T2)
2. [P0] while-let 加 per-turn 替换上限 + validate_nodes 拒绝 bank-mutating tools (declare_edg) — 封死自扩展无限循环 (T9)
3. [P0] EDG 每回合发 Event::Assistant/ToolCall(revert_key)/ToolResult 到 session sink, 保 replay/rollback 保真 (T3)
4. [P0] observe() 改 per-tool-name 收据, last_ok 绑定相关 receipt 而非全局末位 (T4)
5. [P1] is_literal 全串子串扫描: 任何含 ${…}/{{…}} 的字符串拒收 (S1)
6. [P1] parse_inline: 失败发 eprintln 诊断; 成功后从 assistant_text 剥离块再 push/sink (T5+T6)
7. [P1] replace() 引入 bank epoch, done/failed 按 (epoch,label) 记 (T7)
8. [P1] claim() 移到执行成功之后 (bypasses 区分 attempted/succeeded); abstentions 补全所有回退路径 (T8+S7)
9. [P1] SIGIL_FREEZE 覆盖 MemoryDeleteTool (freeze 下禁止删除) (pi P1)
10. [P2] 清理项: cwd() 注释纠偏; file_exists guard 拒 `..`/绝对路径; record_failure 清传递依赖; clone_shallow 银行隔离 (或文档声明); SIGIL_EDG=off 时不广告 declare_edg

## 原始输出
- /tmp/edg_review/sigil_out.md (31KB, 22 findings)
- /tmp/edg_review/codex_out.md (15KB, 13 findings)
- /tmp/edg_review/pi_out.md (17KB, 14 findings, 953s)
