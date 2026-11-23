---
name: oni-colony-ops
description: |
  接管并长期运营《缺氧》(Oxygen Not Included) 殖民地。Use when the user asks to take over the colony, check on colony status, re-plan the build order, diagnose why the colony is stuck, or when a new user message arrives during an active run. Enforces the freeze-observe-replan-execute-verify loop so the world never keeps running while the agent thinks — freezing is explicit (`oni hold` / `oni release`); there is no automatic prompt hook. Refuses to guess map facts instead of reading them.
  触发场景：接手这局、殖民地怎么了、重新安排计划、为什么卡住了、代理停摆了、新消息到达时重规划。
descriptions:
  zh-Hans: "接管并长期运营《缺氧》殖民地：冻结→观察→重规划→执行→验证循环；冻结靠显式 `oni hold` / `oni release`（没有自动暂停 hook）；不猜地图事实，一切以官方读数为准。"
---

# ONI 殖民地接管与运营

## 0. 为什么必须先冻结

主 agent 的思考要几分钟，**游戏不会等你**。世界在你思考期间继续跑 =
复制人在你读代码时窒息或饿死。

**每轮的第一件事是冻结世界，不是观察世界。**

- **当前没有自动冻结**：`UserPromptSubmit` hook **未注册**（`.minimax/hooks.json` 的 `hooks` 是空映射；
  `scripts/pause-on-prompt.mjs` 只是保留的参考实现，无人引用）。
  用户发来新消息**不会**自动 `oni pause` —— 要冻结必须**显式** `oni hold "理由"`。
- 用户**手动**暂停（owner=user）**不会**让代理停手：AI 开启时，除 `hold` 外的所有暂停
  （owner = agent / none / user / escalation）代理都会**自主恢复**并开施工窗口
  （`agent.rs` 的统一恢复分支）。**唯一受保护的停止信号是 `oni hold`。**

## 1. 一轮的标准循环

```
FREEZE → OBSERVE(全局) → JUDGE → REPLAN → EXEC(预检→dry-run→apply) → VERIFY → RESUME(有限窗口) → RECORD
```

### FREEZE
- `oni hold "理由"`（写 hold 标记 + 暂停游戏；幂等）。
  **别用 `oni pause`** —— pause 的 owner 是 User，会被代理**自动恢复**，冻不住。
- 冻结只是为了拿到**静止快照**。冻结期间不思考下一步动作。
- 看完/改完 → `oni release` 解除 hold；要施工再 `oni window <秒>`。

### OBSERVE —— 一次采全，禁止只看单一数据源
```bash
oni observe                 # 全局态势卡 + 账本 + 趋势（一切判断的起点）
oni graph --full            # 气室连通图：哪个气室快没气了 / 到地表要挖几格
oni reach                   # 复制人可达域（校验世界模型）
oni probe x1,y1,x2,y2       # 逐格元素/实心/质量/建筑/标记
oni preflight dig  x1,y1,x2,y2    # 挖掘工位可达性
oni preflight build x,y Prefab    # 占格/支撑/落脚点/材料
oni raw 'oni://buildings/materials?prefabId=X'   #  这个建筑当前能用哪些材料
```

**读数优先级（官方真值 > 自研推断）**：
可达区用 `/active/dupes/reachability.md` 的 samples；空闲用 `step_facts.workIdleCount`；
电力读 `oni://power/summary` 的 **summary 子对象**；材料量读 `oni://resources/inventory` 的**真实散装量**；
建筑占格读 `/active/buildings/index.md`。**不要从渲染字形猜建筑或元素。**

### JUDGE —— 输出判定，不是建议
- 空气：看**复制人呼吸值**（< 35 是危险），不是全局气体比例。
  CO2 比氧气重会沉在坑底，全局比例低但人人呼吸 100 = 不危险，只是该换气。
- 人口为 0 → 完全停手（别在死掉的地图上刷工单）。
- 食物/氧气按observed rate算还能撑几周期，而不是看瞬时值。

