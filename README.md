# oni-agent —— 《缺氧》(Oxygen Not Included) 长期自主代理

用 **Rust** 实现的 MCP 客户端 + 循环引擎，让 AI 长期接管一局《缺氧》而不浪费游戏周期。

```
┌── oni-agent ─────────────────────────────────────────────────┐
│ crates/oni-mcp      MCP 客户端（streamable-HTTP，传输层）      │
│ crates/oni-world    世界模型 + 下单前预检（坐标/占格/工位）    │
│ crates/oni-engine   循环引擎 + 常驻守护进程（感知→…→验证）    │
│ crates/oni-cli      `oni` 命令行（二进制名仍叫 `oni`）         │
│ ~/.oni-agent/       运行状态：agent.log / open_window / hold  │
└──────────────────────────────────────────────────────────────┘
```

## 命名：为什么是 oni-agent 而不是 oni-mcp

产物是**能下发、能执行、能自己判断的代理**，`oni-mcp` 只是最底下那层传输。
名字按"层"分开，避免后来人以为这是个 SDK：

| 名字 | 指什么 |
|---|---|
| `oni-agent` | **仓库 = 整个代理**（世界模型 + 循环引擎 + 守护进程 + CLI） |
| `crates/oni-mcp` | 只负责跟游戏端点说话（MCP 客户端） |
| `oni` | 二进制名，保持短：`oni observe` / `oni agent` |
| `~/.oni-agent/` | 运行状态目录（旧 `~/.oni-mcp` 首次访问自动迁移） |

## 快速开始

```bash
cargo build --release
./target/release/oni doctor        # 连通性 / 建造目录 / 协议自检
./target/release/oni observe       # 全局态势卡 + 资产负债表 + 趋势
./target/release/oni observe --cached # 读守护进程的共享快照（零 MCP 压力，~4ms）
./target/release/oni plan          # 按依赖图选本轮唯一目标
./target/release/oni act           # 执行（预检 → dry-run → apply）
./target/release/oni preflight dig x1,y1,x2,y2      # 下单前预检：工位可达？
./target/release/oni preflight build x,y Prefab      # 占格空？支撑实心？材料合法？
./target/release/oni map x1,y1,x2,y2                 # 规划用文本地图
./target/release/oni build x,y Prefab [--apply]       # 建造落点（默认 dry-run）
./target/release/oni agent --interval 10 --window 60        # 常驻守护进程（后台循环）
./target/release/oni window 300                            # 请求 300s 施工窗口（到点自动冻回）
./target/release/oni watch                          # 常驻：危急自动暂停、解除自动恢复
./target/release/oni graph [--full]                # 气室连通图 + 空气预算 + 到地表最短挖掘路径（--full 读全图）
./target/release/oni hold "改代码中"                 # 冻结世界：暂停 + 代理只观察不自动恢复
./target/release/oni release                        # 解除 hold，让代理恢复自主循环
./target/release/oni scout                          # 开局侦察：关键资源位置+距离（落盘 scout.json）
./target/release/oni plan-reset                     # 按 plan.template.json 重置计划（换存档用）
./target/release/oni restart [存档索引]              # 一键重开：回滚存档+重置计划+清局内状态
```

## 冻结纪律（不要让世界在你眼前空跑）

主 agent 思考/改代码要几分钟，游戏不会等你 —— 世界在你看不见的时候变化，
等于丢失了观察权。真实事故：修 bug 期间殖民地三个人全死了。

| 层 | 机制 | 触发 |
|---|---|---|
| 用户新消息 | 项目级 Hook `UserPromptSubmit` → `oni pause` | 用户一发消息立刻冻结 |
| 主 agent 改代码 | `oni hold "原因"` | 开始改代码前 |
| 代理空闲 | **空闲即冻结**（无待办工单 → 立刻冻回） | 自动 |
| 代理动作失败 | 自动冻结等人，不空转重试 | 自动 |

紧急态是例外：有人呼吸 < 35 时必须占空比（冻 20s → 跑 120s），
因为暂停只冻结倒计时，救不了窒息的人。

## 项目级 Hook 与 Skill（随仓库开源）

```
.minimax/
├── hooks.json                      # 项目级 Hook（设置 → 钩子 → 项目 里开启）
├── scripts/pause-on-prompt.mjs      # 静默执行 oni pause，失败不阻塞对话
└── skills/oni-colony-ops/SKILL.md  # 殖民地接管流程（冻结→观察→重规划→执行→验证）
```

Hook 只在本仓库的会话里生效 —— 不污染其它项目，随仓库走，clone 下来即可用。

## 实时优先级调度 + 语义化动作（通用 agent 的核心）

