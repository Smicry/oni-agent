# AGENTS.md — oni-agent 工程规则

本文件是 agent 在此仓库工作的**生效规则**，只写"跨存档成立"的约束。
历史事故、局内坐标、人名等过程记录放在本地 `records/`（不进开源仓库）。

## 1. 唯一入口

```bash
cargo build --release
./target/release/oni observe     # 全局态势卡（一切判断的起点）
./target/release/oni plan        # 本轮唯一目标
./target/release/oni act         # 执行（预检 → dry-run → apply）
./target/release/oni preflight dig x1,y1,x2,y2
./target/release/oni preflight build x,y Prefab
./target/release/oni probe x1,y1,x2,y2
./target/release/oni graph [--full]
./target/release/oni window <秒> / pause / resume / hold / release
./target/release/oni escalations / bus timeline / status
```
不手写 MCP 调用、不手写 SEARCH/REPLACE；引擎封装了预检与回写。

## 2. 坐标系

- Y 轴向上递增；锚点 = 左下角（Y 最小），建筑向上占 (x..x+w-1, y..y+h-1)。
- 支撑必须在 y-1；多格宽建筑要整条底边都有支撑。
- 尺寸：床 2×2 / 厕所 2×3 / 砖 1×1 / 梯 1×1 / 发电机 2×2 / 存储 1×2 / 农业站 2×3 / 打印舱 4×4。

## 3. 官方事实优先

自研判定一律让位给游戏真值：
- 可达性：只取 `/active/dupes/reachability.md` 中 json 代码块的 JSON；
- 空闲/待办：`game_control step_facts` 的 `nativeIdleCount` / `remainingTrackedOrders`；
- 电力/燃料：`oni://power/summary` 的 `summary` 子对象；
- 材料：`oni://resources/inventory` 真实散装量 + `oni://buildings/defs`（含 includeUnavailable 全集）；
- 建筑占格：`/active/buildings/index.md` + cell 文件，不用渲染字形；
- 元素：看 cell 的 `phase=` / `solid=`，不看字形；`Carbon` 是煤炭。

## 3.5 世界模型必须与游戏对齐（否则下的全是死工单）

**判据：自造模型比游戏宽松 → 派工过去没人能执行 → 全员闲 + 工单积压 = 看起来"没进展"。**

两条硬规则（2026-11-23 实测确证）：

1. **可站性 = 可通行 ∧ 紧邻下一格实心**（不是"下方 N 格内有地板"）。
   官方可达集（`/active/dupes/reachability.md` 的 samples）与该判据逐格吻合。
   放宽后 → 悬空格被当成能站 → 挖掘预检给悬空岩层选落脚点 → 死工单。
2. **建筑占格用实时 cell 的真实 anchor（anchorX/anchorY）**，不要用索引 Position 猜。
   猜锚点会在气体格里造出"幻影建筑" → 幻影墙被当成地板 → 同样的死工单。
   幻影格只做占位保护，**不能承重**。

诊断手段：`oni cells x1,y1,x2,y2` 并排打出「模型判定」与「实时元素」；
`oni reach` 打印模型可达集；两者不一致时**以游戏为准**。

## 4. 判定占格

1. `/active/map/cell_X_Y.md` → `bottomLeft` + `footprint`
2. `/active/buildings/index.md` → 锚点
渲染网格锚点可能偏移；以 cell 文件为准，关键建筑留 1 格缓冲。

## 5. 移动规则

- 向上必须靠梯子（当前格或目标格）；向下自由但需落脚点（向下扫 8 格找地板）。
- 斜向上 = 上台阶，目标格正下方必须实。
- 挖过的地方留碎岩会挡导航 → 先扫。
- 往下挖只能走斜向阶梯（目标正下方实）；竖井靠梯子列逐格上铺。
- 梯子可悬空贴墙；砖块要贴实心格；竖直砖柱不是楼梯。

## 6. 写操作

1. 预检不过不执行；2. dry-run → apply；3. 复读受影响区域确认效果。
一次只下一条工单。地图补丁用 `.*` 通配符（网格含复制人位置，精确补丁会失配）。
建造 token = `PrefabID:优先级#MaterialID`（优先级 1–9）；无材料时省略整个 `#` 段；
材料以 `oni://buildings/materials?prefabId=X` 为准，缺材料时 mod 报 phase:failed，先采建材。

## 7. 冻结纪律