### REPLAN —— 动作必须对应"当前缺什么"
- 资源类节点用**多条件判据**（`loose_multi_min`）：煤+铜+土**同时**达标才放行下游。
- 引擎语义：`requires` 是**硬依赖**，`unlocks` 只是**优先级权重**（引擎取其长度）。
  `unlocks=[]` 的节点会永远排最后 —— 给关键节点配上 unlocks。
- 判据必须**可达且可能达成**：读错层级 / 建筑未解锁 / 阈值不可能达到 → 节点永远不 done →
  下游永不解锁 → 整张依赖图卡死。**排计划前先验证判据能变 true。**
- 换气完成的判据是 `surface_open`（复制人能走到地表格），**不是**"挖到煤"。
- 建造动作要**幂等**：发电机已建成时，下一个动作是**送煤**而不是再"建一个"。
- 判据用错了 → 依赖图提前放行 → 下游缺料 → 循环卡死。

### EXEC —— 预检不过就不执行
1. `oni preflight ...` 不过 → 换方案，绝不绕过。
2. dry-run → apply（地图补丁用 `.*` 通配符，复制人移动不会让补丁失配）。
3. **一次只下一条工单**（游戏变更非事务性）。

### VERIFY —— 复读，不靠日志自证
```bash
oni probe x1,y1,x2,y2        # 标记/建筑真的落地了吗
oni call world_editor '{"command":"read","path":"/active/buildings/index.md"}'  # 建筑实例真的建成了吗
```
日志说 `ok: true` **不等于**动作生效。必须复读目标区域比对。

### RESUME —— 有限窗口
`oni release`（解除 hold）后 `oni window <秒>`（如 120–300s），到点自动冻回。
`oni window` 本身也是"授权代理恢复"的动作（会清掉 user/hook 的暂停所有权）。
不要无限期恢复：暂停只冻结倒计时，不会让人好转。

## 2. 必须记住的硬约束

| 约束 | 后果 |
|---|---|
| 开局地上**零建材** | 梯子/砖块/发电机全部造不了（`phase: failed`）→ **第一步是采建材**，不是挖矿 |
| 建造材料必须问 mod | `oni://buildings/materials?prefabId=X` 按地上实际库存给出可用材料 |
| 梯子 1 格 1 格往上 | `ladder_step()` 先定该列**基座**（复制人真走得到的那格），再从**基座+1** 起补**第一处缺梯子的格**，逐格向上铺成连续链；**不是固定 `(x,y-2)` 落点** |
| 梯子约 100kg/格 | 开长竖井前先算材料；**横着采通常比竖着挖省** |
| 建筑占格以 **mod 目录**为准 | 本仓库不硬编码尺寸（从目录取 width/height）→ `oni doctor` 会打印各建筑 WxH，别照抄记忆里的尺寸 |
| 直挖深井会困死人 | 往下只走斜向阶梯；往上靠梯子列 |
| 挖过的地方留碎岩 | 必须先 `扫` 清障，否则工单被立刻取消 |
| 一次只能下一条工单 | 游戏变更非事务性 |
| 保命节点永不熔断 | 它没有替代方案，熔断=自杀 |
| 熔断后要复活整条链 | 只复活选中的下游没用，前置仍 blocked → 死锁 |
| 复制人睡觉时不会挖矿 | 看着像卡住其实是作息；判空跑要看有无待办工单 |
| **建筑可能锁在研究后** | `autoMaterialReason: building_locked_by_research` → 这个建筑**根本造不出来**，别排在计划后面（先建研究站） |
| **造出来 ≠ 有人吃** | 复制人有食谱白名单；农业站种的东西若不在 `/active/management/food.md` 允许列表，要先开 `food_policy` |
| 电力字段在 **summary 子对象** | 读顶层永远拿不到 → `power_operational` 恒假 |

## 3. 常见故障速查

