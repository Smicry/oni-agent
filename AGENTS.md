# AGENTS.md — oni-agent 工程规则

本文件是 agent 在此仓库工作的**生效规则**，只写"跨存档成立"的约束。
历史事故、局内坐标、人名等过程记录放在本地 `records/`（不进开源仓库）。

## 0. 目标与实现原则（一切改动的出发点，先读这一节）

### 0.1 最终目标

**一个能长期自主运营《缺氧》殖民地的类智驾系统：**
有指令 → 按指令执行到完成；无指令 → 按自身判断持续让殖民地变好；
全程世界默认运行，不靠人盯。

### 0.2 分层职责（每一层只做一件事，层间只传结构化数据）

| 智驾对照 | 本系统 | 唯一职责 | 绝不做 |
|---|---|---|---|
| 传感器 | MCP 读数（sense / reflex::world_paused） | 给真值 | 不解释、不缓存关键状态 |
| AEB/ESC | 安全壳（reflex.rs） | 红线反射：人不能死 | 不规划、不猜意图 |
| 导航 | 滚动窗口（window.rs） | 目的地 + 3 步轨迹，**轨迹不可行就重算** | 不执行 |
| 大脑 | 慢系统 LLM（slow.rs） | 理解世界、决定去向、处理长尾 | 不手算坐标、不直接下原始工单 |
| 线控 | 执行器（exec.rs / autoact.rs） | 把一步变成工单；**能力不足自己补**（如先铺梯子）；如实回报 | 不判断该不该做 |
| 方向盘 | 指令通道（commands.rs） | 人说去哪就去哪；**做完要回执** | — |

### 0.3 每条改动必须回答的三个问题（答不上来就不做）

1. **它属于哪一层？** 说不清 = 不做。
2. **它让这一层做好了自己的事，还是替另一层做事？** 替别人做事 = 不做。
3. **它有没有新加 if-else 去猜 LLM 或游戏的意图？**（字符串匹配 goal、
   用中文关键词找节点、按文案判状态）有 = 不做。

### 0.4 已验证的反模式（每一条都咬过我们，不要再走回去）

| 反模式 | 事故 | 正解 |
|---|---|---|
| 用 tag.contains("厕所") 把意图翻译成节点 | LLM 把厕所写成 expand 就匹配不上 | LLM 直接给结构化 goal，执行器不解析文本 |
| 判据字段只认一种写法 | LLM 写 {key,n} 代码认 {element,kg} → 节点永不 done | 契约层做别名容错（crit_num/crit_str） |
| 执行结果硬编码 ok | 零进展记成功 → 熔断永不触发 → 无限乒乓 | aggregate_ok 真实汇总 |
| 用缓存快照判"世界在跑" | 游戏停着却下工单 → 误熔断 | 只问传感器，colony/status 1s TTL |
| 连续失败就换目标 | research/scout 来回跳，没方向持续推进 | 换**手段**（绕障/精确工单），不换目标 |
| 大脑直接给线控发原始命令 | $ oni build X --apply 无坐标静默失败 | commands 只收带坐标的精确工单，其余拒绝并回执 |
| 静态长链 plan | 世界一变整条链锁死 | 滚动窗口，不可行就重算 |

### 0.5 工作方式

- 改代码前 oni hold，改完 oni release；**必须 cargo build --release**（cargo test 不重建 bin）。
- 每个 fix 配一个回归测试，测试名说清它锁住哪个不变量。
- 文档与代码不符时改文档；契约（FAST_SLOW_CONTRACT）与代码不符时改代码。
- 汇报时先说"这属于哪一层、补的是哪层的能力"，再说细节。

## 1. 唯一入口

```bash
cargo build --release
./target/release/oni serve       # ★ 主力入口：一条命令同时起快系统 + 慢系统 + Web 控制台(:8789)
./target/release/oni observe     # 全局态势卡（一切判断的起点）
./target/release/oni plan        # 本轮唯一目标
./target/release/oni act         # 执行（预检 → dry-run → apply）
./target/release/oni preflight dig x1,y1,x2,y2
./target/release/oni preflight build x,y Prefab
./target/release/oni probe x1,y1,x2,y2
./target/release/oni cells x1,y1,x2,y2   # 模型判定 vs 实时元素并排诊断
./target/release/oni reach              # 打印世界模型可达集
./target/release/oni graph [--full]
./target/release/oni scout          # 开局资源侦察（落盘 scout.json）
./target/release/oni plan-reset    # 重开规划：清旧计划 + 同步生成新开局战略
./target/release/oni plan-llm      # 手动触发 LLM 开局规划（采事实→生成→校验→落盘）
./target/release/oni plan-check     # 依赖图体检：假 done / 判据复核
./target/release/oni window <秒> / pause / resume / hold / release
./target/release/oni escalations / bus timeline / status
./target/release/oni raw 'oni://<资源>'  # 直接读 mod 资源（排查用）
```

