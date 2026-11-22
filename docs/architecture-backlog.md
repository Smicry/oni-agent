# 架构迭代待办清单 —— 讨人厌的实验室（2026-11-22 收官）

> 目的：把这局踩出来的**尚未修复**的坑，按"能不能让代理独立跑完一局"排序，
> 供下一轮迭代直接施工。每条给出**现象 → 根因 → 建议做法 → 验收标准**。

---

## P0：阻断性 —— 不修就无法让代理独立跑完一局

### P0-1 挖掘目标的选择与推进（代理侧不可靠）

**现象**
* 代理在长竖井上于 116 / 117 / 118 / 120 四列之间横跳，梯子铺了四列，竖井始终没通。
* 人工精确指挥（固定一列 + 逐格铺梯子 + 逐格 dig）：**95 秒挖 9 格**。

**根因**
* 目标锁定是"路径上第一个未挖的实心格"，但**路径会随复制人位置/气室变化重算**。
* 梯子落点依赖 ladder_step（官方可达集 + 梯子链），而**官方可达集不反映梯子可爬的格**
  → 起步点常判错 → 每轮换一个列。

**建议做法**
1. 把"竖井"做成**独立一等任务**（Goal::Shaft），不走通用 dig 管线：
   * 选定**一条列**写入 goal_lock（含已铺梯子的最高格）；
   * 每轮只做一件事：**该列往上铺 1 格梯子** XOR **挖该列下一个实心格**；
   * 梯子落点固定为"当前最高梯子格 + 1"，不重新搜索。
2. **代理每轮只允许一种挖掘动作**，杜绝"既铺又挖"造成的中断。
3. 增加进度判据：shaft_depth（已挖格数）必须单调递增，否则熔断。

**验收**：代理连续 30 轮不换列；竖井从起点到地表一次贯通。

---

### P0-2 电网连通的正确性（"线存在"≠"线连通"）

**现象**
* auto_connect dryRun 报 reusedExisting=20 / planned=0（像"只差 0 段"），
  实际 Y=202 行有 4 个无导线的缺口，且**相邻端口方向背对背**（→ 紧邻 ←）。
* 补线后电路仍不合并：发电机 circuit 7 / 扩散器 circuit 4 / 研究站 circuit 5。

**根因**
* reusedExisting 只校验"格子上有电线建筑"，**不校验端口是否对接**。
* mod 的 auto_connect 走 continuous_manhattan_path（直线），遇到障碍会绕，
  但**绕出来的线方向可能与已有线相反**。

**建议做法**
1. 接线前后都用 world_editor command=zoom views=["power"] 逐格核对
   dirs/links/to/circuit，**不要相信 dryRun 的复用数**。
2. 写 power_connectivity()：沿导线 links 做 BFS，判断两建筑是否同 circuit；
   不通就报告断点坐标。
3. AutoConnect 执行前先做连通性检查，不通则换路径重试（上下偏移 1 格），
   而不是接受"铺了线"这个假成功。

**验收**：oni raw 'oni://power/summary' 的 consumerPowered 达到消费者总数。

---

### P0-3 建筑落点的自动搜索（含"挖空间 + 铺砖 + 建"三步）

**现象**
* 基地里 2 格高建筑**永远找不到落点**：Y=189 被占满，Y=190+ 没有地板砖。
* 人工手工"铺 4 块砖 → 建床"才成功，proxy 的 auto_build 做不到。

**根因**
* find_build_site 只在"已有砖"的区域找；Goal::Power 有四步流程但**其它建筑没有**。

**建议做法**
1. 把"找落点"抽成通用能力：find_site_with_prep(prefab) =
   找空位 → 若底边无砖则**先挖开上层** → 铺砖 → 再建。
2. 严格可呼吸判据（is_safer_for_occupied）要**内建在选点里**，不能只给发电机用。

**验收**：代理在无人工干预下建成第 2 个厕所与第 2 个发电机。

---

### P0-4 人口恢复的确定性

**现象**
* 打印舱 care package 是**随机物品**：实测连续 4 次拿到
  发光虫卵 ×3 / 氧石 ×100 / 营养棒 ×5 / 喷浮飞鱼幼崽 —— **一次都没带新人**。