| 症状 | 多半是 | 处理 |
|---|---|---|
| `phase: failed` + `reportedActions: 0` | 材料不在该建筑的合法类别 | 查 mod 材料查询；先采建材 |
| `Unsupported map cell transitions` | token 格式错 | `PrefabID:优先级#MaterialID`，优先级 1-9，无材料时**省略整个 `#` 段** |
| 指令 `ok:true` 但没人干活 | 工位不可达（落脚点不在官方可达区） | `oni preflight dig` 看落脚点 |
| 每轮重复同一条命令 | 目标格已有标记 | 先复读标记状态；换个目标 |
| "依赖图里没有可执行目标"刷屏 | 整条链被熔断 | 复活所有 blocked 节点 |
| 进度指纹不变 | 真卡住 | **换方案，不是加轮次** |
| 节点永远不 done、下游全不解锁 | 该节点 `success_check` **恒假** | 查判据层级 / 建筑是否解锁 / 阈值是否可达 |

## 4. 不要做的事

- 不要手写 MCP 调用或 SEARCH/REPLACE 补丁 —— 用 `oni` CLI，它封装了预检、通配符补丁与回写。
- 不要用 `oni pause` 代替 `oni hold` 冻结世界 —— pause 会被代理自动恢复，等于没冻。
- 不要在没读格子的前提下断言"这里有矿/这里是空"。
- 不要因为"指令返回 ok"就认为做成了 —— 必须复读验证。
- 不要在人口为 0 时继续下工单。
- 不要把存档专属坐标写进通用规则（知识库只放跨存档成立的东西）。

## 5. 记录与沉淀

每次失败 → 提炼成**可执行规则**（不是事故流水账）：
- 通用的游戏机制 → `docs/knowledge-base.md`
- 场景处置流程 → `docs/playbook.md`
- 工程硬规则（坐标、真值优先级、熔断纪律）→ `AGENTS.md`
- 事故细节 → `records/`（**不进知识库**）

## 6. 冻结纪律（ 别让世界在你眼前空跑）

主 agent 思考/改代码要几分钟，游戏不会等你 —— 世界在你看不见的时候变化，
等于丢失了观察权。Keep the world frozen while reasoning; otherwise the colony changes unseen.

```bash
oni hold "改 agent.rs：修复 auto_ladder"   # 开始改代码前：暂停 + 写 hold 标记
oni release                                  # 改完解除
oni window 120                               # 需要施工时开一个有上限的窗口
oni pause / oni resume                       # 手动控制（pause 的 owner=User，
                                             # 会被代理自动恢复；要真停住用 hold）
```

| 场景 | 动作 |
|---|---|
| 用户发来新消息 | **当前无自动冻结**（UserPromptSubmit hook 未注册）→ 需要冻结就显式 `oni hold "原因"` |
| 你要改代码/深挖问题 | `oni hold "原因"` —— 游戏冻结，代理只观察不自动恢复 |
| 用户手动按了暂停 | 代理**仍会自主恢复**（owner=user 也在统一恢复分支内）→ 想让它停住只能用 `oni hold` |
| 代理没有待办工单（全员空闲） | 只**上报慢系统**，世界**继续跑**（不冻结）→ 别指望它自己停 |
| 代理动作失败 | 慢系统**无待办** → 冻回暂停等人（终态）；慢系统**有待办** → 冻 20s 后自持重试 |
| 有人呼吸 < 35 | **例外**：占空比（冻 20s → 跑 120s），暂停救不了窒息的人 |

> hold 期间**不要**执行写操作 —— 世界冻结着，行动要等 `oni release` 之后。

## 7. 实时优先级调度（ 代理看不见"人"就一定会死人）

Design conclusion: 代理观察的是材料/建筑/气体总量，**没有"人现在怎么样"**。
等计划反应过来时，人已经没了。所以调度必须**实时**：每轮重新评估，能抢占静态计划。

