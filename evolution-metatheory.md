# sigil 进化元定理 → 设计原则

> 来源:Cordis 论文《A Programming Paradigm for Spatiotemporal Composability》的 5 个元定理。
> 定位:**不是形式化证明**,而是把 5 个定理当作 sigil 自进化(harness evolution)操作的**设计判据**——
> 每个定理给出一个可执行的检查,对应 P0–P3 已经落地的具体机制 + 对应测试。
> 一句话:进化必须**保类型、可回滚、空间隔离、保进展、可合流**;任何进化操作违反其中一条就是 bug。

---

## 0. 总览:5 定理 → sigil 机制

| 定理 | 含义 | sigil 落地机制 | 对应测试 |
|------|------|----------------|----------|
| **Preservation**(类型保持) | 进化不破坏 Agent 类型系统/契约 | 契约门(hook 必须 `return function`)、schema 净化、安全门三级 | `evolve_hooks_tests`、`scripted::sanitize_schema` |
| **Temporal Composability**(时间可组合) | 可回滚到任意历史点 | P1 可逆效应(LIFO 运行时回滚)+ P3 会话日志 `revert_key`/`revertible` + `versions` 归档 | `revertible::`、`p2_/p3_` 系列 |
| **Spatial Composability**(空间可组合) | 工具 A 的进化不影响工具 B 的注册 | P2 依赖注入(默认无依赖 = 永远就绪)、依赖图反向边、`register` 不扰动无关工具 | `p2_register_unblocks_*`、`re_register_same_name_*` |
| **Progress**(进展) | 进化不死锁/无限循环 | 依赖环检测(环 = 永不就绪,不挂起)、`max_steps`、circuit breaker | `p2_cycle_is_never_ready` |
| **Confluence**(合流) | 并行进化有冲突时可收敛 | LIFO 效应栈(逆组合正确)、hot-queue 单点排空、`with_dir_lock` 文件锁、hook 按文件名确定性排序 | `p2_delta_reports_became_ready`、`hooks::` 分发 |

> 这 5 条不是新代码,是**已有机制的不变量声明**:后续任何进化相关改动,先问"是否违反其中一条",违反即回滚/拒绝。

---

## 1. Preservation(类型保持)

**定理**:进化操作不破坏 Agent 的类型系统——即:进化后,工具/钩子的输入输出契约仍然成立,agent 循环的类型假设不被打破。

**sigil 检查判据**(进化前/后各问一次):
1. **钩子契约**:`edit_hook` 的内容必须包含 `return function(ctx) ... end`,否则在写入前被契约门拒绝(hook 必须导出一个函数,否则 `hooks.rs` 分发层无法调用)。
2. **参数 schema 契约**:`create_lua_tool` 的 `-- schema:` 若是非法 JSON Schema(如 `properties` 不是对象),`sanitize_schema` 在注册前净化,绝不把会 400 的 schema 递给 LLM。
3. **安全契约**:三级安全门(执行环/评审门)对进化产物同样生效——进化不能通过写 harness 绕过权限。

**已落地**:`improve_harness`/`create_lua_tool` 的写入前校验、`scripted::sanitize_schema`。
**已证**:`evolve_hooks_tests`(契约门拒绝非函数 hook)、`scripted` schema 净化测试。

---

## 2. Temporal Composability(时间可组合)

**定理**:进化可回滚到任意历史点——每个副作用都配一个显式逆函数,逆函数可以按序组合(论文 Theorem 5:track 是幺半群同态)。

**sigil 检查判据**:
1. **每个 harness 写入都产生一个可逆效应**(P1):`improve_harness` 的 4 个写动作(`edit_prompt`/`add_skill`/`edit_hook`/`delete_hook`)每次成功后,向共享 `EffectTracker` 注册一个"恢复写入前内容(或删除新建文件)"的逆闭包。
2. **LIFO 组合正确**:逆操作按注册逆序执行,`rollback(n)` 逐帧恢复,`revert_effects` 工具是运行时入口(`last`/`all`)。
3. **会话日志逐帧可定位**(P3):每条 `tool_call` 带 `revert_key`(name + 规范化 args 的稳定指纹),每条 `tool_result` 带 `revertible` 标注(`Some(true)` = 该调用注册了可逆效应,可被逐帧回滚;`None` = 未评估,不是"不可逆"的断言)。
4. **持久回滚兜底**:`versions` 归档 + `skill_rollback` 提供跨会话、带版本历史的 durable 回滚;P1 的运行时 LIFO 回滚是它的活体补充,两者不冲突。

**已落地**:`revertible::EvolveEffect`/`EffectTracker`(P1)、`session::revert_key` + `Event` 的 `revert_key`/`revertible` 字段(P3)、`versions` 归档。
**已证**:`revertible::`(6 个框架测试,含 LIFO 组合)、`p2_`/`p3_` 系列、4 个写动作正逆往返测试。

---

## 3. Spatial Composability(空间可组合)