三次全灭的复盘结论：代理**看不见人** —— 观察的是材料/建筑/气体总量，
没有"每个人现在怎么样"；计划是静态依赖图，反应太慢。

```
Level 3 救命 │ 有人失去行动能力 → 救援（排毒/拖到有空气处/治疗）
Level 2 紧急 │ 有人呼吸<50 或所在格不可呼吸 → 先把人弄出来
Level 1 关注 │ stress飙升 / 氧气消耗归零 / 空气差 / 食物不足
Level 0 正常 │ 按 plan 依赖图推进
```

Level≥2 会**抢占**静态计划。`plan.json` 里的动作是**语义化**的（零硬编码坐标）：

| goal | 含义 |
|---|---|
| `surface` | 挖通到地表（路径由图模块实时算） |
| `element:Algae` | 挖到含某元素的格 |
| `mop_dirty` | 排掉污染水/污染氧 |
| `auto_build` + `region:"auto"` | 自动找建造落点 |

**同一份 plan.json 可直接用于任何存档/星球。**

## 架构演进：从 9 次事故学到的（★ 本项目最有价值的部分）

这个代理是被**真实全灭**逼出来的。每一版架构都对应一次事故的教训 ——
下面每一条都有实测证据，且都已固化进代码（不是只写在文档里）。

| # | 事故 | 症状 | 根因 | 固化方式 |
|---|---|---|---|---|
| 1 | 观测缺失 | 代理看不见人 | 只看总量，没有"人现在怎么样" | `sense::Dupe` 采集位置/呼吸/胁迫/所在格可呼吸性 |
| 2 | 静态计划 | 反应太慢，plan 跟不上 | 依赖图不会被打断 | `priority.rs` 实时调度器，Level≥2 可抢占 |
| 3 | 效果判据 | 判据永假 → 节点卡死 | 用"地上散装 X kg"判据，但 X 会被消耗/转化 | 判据改用**终局指标**（氧气质量/食物 kcal/人口） |
| 4 | 判据不可达 | 反复失败冻回 | 判据要求物理上放不下的东西（3 张床） | 判据按**当前空间**标定（床≥2） |
| 5 | 空跑误判 | 工单挂着没人干，代理冻回 | mod 工单在 `step_facts` 里**看不见** | 挖掘结果显式回报"视野内还有 N 格待完成" |
| 6 | 熔断误伤 | 好节点被自己熔断 | 工单执行期指纹不变 → 连续 3 轮判"零进展" | busy 口径含"本轮自己下的单" |
| 7 | 梯子修错地方 | 竖井挖不动 | 梯子修在复制人**站不到**的格 | 梯子从官方可达格**连续**往上铺 |
| 8 | 视野盲区 | 找不到 20 格外的铁矿 | 元素搜索只扫本地网格（=相机视野） | 回退到 mod `world/search` 全图搜索 |
| 9 | ★挖掘会杀人 | 三人全灭、一人死亡 | **在人脚下开深井** → 地下 CO₂ 涌入低层走廊 | 挖掘点**避人** + 深挖惩罚 + 优先向上 |

### 三条最反直觉的经验

**1. 效果判据必须用终局指标，不能用中间量。**

```
✗ "地上有散装氧石 ≥ 1kg"   → 氧石挖出即释氧，地上永远留不住 → 节点永远卡死
✓ "可见氧气 ≥ 350kg"        → 反映终局事实，达成即 done
```

**2. 空闲判定要放在动作执行之后。**

用「上一轮的 pending_orders」判空闲 → 这一轮刚下的第一张工单还没进队列 →
被当成空闲 → 立刻冻回 → 那张工单永远不会被执行 → 看着像在跑其实零进展。

**3. 深挖有环境代价，不能只看"目标有多近"。**

铁矿在 20 格外、氧气很足 → 直觉是"赶紧挖过去"。但挖 20 格竖井会把
地下 CO₂ 全部释放到低层走廊（CO₂ 沉底）→ 正在作业的复制人全部倒地。

ONI 的 mod **没有拖人/治疗工单**（工具只有 6 个），倒地的复制人不执行工单 →
没人清理污染 → **殖民地死锁，无法自愈**。

所以现在的规则是：**同层/上方的资源永远优先于深埋资源**；换气（向上到地表）
排在深挖之前；开局先 `oni scout` 侦察资源分布再决定挖哪里。

```bash
oni scout        # 探测关键资源位置+距离（落盘 scout.json）
oni plan-reset   # 换存档：按 plan.template.json 重置 + 清局内状态
oni restart [i]  # 一键回滚存档 + 重置 plan + 开新局
```

详见 `AGENTS.md`（工程硬规则）、`docs/`（架构/机制/协作文档）与
`records/optimization-backlog.md`（通用待优化清单）。
> 单局游戏记录（事故复盘 / 值守日志）含具体存档坐标，本地保留、不随仓库开源。

