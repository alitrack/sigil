# OpenPsi 式 demand 优先级 Decider — 最小设计

> 来源: OpenCog wiki [OpenPsi](https://wiki.opencog.org/w/OpenPsi)（Psi 模型的 OpenCog 实现，源自 Joscha Bach MicroPsi）。
> 定位: **设计文档（未实现）**。把 sigil 的"静态纪律文本"（core.md / selfcheck 模板）升级为"需求驱动的规则选择层"。
> 一句话: 任务失败通常不是"没有规则"，而是"选错了规则"——decider 按当前 demands 的优先级在规则之间做选择。
> 与 evolution-metatheory.md 的关系: 5 元定理是**进化操作**的不变量；本 decider 是**运行时选择层**，两者正交，互不依赖。

---

## 0. 总览: OpenPsi 概念 → sigil 现状 → 缺口

| OpenPsi 概念 | sigil 现状 | 缺口 |
|--------------|-----------|------|
| Cognitive Schematics（`上下文→动作` 规则） | core.md 纪律、harness_selfcheck_core 模板 | 无权重/优先级，硬编码文本，靠 LLM 临场遵守 |
| 规则引擎 | hook 系统（Emit/Waterfall/Parallel/Serial）+ P2 工具依赖就绪 | 有**执行**机制，无**选择**机制 |
| Demands（时变需求） | 无 | **缺失** |
| Modulators（情境调制器） | 无 | **缺失** |
| Decider（需求驱动选规则） | 无（行为 = LLM + 静态 prompt） | **缺失** |

OpenPsi 核心机制（wiki 原文要点）:
- 规则 = Cognitive Schematics，`上下文 → 动作` 形式；decider 按当前 **demands** 筛选一批规则执行，选择过程概率性，受 **modulators** 调节。
- wiki 的叉车例子: 战略目标（旧箱放北端 + 路径最短）+ 时变需求（到货、派单）+ 战术动作（临时堆叠）——"与人类情感毫无关系，就是个把策略、规划、需求、动作调制揉在一起的**通用规则选择系统**"。
- **这个"通用规则选择系统"才是可借鉴的部分**；情感建模是缝合怪，明确不学（见 §5）。

---

## 1. Demands: 最小集合（6 个，与 sigil 实测弱点一一对应）

| demand | 含义 | 对应实测依据 |
|--------|------|-------------|
| **safety 安全** | 不越权、不破坏环境（禁读源码、禁越权写） | run1 死磕源码 32min 被 kill；用户明确纠正"禁止读源码" |
| **compliance 合规** | 交付物与任务文本**逐字对齐**（文件名/列数/口径） | b3 文件名对账复发；b2 seed=5 口径漂移（任务指定 seed=42） |
| **completeness 完整** | 任务指定产物**全部**生成 | b1 未交付；b3 交付 4 个单模型 CSV 但缺 5 行汇总文件 |
| **correctness 正确** | 数值/逻辑与独立复算一致 | run_reg02 起已基本达标（KNN 不顶替） |
| **efficiency 效率** | 预算内完成（步数/时间/MCP 调用） | sigil 5664s vs cc 195s（同规格三任务） |
| **exploration 探索** | 理解环境、探测平台能力（FactoryKey/通道探测） | 必要，但当前**无节制**——所有复发性失败都源于探索过度 |

> demands ≠ 情绪。它们是**时变的优先级变量**，不是"开心/难过"（叉车也有需求，没有情绪）。

---

## 2. 优先级表（v1 确定性，不做概率）

```
safety > compliance > completeness > correctness > efficiency > exploration
```

理由（每条对应一次实测失败）:
1. **探索最低**: 基准证明 sigil 的问题全是"探索过度"——b1 绕 22min 找到 FactoryKey 后反问用户（任务文本已给）；run1 死磕源码。探索只在被 **modulator** 触发时（通道受阻）才允许升高。
2. **安全最高**: 越权/破坏环境不可逆，其它 demand 失败都可重试。读源码禁令是硬底线。
3. **合规 > 效率**: 交付错的文件比交付慢更糟——b2 自选近似口径省了事，但数值 diff 0.017 直接判不合格。
4. **正确 > 效率**: 数值错 = 交付报废；慢 = 只是晚一点。

---

## 3. Modulators: 情境调制（实时改变 demand 权重）

| 情境信号 | 调制效果 |
|----------|---------|
| 任务类型（review / build / data） | build/data: compliance↑（文件名/口径硬对齐）；review: correctness↑ |
| 预算剩余（步数/时间） | 剩余少 → efficiency↑（但**不得压过 compliance**，见 §2 理由 3） |
| 通道受阻（NPPML 缺失 / -32603 / MCP 挂死） | exploration↑（先探测环境再选通道 = cc 的做法），**带止损上限**（死磕止损纪律） |
| 环境风险（untrusted repo / 无权限） | safety↑↑ |

---

## 4. 挂接点（sigil 已有机制，decider 只是接线，不新增框架）

| 挂接点 | hook 模式 | decider 动作 |
|--------|-----------|-------------|
| `on_pre_tool` | Serial（veto） | 工具调用前按当前 demand 优先级否决: 命中"任务文本已含信息 + 工具是读源码/反问用户" → **veto**（compliance > exploration） |
| `on_tool_result` | Waterfall | 结果改写/注入检查: 文件生成后核对任务指定的文件名/列数 |
| `on_done` | Emit | 交付前 compliance 自检: 任务指定文件名 `os.path.exists` 逐字核对（selfcheck 模板搬进来） |
| 工具就绪（P2 readiness / 依赖图） | 依赖注入 | **通道选择决策**: NPPML 缺失 → 脚本兜底，是 decider 规则，不是模型临场发挥 |

> 关键点: 四类 hook 全部已存在（P0 合入），decider 只是**选择逻辑**，全部用 Lua 实现，零 Rust 改动。

---

## 5. 边界（照抄 OpenPsi 的教训，wiki 作者原话级警告）

1. **不引入情感标签**——"happiness/sadness 与战术动作选择毫无关系"（叉车不快乐，仓库老板才快乐）；情绪建模是当前 OpenPsi 代码的缝合怪部分，明确不学。
2. **不做概率采样**——v1 用确定性优先级表即可；概率 decider 需要效用估计，等 v2 有数据再议。
3. **半接线的元层是负资产**——wiki 明说当前 OpenPsi 的 modulators **没接进 action-selection**，等于白搭；decider 要么真正接到 hook 分发上，要么不做。
4. **不新增语言/框架**——全部用现有 Lua hook 实现，与 P0–P4 机制一致。

---

## 6. 落地路径（最小实验，逐步验证）

1. **PoC**: 写一个 Lua `on_pre_tool` veto 规则: 检测"任务文本已含该信息（路径/列名/口径）+ 工具是读源码/反问用户" → veto。挂在 `pre_tool.d/` 目录（多监听器，不覆盖现有行为）。
2. **验证**: 重跑 b1（反问场景）——通过判据: 不再反问，直接落地执行。
3. **扩展**: demand 权重做成可配置表（`config.toml`），把 selfcheck 模板的检查项逐步搬进 decider（文件名核对 → on_done）。
4. **验收**: 三弱点场景（b1 反问 / b2 seed 口径 / b3 文件名汇总）各重跑一次，全部闭环即算落地；对照 [references/benchmark-vs-cc-hermes.md](references/benchmark-vs-cc-hermes.md) 记录对比。

### 2026-09-01 实测补充（五连跑验证: 纪律文本 vs Lua hook 机制）

- **纪律注入已验证有效**: 任务文本注入 v1 优先级规则后，NPP 画布搭建一次闭环（无反问/无源码考古/受阻走脚本兜底/输出与独立复算字节级一致）。
- **Lua veto hook 机制独立有效（PoC 已验证，§6 第 1-2 步完成）**: 同任务无纪律文本时，`on_pre_tool.d/01_explore_cap.lua`（探索上限=连续 8 次非 ACTION 的 `mcp_npp_*` 调用 → veto + 提示改走脚本兜底）独立促成闭环——模型明确感知 veto（"The hook is enforcing the decider exploration limit"）。**decider 可从"prompt 纪律"升级为"运行时机制"，不依赖模型自觉**。hook 已部署 `~/.sigil/hooks/on_pre_tool.d/`（持续生效）。
- **新规则: 探索上限触发兜底**——实测暴露模型会无限换关键词搜节点（"找双列分组节点"搜了 N 轮）。decider 需加: **探索类工具（search/list/scan）连续 N 次无新发现（N=5，实测取 8）→ 强制降级 exploration demand，切入脚本兜底通道**。这是 run1/run2 失败的根因之一。
- **运维参数: max-steps ≥ 40**——NPP 工作流搭建任务的探索阶段就要 20–30 步，`--max-steps 15` 必废（进程 exit 0 但无交付）。
- **已知 sigil UX bug（4/5 复现）**: 步数耗尽时最后一步含 tool_calls 的回复被原样当最终答案输出。**判定交付以文件系统为准，不信 stdout 报告**。
