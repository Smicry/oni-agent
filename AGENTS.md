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

- 用户手动暂停 = 最高优先级，代理绝不越权恢复；暂停所有权持久化在 `saves/<fp>/control.json`。
- 恢复按 owner 校验；未知来源保守判 user；`oni window` 是用户授权代理恢复的信号。
- 主 agent 改代码前 `oni hold`，改完 `oni release`。
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

快系统只做「确定性计算 + 指标看门 + 执行」；所有需要权衡的语义决策（建厕所/产氧方向/救援落点/禁止扩张）用 `needs_ai` 标记 → 冻结世界 + escalation 上报 AI，不在本地决断。
唯一例外：倒地救援（downed）保留快系统占空比（冻20s→跑120s）——倒地不可自愈，冻结会阻断恢复。
AI 慢系统通过 `oni status/escalations/decision` 读局面，改 locked plan 决策，`oni decision` 留痕。