## MCP 并发治理（各司其职，不重复查询）

`oni graph --full` 逐格读全图时**单次查询就是几百个请求**；守护进程、主 agent、
子代理各自采集 → 几十个并发 → mod 端点直接返回 **503**（实测 4 并发 → 657 个格子读取失败）。

| 层 | 机制 | 效果 |
|---|---|---|
| 限流 | 跨进程信号量（槽位文件 + `create_new` 原子抢占） | 同时在飞请求 ≤ 4（`ONI_MAX_CONCURRENT` 可调） |
| 退避重试 | 503/502/504/429 → 指数退避 | 过载变成"等一下"而不是"失败" |
| 查询复用 | 进程内 TTL 缓存 + 跨进程 `live_snapshot.json` | 同一份数据只打一次 MCP |

**职责划分**：守护进程是**唯一采集者**（每轮落盘共享快照），主 agent 是**消费者**
（`oni observe --cached` 零 MCP 压力，两边看的是同一份真值）。

优化后同一压力测试：**0 失败**（优化前 657 个）。

## 文档

| 文档 | 内容 |
|---|---|
| [`AGENTS.md`](AGENTS.md) | 工程硬规则（改代码前必读） |
| [`docs/README.md`](docs/README.md) | 文档索引（通用知识 vs 工程规则的分界） |
| [`docs/playbook.md`](docs/playbook.md) | 场景手册：开局/缺氧/卡住/建造失败/死锁 |
| [`docs/knowledge-base.md`](docs/knowledge-base.md) | 带出处的《缺氧》机制知识库 |
| [`design/graph-model.md`](design/graph-model.md) | 地图图结构设计（工程设计文档） |

## 循环引擎

```
SENSE → JUDGE → FREEZE → DECIDE → EXEC → VERIFY → RESUME → RECORD
```

* **SENSE 一次采全**（10 个资源并行，实测 **~30–50 ms**），禁止只看单一数据源。
* **JUDGE 用阈值表**输出**判定**（DANGER / IDLE / STALL / OK），不是建议。
* **FREEZE / RESUME 必须配对**：暂停只用于紧急态与写世界；任何一次暂停都要有配对的恢复，
  且恢复前必须已经"验证有效"——否则就是让复制人白等死。
* **DECIDE 按依赖图拓扑序**取**一个**目标，而不是修"当前最痛的问题"。
* **EXEC 前必过预检**：预检不过 → 换方案，**绝不执行**。
* **VERIFY 不过 = 没做成**：换方案，不是加轮次。
* **RECORD** 每轮落一个趋势样本 → 用**实测速率**换算"还能撑几周期"。

## 关键设计（都是踩坑换来的）

| 设计 | 原因 |
|---|---|
| 建造目录**两个查询取并集**（默认集 + `includeUnavailable=true`） | 默认集只有 43 项，`includeUnavailable` 那批又按字母序截断；单看任一个都会得出"这游戏没有电解器"的致命误判 |
| 地图补丁用 `.*` 通配符 | 网格里含复制人位置，精确 token 补丁会 `SEARCH did not match`；通配符补丁与位置无关 → **写操作不需要暂停游戏** |
| 预检检查"落脚点" | 目标格四周没有**可站立**的相邻格 → 指令 `ok:true`、标记也在，但没人能走过去干（历史上栽了三次） |
| 挖掘**前沿**语义 | 阶梯/走廊必须一格一格长；每轮只下当前有落脚点的那一格 |
| 坐标：mod 的 **Y 轴向上递增**，锚点=左下角，支撑在 `y-1` | 与直觉相反，搞错会让所有建造报 `missing_support` |
| 账本用**实测速率**算 runway | 只看瞬时值会"发现问题时已经晚了" |
| 可达性用**官方** `/active/dupes/reachability.md` | 自研 BFS 会把"占格但能走"的建筑当墙 → 17 格走廊算成 8 格 → 预检全灭 → 一格也挖不动 |
| 空闲判定用**官方** `step_facts.workIdleCount` | 差事名会把"已预约工单正在路上"的人算成空闲 → 误判空跑 → 反复停机 |
| **窗口结束冻结 ≠ 紧急暂停** | 冻结是硬要求（结束即暂停），不得自动恢复；两种状态必须用不同标志位 |
| 暂停期间不累计"空跑轮数" | 暂停时 dupe 本来就没差事，累加会让开窗第一轮就误停机 |

## 环境

* Rust 1.94（`.cargo/config.toml` 已配 rsproxy 稀疏索引镜像）
* 模组：游戏内 `Jlib` + `ONI MCP Server`，端点 `http://127.0.0.1:8788/mcp/`，协议 `2025-11-25`
