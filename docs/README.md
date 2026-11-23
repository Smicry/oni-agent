# oni-agent 文档索引

> 一切判断的起点：`oni observe`（全局态势卡）。
> 工程硬规则：`../AGENTS.md`。

## 本目录只放「通用知识」

| 文档 | 回答什么问题 | 何时读 |
|---|---|---|
| [`knowledge-base.md`](knowledge-base.md) | 《缺氧》**游戏机制**与策略：产氧/温度/电力/水/食物/士气/地形，带出处 | 需要机制细节或具体数字时 |
| [`playbook.md`](playbook.md) | **通用场景**手册：开局/缺氧/卡住/建造失败/死锁/人口告急 | 每轮决策时按场景查 |


## 不在本目录的东西

| 内容 | 位置 | 原因 |
|---|---|---|
| 工程硬规则（坐标系、真值优先级、熔断纪律、阈值） | [`../AGENTS.md`](../AGENTS.md) | 规则是"怎么做对"，不是"游戏是什么" |
| 工程设计文档（图结构、模块设计） | [`../design/`](../design/) | 设计说明属于工程，不是游戏知识 |
| 失败/事故记录详情 | [`../records/`](../records/) | 失败详情是**历史**，只提炼成规则，不进知识库 |
| 存档专属地形坐标 | [`../AGENTS.md`](../AGENTS.md) 第 11 节 | 只对当前存档有效，换局即失效 |
| 依赖图与本轮目标 | [`../plan.json`](../plan.json) | 运行时状态，不是知识 |

## 边界纪律

1. **知识库只写"换个存档依然成立"的东西。** 凡是带具体存档坐标、具体局数、具体人名的，一律不进知识库。
2. **失败不写进知识库，只写进规则。** 每次事故的详情放 `records/`；提炼出的可执行规则进 `AGENTS.md`；固化成代码。
3. **知识库每条实质性数值必须有出处**；社区经验值明确标注，不与官方机制混淆。
4. 阈值以 `AGENTS.md` 与代码为准；知识库不重复收录，避免两处打架。

## 项目级 Skill 与脚本

本仓库自带项目级 Skill 与辅助脚本：

```
.minimax/
├── hooks.json                      # hook 开关；当前 **未注册** UserPromptSubmit 条目
├── scripts/pause-on-prompt.mjs      # UserPromptSubmit 执行体（静默、fail-open）——当前未被引用
├── scripts/session-id.sh            # 单局稳定 key（初始存档 hash 前 16 位）
└── skills/oni-colony-ops/SKILL.md  # 殖民地接管流程（冻结→观察→重规划→执行→验证）
```

**注意："用户新消息即自动暂停"当前不生效。** `hooks.json` 的 `hooks` 映射为空，
`UserPromptSubmit` 条目已被有意移除（慢系统改为常驻 `oni serve` 守护进程后，
不再依赖 MCode 会话驱动）。`pause-on-prompt.mjs` 作为参考实现保留。

主 agent 需要冻结世界时用 `oni hold`（唯一受保护的停止信号），改完 `oni release`。
若要重新启用该 Hook，需在 `hooks.json` 的 `hooks.UserPromptSubmit` 补回条目——
但注意它调的是 `oni pause`（owner=User），AI 开启时会被代理自动恢复，冻结不住。

**为什么放项目级而不是全局 plugin**：
- 这个 Hook 只在《缺氧》项目里有意义，不该污染其它项目的会话。
- 项目级 Hook 随仓库走，团队/协作者 clone 下来即可用；全局 plugin 还要单独安装。
- 官方约定：单个项目的 Hook 放 `<project root>/.minimax/hooks.json`，**不要**为它创建 Plugin。

**注意**：写入项目 Hook 后需要用户在 **设置 → 钩子 → 项目** 里打开开关才会生效。

**用法**：把 `oni-colony-ops` skill 当作殖民地操作手册 —— 它规定了每一轮
「先冻结 → 一次采全 → 判定 → 重排 → 预检执行 → 复读验证 → 有限窗口恢复」的顺序，
以及那张硬约束表（开局零建材、研究锁、造出来≠有人吃、判据恒假的连锁反应等）。