* 1 人殖民地无法完成任何规模化任务。

**根因**
* mod 的 immigrantsAvailable 对应"有 care package 可领"，**不保证是新人**。

**建议做法**
1. 每轮检查 printing_pod status → immigrantsAvailable 时**优先 claim**（机会窗口）。
2. 连续 N 次没拿到新人时，**在报告里明确写"人口恢复未达成"**，不要让调度假装正常。
3. 研究是否存在"直接开移民界面并选人"的参数，而不是被动等随机包。

**验收**：30 周期内至少新增 1 名复制人，或明确报告"随机包不含新人"。

---

## P1：重要 —— 影响可持续性与效率

### P1-1 产氧原料的可持续性

* **氧石（OxyRock）是 care package 给的一次性物品**，库存里没有、地图上也没找到矿脉。
* ⇒ 扩散器吃完就停；**本局产氧路线结构性断裂**。
* **建议**：开局就确认"地图上有没有氧石矿脉"；没有的话产氧只能靠
  **电解器（需电 + 水）或藻类箱（需研究 + 肥料 + 水）** —— 都要提前排进依赖图。

### P1-2 CO₂ 的"只进不出"是全局死因

* 六次全灭的上游都是它：挖 → 放 CO₂ → 基地被淹 → 人依次死亡。
* **建议**：把"基地可呼吸气体占比"做成**挖掘的前置门控**：
  占比低于阈值时**禁止任何挖掘**（除非目标是打通排气口）。
* 同时把"挖通到天空"与"挖普通空间"分成两个动作类别，只有前者允许在 CO₂ 高时执行。

### P1-3 建筑/设备的"人会停留"语义

* 人力发电机（人必须在机器里发电）建在 CO₂ 区 → **吉恩被闷死**。
* 床铺/厕所建在 CO₂ 区 → **睡眠时窒息**。
* **建议**：所有"人会停留"的建筑，落点必须满足 is_safer_for_occupied（CO₂ < 0.05kg/格）。

### P1-4 计划节点（plan.json）的三纪律

| 纪律 | 现象 | 对策 |
|---|---|---|
| 判据与 status 必须成对 | 改了判据忘了改 status → "依赖图里没有可执行目标" | 同一次提交里一起改 |
| requires 必须指向存在的节点 | A9 曾依赖不存在的 "A8" → 节点永远阻塞 | 加校验：requires 的 id 必须存在 |
| 常驻能力节点不能有"完成"态 | rescue 节点被判据达成标 done → Urgent 找不到落点 | mark_node_done 拦截 rescue 节点 |

### P1-5 代理的执行日志要能审计

* 代理日志只打 order ok: true，**看不到实际下发的命令** → 无法判断"成功"是真成功还是静默失败。
* **建议**：order/build/dig 动作的 detail 必须进日志（含目标坐标）。

---

## P2：体验与效率

1. **一次只能下一条工单**是 mod 的硬限制 → 所有"批量下指令"的代码都要改成逐条循环。
2. **mod 建造受相机视野限制** → 任何视野外的建造前先 navigation_control set_view。
3. **mod 的工单语法必须查表**（/active/ops/orders.md 的 Edit Commands 段），
   带类型的 target（收/挖/杀/捕/拆/扫物品）**要写类型前缀**。
4. **一次工单一个目标**：批量下会被静默丢弃。
5. **mod 的 prefab 查找会模糊匹配**：Generator 会命中 DevGenerator（1×1）而不是
   煤炭发电机（3×3）→ 尺寸判断全错。**prefab id 以 building_control search_defs 为准**。

---

---

# 第二轮架构迭代（2026-11-23 · 拉德多/科斯姆星 全灭复盘）

> 上一轮（讨人厌的实验室）修的是"**动作层**能不能做对"；
> 这一轮暴露的是"**依赖图层**让不让它有机会做"——两类问题互不替代。

## P0-3 依赖图饿死：一个节点永久霸占执行位

**现象**
* serve 日志：`节点=AIR` 166 次、`节点=T1` 139 次、**`节点=P1`（供电）0 次**。
* 结果：氧气扩散器造出来了却**没通电**（`generatorCount=0`、`consumerOperational=0`、
  `netActiveWatts=-120`）→ 一点氧都不产 → 基地氧气 240kg→154kg → 三人全灭。