| Level | 触发 | 动作 |
|---|---|---|
| 3 救命 | 有人失去行动能力 | 救援：排毒 / 拖到有空气处 / 治疗 |
| 2 紧急 | 有人呼吸 < 50；或**人在不可呼吸格**（污染水/CO₂/氯气）**且呼吸 < 95**（读不到呼吸值时保守判危险）；或膀胱 > 80% 且厕所数 < 人口 | 先把人弄到能呼吸的地方 / 立刻建厕所 |
| 2 紧急 | 氧气续航 < 3 周期 | 产氧优先 + **禁止扩张性挖掘**（见 §10） |
| 2 紧急 | 氧气消耗几乎归零但人还在 / 氧气完全冻结且呼吸或胁迫异常 | 立刻查人、准备救援 |
| 1 关注 | 人在不可呼吸格但呼吸 ≥ 95 / stress 飙升 / 空气比例低 / 食物续航 < 0.5 周期 | 对应计划节点（不抢占） |
| 0 正常 | 无异常 | 按 plan 依赖图推进 |

**早期预警（比"呼吸值=0"早几分钟）**

- **氧气消耗速率归零** = 人不再呼吸 = 已经不行了。
- **stress 飙升** = 有人在受苦。

```bash
oni observe            # 全局态势卡（周期/气体/电力/建筑/趋势）
oni observe --cached   # 零 MCP 压力地看同一份快照
tail -f ~/.oni-agent/agent.log   # 📊 调度[Level] 在守护进程日志里，**不在**态势卡里
```

>  **读不到数据 ≠ 数据是极值。** 快照里读不到的字段是 0，
> 直接比阈值会把"没读到"当成"耗尽了"。判定前先确认数据源可信。

## 8. 语义化动作：同一份计划跨存档复用

plan.json 里**不允许出现硬编码坐标**。动作只说**意图**，坐标由图模块运行时算：

| goal | 含义 | 运行时怎么算 |
|---|---|---|
| `surface` | 挖通到地表 | 图模块 0-1 BFS 最短路的**第一个要挖的格**（不是终点） |
| `material`（采建材） | 挖到任意一种合法建材（梯子/砖块的材料） | 可达区 bbox 外扩 3 格圈成目标区；挖掘管线选前沿格时优先挑砂岩/泥土/藻类 |
| `element:Algae` | 挖到含某元素的格 | 以可达区为中心逐环搜索最近目标 |
| `mop_dirty` | 排掉污染水/污染氧 | 扫地图 token（`染`/`污`） |
| `auto_build` + `region:"auto"` | 自动找建造落点 | 以复制人位置为中心的可达区矩形 |

自检：`grep -E '"(rect|region|anchor)": "[0-9]+,' plan.json` 应输出 0 条。

>  语义动作每轮只推进**一格** —— 一次挖多格会抽走支撑，把阶梯挖成无地板竖井。

## 9. 执行闸门与熔断纪律（ 代理最容易把自己焊死的地方）

守护进程每轮的**顺序就是规则**：

1. 采快照（只读，永远允许）
2. **判运行状态**：世界在不在跑？
3. 世界不跑 → **只观察直接返回**（不下工单、不计进展、不熔断）
4. 决策（实时调度可抢占静态计划）→ 执行
5. 执行**之后**再判空闲 → 没活才冻回暂停

| 硬规则 | 原因(verified) |
|---|---|
| 世界不跑 ⇒ 不下工单、不计进展、不熔断 | 暂停期下的工单永不执行 → 指纹不变 → 3 轮后**误熔断好节点** |
| **先执行、再判空闲** | 执行前用上一轮的 pending_orders 判空闲 → 刚下的第一张工单当场被冻死 |
| 语义动作只圈目标区，挖掘走**统一管线** | 语义动作自己拼 raw 工单 → 死工单 + 重复对同一格下单 |
| 趋势/异常判定只用「世界跑过」的样本 | 暂停期氧气纹丝不动 → 误判「人不再呼吸」→ 劫持整个计划 |
| stress 用**相对基线的升幅**，不用固定绝对值 | maxStress 是 0–100 百分比，开局基线就有 4.4~4.7（完全正常） |
| 梯子从复制人站得到的那格**连续**往上铺 | 梯子修在够不到的格子上 → 白修 → 竖井永远挖不动 |
| 缺梯子材料/梯子落地失败 **不能阻断横向挖** | 梯子是「往上挖」的手段，不是「挖建材」的前提 |