**定理**:工具 A 的进化不影响工具 B 的注册——组件间依赖可声明、可自动解析,一个组件的动态加载/卸载只波及它的依赖闭包。

**sigil 检查判据**:
1. **默认空间隔离**:每个工具的 `dependencies()` 默认空 → **永远就绪**。一个工具注册/注销只影响**声明了依赖它的**那批工具,其余工具的注册与就绪状态不变(`register` 幂等、`order` 不重复)。
2. **依赖可声明、可解析**:工具可显式声明依赖(Lua 工具 `-- requires: a, b`;原生工具覆写 `dependencies()`),registry 运行时解析并维护就绪位。
3. **反应式传播有界**:依赖注册/注销只触发**受影响闭包**的就绪重算(`recompute_readiness` 返回 `ReadinessDelta`,精确列出 became_ready / became_blocked),不波及无关节点。
4. **可内省**:`tool_dependencies` 工具能查全图、查某工具的依赖 + 反向边("谁依赖它,删了它会伤谁")。

**已落地**:`Tool::dependencies()`(默认空)、`DependencyState` + 反向依赖图、`recompute_readiness()` 反应式 delta、`tool_dependencies` 内省工具(P2)。
**已证**:`p2_register_unblocks_dependent_tool`、`p2_unregister_blocks_dependent_tool`、`p2_transitive_readiness`、`p2_delta_reports_became_ready`、`p2_tool_dependencies_introspection`、既有 `re_register_same_name_does_not_duplicate_order`。

---

## 4. Progress(进展)

**定理**:进化操作不会死锁或无限循环——系统总能推进。

**sigil 检查判据**:
1. **依赖环 = 永不就绪,但不挂起**:就绪计算用带环检测的 DFS(0=未访问/1=进行中/2=完成)。遇到环,环上节点判定为 `false`(永不满足),**立即返回**,不无限递归。两个互依赖的工具都标记 BLOCKED,`execute` 友好降级(报缺失依赖、不执行),agent 循环继续推进。
2. **步数上界**:`max_steps` 限制单 turn 的模型请求数,防进化后循环不退。
3. **熔断器**:反复失败的工具被 circuit breaker 跳过,防单点拖死整条循环。

**已落地**:`recompute_readiness` 的环检测 DFS、`max_steps`、safety circuit breaker。
**已证**:`p2_cycle_is_never_ready`(环不死锁、双 BLOCKED)、`p2_execute_gates_blocked_tool`(阻塞工具降级不挂起)。

---

## 5. Confluence(合流)

**定理**:并行进化在有冲突时可收敛——两个操作以任意顺序应用,最终状态一致。

**sigil 检查判据**:
1. **效应栈 LIFO 唯一序**:同一进程内的可逆效应进同一个 `EffectTracker` 栈,回滚顺序唯一(LIFO),不存在"两个逆操作竞争"。
2. **热注册单点排空**:`hot_queue` 由 agent 循环每 turn 末 `drain_hot()` 单线程排空,避免并发注册竞争;`with_dir_lock` 对文件级写入加锁,跨进程也收敛。
3. **hook 分发确定性顺序**:P0 的多监听器按**文件名排序**执行(目录优先于单文件),emit/waterfall/parallel/serial 各自语义确定,并发(`parallel`)结果按序保留——并行 hook 的输出可复现。
4. **依赖就绪确定性**:`recompute_readiness` 纯函数式重算,同一注册集合 → 同一就绪态,与到达顺序无关(合流)。

**已落地**:P0 四模式分发 + 文件名确定性排序、P1 LIFO 效应栈、P2 纯函数式就绪重算、hot-queue 单点排空、`with_dir_lock`。
**已证**:`hooks::`(4 分发模式确定性,15 个)、`p2_delta_reports_became_ready`(纯重算幂等)、`revertible::` LIFO 组合测试、既有 `re_register_same_name_does_not_duplicate_order`。

---

## 6. 使用方式(给后续进化改动的检查清单)

新增/修改任何"能改 harness 或工具集"的代码时,过一遍:

- [ ] **Preservation**:是否在校验输入契约(schema/hook 函数签名)?非法产物是否被净化/拒绝?
- [ ] **Temporal**:这次写入是否产生了可逆效应(若属 `improve_harness` 类)?会话日志能否定位/回滚这一帧?
- [ ] **Spatial**:是否意外引入了对其他工具的隐式依赖?`dependencies()` 声明了吗?`register` 是否仍不扰动无关节点?
- [ ] **Progress**:新增的依赖/等待会不会形成环或挂起?环是否被判为"不就绪但不死锁"?
- [ ] **Confluence**:并发路径(hot-queue / hook 并发 / 文件锁)是否仍收敛到唯一态?

任何一条答不上来 → 先补机制/测试,再合入。

> 维护:本文件随 P0–P4 演进。新增机制时,在对应定理节追加"已落地/已证";发现机制违反定理时,修机制并把案例记入该节。
