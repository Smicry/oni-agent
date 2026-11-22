# Eval 对比：oni-colony-ops

- 任务：接手《缺氧》殖民地，诊断并重新安排计划。
- with-skill: 见 TMPDIR 下 eval-oni-colony-ops/with-skill.md
- baseline:   见 TMPDIR 下 eval-oni-colony-ops/baseline.md

## 1. with-skill 是否更好？—— **是（明显更好）**

两个版本都诊断出了同一批真问题，但 with-skill 的**执行纪律**明显更严：

| 维度 | with-skill | baseline |
|---|---|---|
| 先冻结世界再观察 | 明确执行 FREEZE 并复核状态 | 未主动暂停，直接观察 |
| 命令范围 | 全部只读 + 一次外科式 plan.json 改动 | 同样只读 + 改动，但没先冻结 |
| 写操作 | 零游戏写操作 | 零游戏写操作 |
| 判据层级 bug | 发现 power_operational 读顶层 | **独立发现同一个 bug** |
| 计划质量 | 10 节点，图论式重排 | 9 节点，改了 11 处 |
| 额外发现 | 研究锁、人力发电机无燃料槽、氧石产出污染氧、换气短路径只要 6 格 | unlocks 只是权重、食物第二道闸门是食谱、打印舱无电源端口、电线必须与端口正交相邻 |

**关键交叉验证**：两个独立子代理（一个有 skill、一个没有）**都发现了
`power_operational` 读顶层而非 summary 这个 bug** —— 说明这是真 bug，不是 skill
引导出来的幻觉。已在同轮修掉。

## 2. 更好在哪里

- **with-skill 的第 0 步 FREEZE** 让它全程在世界静止的快照上判断，避免竞态。
- **with-skill 的 VERIFY — 复读不靠日志自证** 让它去读了 ManualGenerator 实例参数，
  从而发现「人力发电机没有燃料槽」。baseline 用了更多命令但没深挖实例文件。
- **with-skill 的故障速查表**让它直接想到「判据恒假 → 下游永不解锁」这条链。

## 3. 若有不足

- skill 的硬约束表里没有「研究锁」这一条 —— 两个版本都靠现场发现补上。
- skill 没提 `unlocks` 只是权重、`requires` 才是硬依赖。
- skill 没说「造出来 ≠ 有人吃」（复制人食谱白名单）。baseline 反而抓得更准。
- skill 没提「并行读全图会大面积 MCP 503」—— with-skill 明确退回便宜读法处理了。

## 4. 改进建议（已落地）

1. 硬约束表增加两行：
   - **建筑可能锁在研究后** —— 查 `oni://buildings/defs` 的 `unlocked` /
     `autoMaterialReason: building_locked_by_research`；锁着的造不出来，别排在计划后面。
   - **造出来 ≠ 有人吃** —— 复制人有食谱白名单；农业站种的东西若不在
     `/active/management/food.md` 的允许列表，要先开 `food_policy`。
2. REPLAN 补引擎语义：`requires` 是硬依赖，`unlocks` 只是**优先级权重**（引擎取其长度），
   `unlocks=[]` 的节点会永远排最后。
3. OBSERVE 补一句：**MCP 并发读全图时会大面积 503**，这时退回视口模式 + 逐格读。
4. 故障速查加一行：`节点永远不 done → 下游全不解锁 → 整图卡死`，
   先查该节点 `success_check` 是否**恒假**（读错层级 / 建筑未解锁 / 判据不可能达成）。

## 5. 要不要打包脚本？

**不需要**。with-skill 过程里没有反复做同一个确定性本地动作 —— 全是一次性 CLI 调用。
打包脚本只增加维护面、不省步骤。

但有一处值得固化到**代码**（而不是 skill）：`success_check` 的判据可用性检查。
已在同轮把两处判据 bug 修掉（`power_operational` 读 summary；建造时区分研究锁 vs 缺材料）。