- **唯一的用户停止信号 = AI 总开关**（Web「停止 AI」= `oni hold`，落盘 owner=hold）：冻结世界 + 代理待命，重载/重启后保持，直到「启动 AI」(`oni release` + window)。
- AI 开启时，游戏的暂停/运行**完全由代理自主决定**：任何非 hold 的暂停（owner=agent/none/user）都视为可恢复，代理直接清 owner + resume + 开施工窗口，长线任务不被打断。用户不再需要单独暂停游戏。
- 主 agent 改代码前 `oni hold`，改完 `oni release`（与 AI 总开关同一机制）。
- 紧急态（有人倒地/呼吸<35）：占空比冻 20s → 跑 120s；暂停只冻结倒计时，救不了窒息的人。
- 卡住时不停世界：只本轮不下新工单，让已有工单继续做。
- 空闲即冻结（无待办且无施工）；窗口到期按"有活续窗 / 空闲冻回"。

## 8. 依赖图（plan.json）

**AI 拥有战略决策**：plan.json 带 `author=ai` + `locked=true` 时，机械规划器**永不覆盖**，
只能把重规划建议写入 `suggestions.jsonl` 等 AI 审阅；`oni plan-auto` 默认只出草稿，
`--apply` 也会被 locked 拒绝。AI 不在时宁可停摆，也不让 if-else 规则越权做战略判断。
慢系统决策用 `oni decision add/list/resolve` 留痕（依据+证据+预期+回填）。

- 节点有 requires / unlocks / actions / success_check / status / priority。
- decide() 先比 priority 再比 unlocks，平局取文件靠前（文件顺序=优先级）。
- status: todo / in_progress / done / blocked / deferred；熔断只用于有替代方案的节点，保命节点永不熔断。
- 判据用终局指标（氧气/食物/人口/可达格），不用中间产物存量；绝对值判据在新档开局常已满足 → 用续航。
- 计划文件不放硬编码坐标；用语义目标（surface / open_chamber / element:X / material / mop_dirty）。
- 「能不能造」要过三关：材料 + 研究锁（buildability）+ 运转条件（电/水）；只过第一关会"指令 ok 但没效果"。

## 9. 工程纪律

- 动作必须有**效果判据**；连续 3 轮指纹不变 → 换方案，不是加轮次。
- 读不到数据 ≠ 数据是极值：呼吸值读不到要保守判危险；人口为 0 先确认数据源没失败。
- 工单语法查 `/active/ops/orders.md`：`挖 土@(x,y):7`、`擦 @(x,y):9`；返回 ok ≠ 生效，要验证效果。
- 挖掘避人：目标格下方 1~2 格有复制人 → 排除（掉落致死）；水层/污染层算死亡格。
- 设备选址查气体元素，不只是 Kind::Gas；人要待在设备里的必须过可呼吸检查。
- 跨进程共享状态用 `last_seen()` 这类稳定源，不用进程内 `current()`。
- 失败要可见：静默 skip / 只写日志 = 看着在处理、实际没做。

## 10. 决策记录

慢系统的每次决策（改计划/精确工单/改代码）写一条结构化记录到 decisions.jsonl：
局面摘要、候选、选择、依据（判据+证据等级）、预期效果、实际效果（下轮回填）。
没有反馈闭环，规则库只会膨胀而不会变科学。

## 11. 快慢分工（最终架构）

**完整契约见 `docs/FAST_SLOW_CONTRACT.md`（唯一权威，代码与之不符时改代码）。**

核心：**慢系统决定"往哪走"，快系统决定"现在怎么活下来"；慢系统从不阻塞快系统。**
- 快系统每轮必做保命动作（救援/建厕所/产氧/换气/清污），`needs_ai` 只上报 AI 做战略参考、**绝不停手/冻结**。
- AI 开启时世界默认运行；暂停只允许：hold（用户关总开关）、紧急态占空比（冻20s→跑120s）、施工窗口到期自持。
- "等慢系统"不是暂停理由；`escalation_hold` 已废除。急症只跳"扩张类"goal，保命/续航 goal 照常执行。
- AI 慢系统通过 `oni status/escalations/decision` 读局面，改 locked plan / ai_goals，`oni decision` 留痕。
- **开局战略由慢系统 LLM 生成**（opening.rs：快系统采事实写 opening_brief.json，慢系统 LLM 依本局事实出节点图，强校验后落盘 author=ai）。
  **无本地兜底**：LLM 失败即空窗重试（10 分钟冷却），快系统保命层不受影响；
  硬编码 playbook（模板节点图/机械策略族）已删除。
  **禁止把某张地图的开局套路固化进模板/代码**——水世界、金属星、沼泽的节点图必须由 LLM 依事实重排。