**根因**
* 选点排序 = (priority, unlocks 数, 文件顺序)。
  AIR 每轮都能"下几格挖掘工单"（有动作、有进展的样子），
  但**判据从未达成** → 它一直 ready → unlocks 多的它永远赢 → P1 永远排不上。

**建议做法（已落地）**
1. 节点加 `stall`（连续未达成判据的轮数）；
2. 排序键改为 **(priority, stall↓, unlocks 数)** —— stall 权重高于 unlocks；
3. stall ≥ 3 的节点本轮**完全排除**（排在后面仍可能被选中 → 饿死依旧）；
4. 全部 stall 超限时放行 stall 最小的那个（总比整轮空转强）。

**验收**：`stalled_node_yields_to_other_ready_node` / `stalled_node_at_limit_is_excluded`
/ `all_stalled_still_picks_one`。

## P0-4 假 done：依赖图带着一份自欺的完成清单

**现象**
* plan.json：E1「地表换气（无限空气，替代被锁的电解器）」status=done，
  判据 `surface_open(minY=216)`，而**实际可达上界只有 194** → 从没通过过；
* B1「床位」status=done，判据 `building_min(beds>=2)`，而 `buildings.beds = 0`。

**根因**
* `status=done` 可以**未经任何判据校验**就存在（AI 写的计划自带）。
* 快系统信任 status → 依赖图以为"换气与床位都完成了"
  → **再没有任何节点要求产氧** → 基地慢慢耗尽，而且日志看起来一切正常。

**建议做法（已落地）**
* 每轮用**同一套判据**复核所有 done 节点，不成立 → 打回 todo。
* done 只代表"曾经达成过"，不代表"现在还成立"。
* 新增 `oni plan-check` 做静态体检（列出每个节点的 status/判据/假 done 清单）。

**验收**：`done_node_with_failing_criterion_is_reopened`。

## P0-5 造出来 ≠ 在运转

**现象**：`制氧1`（氧气扩散器已建成），但 `power/summary` 显示
`consumerOperational=0` → 设备**通电量为 0** → 一点氧都不产。

**根因**：判据用 `prefab_min`（只数建筑数量）→ "造出来了"被当成"在产氧"。
另外 planner 的依赖顺序也反了：**产氧 A4 排在供电 A5 之前** → 先造耗电产氧机。

**建议做法（已落地）**
1. 新增判据 `oxygen_producing`：产氧设备在运转 **且** 有发电机；
2. planner 产氧节点改用它；
3. **供电节点排到产氧节点之前**（耗电产氧件没电就是一堆废铁）。

**验收**：契约不变量 9、10。

---

## 已完成（本轮已落地并有单测）

| 能力 | 单测 |
|---|---|
| harvest 命令强制「植物」类型前缀 | harvest_command_has_plant_type_prefix |
| 采集植物候选过滤 + 优先级 | forage_candidates_filters_and_prioritizes |
| 危险格按浓度判（两级判据） | hazard_cell_rules_by_concentration |
| 膀胱按人均厕所数抢占 | high_bladder_with_one_toilet_for_three_triggers |
| 2 周期食物不抢占主线 | moderate_food_does_not_preempt_main_plan |
| CO₂ 环境里呼吸 <95 即抢占救援 | breather_in_co2_with_falling_breath_triggers_rescue |

### 2026-11-23 追加（拉德多局）

| 能力 | 单测 |
|---|---|
| 依赖图饿死防护（stall 让位） | stalled_node_yields_to_other_ready_node |
| stall 超限节点本轮排除 | stalled_node_at_limit_is_excluded |
| 全 stall 超限仍放行一个 | all_stalled_still_picks_one |
| 假 done 节点打回 todo | done_node_with_failing_criterion_is_reopened |
| 可站性对齐游戏 navmesh | walkable_requires_immediate_floor |
| 幻影建筑格不承重 | phantom_building_is_not_a_floor |
| 竖井梯子链自下而上连续 | ladder_step_fills_chain_bottom_up |

共 **78 项测试全绿**。
