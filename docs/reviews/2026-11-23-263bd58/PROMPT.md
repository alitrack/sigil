你是一位多领域专家（软件架构 / 安全 / LLM-agent 研究 / 开源产品与维护经济学）。请对当前仓库做一次**独立**评审，只做只读分析，不要修改任何文件，不要运行构建/测试（如需可读关键源码）。

背景事实（请自行核实，不要照抄）：
- 这是一个 Rust + LuaJIT 的 coding agent（crate `sigil-agent`，binary `sigil`），~30k 行 src，192 commits，README 声称 Phase 0-48 / Changelog 到 Phase 71。
- 特性面非常宽：pipe/REPL/ACP server+client/MCP server+client、daemon、cron、sub-agents(delegate, 2900 行)、goals、memory、hooks、harness 自进化(create_lua_tool/improve_harness)、版本回滚、EDG(guarded decision reuse, PoC)、plan(Lua 沙箱工作流 DSL)、doctor、UI、session 导出/导入。
- 最近一次三方评审(review-triage-c6e98c5.md)对 EDG 判"打回"，列了 5 个 P0（EDG 循环绕过 hooks/breaker/session sink、单槽 receipt、自扩展无限循环），现在看代码这些 P0 似乎仍未修。

请给出：
1. **三个最严重的架构/正确性风险**（要指到具体文件:行范围，说明为什么严重，给最小修复方向）。
2. **安全面评估**：Lua FFI 沙箱、审批闸门、harness 自我改写这三个东西叠在一起的真实威胁模型是什么？谁能被攻破？
3. **"该项目该不该做减法"**：明确表态（该/不该），如果该，给出**具体的删减清单**——哪些模块/特性应该删除、合并、或冻结在 feature flag 后面，各自的判据（维护成本 vs 用户价值 vs 与核心命题的相关性）。要区分"核心命题"和"周边装饰"。
4. **如果只能保留 20% 的功能**，你保留哪几个，为什么。
5. 一句话总评 + 0-10 分打分（对"作为可维护的开源 agent 项目"）。

要求：结论先行，证据具体（文件名/行号），不要客套话，中文输出。