看运行状态与调度结论：

```bash
oni observe --cached   # 零 MCP：全局态势卡
tail -f ~/.oni-agent/agent.log   # 📊 调度[Level] / 🔒 / ⏸ / ⛔ 都在这里
```

>  日志里的 **📊 调度[Level]** 是这一轮真正要做的事（Level 取值 = Rescue/Urgent/Watch/Normal）；
> **🔒** 冻结或冻回、**⏸** 只观察不执行、**⛔** 熔断或恢复被拒。
> 「❌ dig → …无可下单曲 / 零进展 / 推不动」= 这一列推不动 → 换列，不是加轮次
> （零进展归类见 `agent.rs` 的 `fail_kind=no_progress`）。

## 10. Hard rule: 缺氧时禁止扩张性挖掘

氧气续航 < 3 周期 → **产氧优先，禁止挖地表/开拓矿层**（挖开会暴露地下 CO₂，
缺氧时是负收益，can turn a healthy run into a death spiral）。

* 调度器：`oxygen_cycles_left < 3` → `ProduceOxygen` + `forbid_expansion`
* 抢占匹配：`forbid_expansion` 时跳过换气/地表/挖矿节点
* 效果判据用**终局指标**（氧气/距离/食物），不是中间量（锁位置/工单数）
* 连续恶化 3 次 = 停手换方案，不在同一方向改第 4 次
* 单局细节用 `records/session-*.md` 记录，不进 memory 工具

## 11. 每局开始：先观察，再推进

不依赖旧局的具体状态 —— 旧坐标/目标锁/进度换局即失效。

1. 看当前存档身份：`oni state`（打印存档名、世界、**指纹**、局内状态目录、plan.json 指纹）。
2. **存档指纹 ≠ plan.json 指纹 = 换过局**（或 plan 没跟着重置）→
   `oni plan-reset`（重置 plan + 清该存档的局内状态 goal_lock.json 等）。
   ~~`~/.oni-agent/current-session`~~ 已废弃 —— 现在是 `~/.oni-agent/current.json` 的 `fingerprint`；
   `./.minimax/scripts/session-id.sh` 算的是**另一个**哈希（初始存档 sha256 前 16 位），
   **不要**拿它跟 current.json 的指纹比。
3. **观察分析**：`oni observe --cached` 拉全局态势 + `oni plan` 看依赖图 + 目视关键地形
4. 基于**当前局面**选第一个目标（通常是产氧/采建材），不继承旧局计划

跨局保留的只有规则（AGENTS.md 硬规则），不是状态。每局重新观察是硬要求。


## 12. Opening scout

**先知道资源在哪，再决定挖哪里。** 不知道铁矿在 20 格外就深挖 = 全灭。

```bash
oni scout          # 探测关键资源位置+距离，落盘 saves/<存档指纹>/scout.json（按存档隔离）
oni plan-reset     # 换存档时重置 plan（写 author=pending_llm 占位）+ 清该存档的局内状态，
                   # 随后**同步**调 LLM 生成开局战略（**没有本地模板兜底**）
```

开局依赖链（现在由慢系统 LLM 依**本局事实**生成，下面的顺序只是典型形态，不是硬编码模板）：

```
侦察 → 建材 → 产氧(氧石) → 换气(向上到地表) → 深挖产氧链(铁矿) → 食物 → 居住 → 供电 → 新人 → 火箭
```

 换气排在深挖之前：挖 20 格会把地下 CO2 释放到低层走廊（CO2 沉底）→ colony loss.
 换存档必须先 `oni plan-reset`：plan.json 带着上一局的指纹与 done 状态，新局不会自动沿用。