其它常用：`doctor`（连通性自检）、`status`（慢系统态势，零 MCP）、`escalations`/`resolve`（卡点上报与解决）、
`decision`（慢系统决策留痕）、`policy`（选址策略参数）、`restart`（回滚存档+重置计划）、`agent`/`slow`（单独起快/慢系统）。
完整列表见 `oni help`。

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
- 全员空闲 → **只上报慢系统，世界继续跑**（不冻结）；窗口到期按"有活续窗 / 空闲冻回"。
- **动作失败且慢系统无待办** → 冻回等介入（不自持）：这是终态，不是循环。
  但只要慢系统还有待办（ai_goals_pending / escalation），失败也只冻 20s 后自持重试。

### 7.1 世界状态必须用实时真值判定（2026-11-23 实测事故）

**判据：拿过期快照判"世界在不在跑" → 代理以为在跑 → 继续下工单 → 游戏实际停着 →
工单永不执行 → 指纹不变 → 判零进展 → 误熔断好节点（契约 §9 铁律被绕过）。**

- 判定世界在不在跑**不能只信快照的 `paused`**（快照是批量采集的，采集与判定之间有秒级间隔）。
- 判据用**三重信号**，任一为停即判停：`isPaused` / `gameSpeed <= 0`（速度 0 等于没跑）/ 实时复核。
- `oni://colony/status` 走 **1s TTL** 的秒级新鲜度（`cache.rs` 的 per-URI 覆盖），
  其余观察源 8s。**关键状态不绕过缓存**——同一套 singleflight 合并并发读，既新鲜又不打爆端点。
- 世界判停时：**不下工单、不计进展、不熔断**，只观察（快系统 `can_execute=false`）。

## 8. 依赖图（plan.json）

**AI 拥有战略决策**：plan.json 带 `author=ai` + `locked=true` 时，机械规划器**永不覆盖**
（机械 planner 与模板 playbook 已删除，见 §11），重规划建议只写入 `suggestions.jsonl` 等 AI 审阅。
AI 不在时宁可停摆，也不让 if-else 规则越权做战略判断。
慢系统决策用 `oni decision add/list/resolve` 留痕（依据+证据+预期+回填）。

- 节点有 requires / unlocks / actions / success_check / status / priority。
- decide() 先比 priority 再比 unlocks，平局取文件靠前（文件顺序=优先级）。
- status: todo / in_progress / done / blocked / deferred；熔断只用于有替代方案的节点，保命节点永不熔断。
- **保命节点禁熔断 ≠ 每轮都重试**：保命节点连续失败达阈值 → 上报慢系统并**暂缓**（deferred，
  有冷却、条件变化后自动恢复）。只"本轮停手"会被下一轮重新选中 → 恢复→失败→冻回无限乒乓。
- 判据用终局指标（氧气/食物/人口/可达格），不用中间产物存量；绝对值判据在新档开局常已满足 → 用续航。
- 计划文件不放硬编码坐标；用语义目标（surface / open_chamber / element:X / material / mop_dirty）。
- 「能不能造」要过三关：材料 + 研究锁（buildability）+ 运转条件（电/水）；只过第一关会"指令 ok 但没效果"。

## 9. 工程纪律

- **失败必须诚实上报**（2026-11-23 实测事故）：动作汇总不能硬编码成功。
  `execute()` 的 `ok` 必须由各结果真实汇总（任一失败 → 整体失败）。
  否则熔断统计读到的永远是"成功" → 连续失败计数不增长 → 熔断永不触发 →
  慢系统无限重复入队推不动的目标 → 快系统恢复→失败→冻回无限乒乓。
  零进展（无可下单曲 / 推不动 / 零进展）必须归类为**失败**（`fail_kind=no_progress`），
  熔断统计还要能从 detail 自行识别（不信历史脏数据里的 ok 字段）。
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

**架构总纲见 `docs/architecture-v3.md`（LLM 驾驶 / 滚动窗口 / 安全壳三层）；完整契约见 `docs/FAST_SLOW_CONTRACT.md`（唯一权威，代码与之不符时改代码）。**

核心：**慢系统决定"往哪走"，快系统决定"现在怎么活下来"；慢系统从不阻塞快系统。**
- 快系统每轮必做保命动作（救援/建厕所/产氧/换气/清污），`needs_ai` 只上报 AI 做战略参考、**绝不停手/冻结**。
- AI 开启时世界默认运行；暂停只允许：hold（用户关总开关）、紧急态占空比（冻20s→跑120s）、施工窗口到期自持。
- "等慢系统"不是暂停理由；`escalation_hold` 已废除。急症只跳"扩张类"goal，保命/续航 goal 照常执行。
- AI 慢系统通过 `oni status/escalations/decision` 读局面，改 locked plan / ai_goals，`oni decision` 留痕。
- **开局战略由慢系统 LLM 生成**（opening.rs：快系统采事实写 opening_brief.json，慢系统 LLM 依本局事实出节点图，强校验后落盘 author=ai）。
  **无本地兜底**：LLM 失败即空窗重试（10 分钟冷却），快系统保命层不受影响；
  硬编码 playbook（模板节点图/机械策略族）已删除。
  **禁止把某张地图的开局套路固化进模板/代码**——水世界、金属星、沼泽的节点图必须由 LLM 依事实重排。
