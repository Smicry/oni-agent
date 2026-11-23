# oni-agent — 《缺氧》长期自主代理

用 Rust 实现的 MCP 客户端 + 循环引擎，让 AI 长期接管一局《缺氧》而不浪费游戏周期。

## 结构

```
crates/oni-mcp      MCP 客户端（传输层）
crates/oni-world    世界模型 + 下单前预检
crates/oni-engine   循环引擎 + 常驻守护进程
crates/oni-cli      `oni` 命令行
```

`oni-agent` 是整个代理；`oni-mcp` 只是传输层。详见 [README.md](README.md)（英文）。

## 快速开始

```bash
cargo build --release
./target/release/oni doctor          # 连通性自检
./target/release/oni observe         # 全局态势
./target/release/oni plan            # 本轮目标
./target/release/oni act             # 执行（预检→dry-run→apply）
./target/release/oni serve          # ★ 一键启动：快系统 + 慢系统 + Web 控制台(:8789)
# ./target/release/oni agent --interval 10 --window 120  # 仅快系统的旧入口
./target/release/oni window 300      # 施工窗口
./target/release/oni hold "<原因>" / oni release
```

## 架构

- **快系统**（守护进程，每 10 秒）：感知、判危、执行、安全降级。
- **慢系统**（AI agent，按需）：战略、重规划、异常处理、规则更新。

守护进程是唯一的**持续**采集者并落盘共享快照；`oni observe --cached` 零 MCP 读它，
而 `oni observe`（默认）与 `oni audit`（快照过期时）仍会实时采样一次——下指令前请用实时读。
慢系统写 `plan.json` 与精确工单。
详见 [`docs/coordination.md`](docs/coordination.md)。

## 关键机制

- 官方事实优先：可达性/空闲/电力/材料/建筑以游戏 API 为准。
- 效果判据：用终局指标（氧气/食物/人口/可达格）而非中间存量。
- 语义动作：计划只写意图，不含硬编码坐标。
- 写操作必过预检 + dry-run + 复读。
- 暂停所有权显式记录，跨进程重启保留。
- 慢系统决策写入 `decisions.jsonl`（依据 + 预期效果 + 下轮回填）。
- **世界状态用实时真值判定**：`isPaused` / `gameSpeed <= 0` / 复核，不用过期快照——
  用陈旧值判定会让代理在游戏已停时继续下工单，工单永不执行 → 看着零进展 → 误熔断好节点。
  关键状态走 1s TTL（`oni://colony/status`），其余 8s；同一套 singleflight 合并并发读，不额外压端点。
- **保命节点连续失败会暂缓**（deferred，可恢复），不是每轮重试：熔断保命节点=自杀，
  但每轮重试会造成"恢复→失败→冻回"无限乒乓，游戏大部分时间停着。

## MCode 接入（Skill + 脚本）

仓库附带项目级 Skill 与脚本（用于 MCode）：

- `.minimax/hooks.json`：hook 开关；**当前未注册** `UserPromptSubmit` 条目
- `.minimax/scripts/pause-on-prompt.mjs`：`UserPromptSubmit` 执行体（静默、失败不阻塞）——当前未被引用
- `.minimax/scripts/session-id.sh`：单局稳定 key（初始存档 hash 前 16 位）
- `.minimax/skills/oni-colony-ops/SKILL.md`：殖民地接管流程（冻结→观察→重规划→执行→验证）

**"用户新消息自动冻结世界"目前不生效**：`hooks.json` 的 `hooks` 为空，该条目已被有意移除
（慢系统改为常驻 `oni serve` 守护进程后不再依赖 MCode 会话驱动）。
主 agent 改代码/深挖期间要冻结世界，请显式执行 `oni hold`（唯一受保护的停止信号），改完 `oni release`。

## 文档
- [`AGENTS.md`](AGENTS.md) — agent 工程规则
- [`docs/knowledge-base.md`](docs/knowledge-base.md) — 带出处的游戏机制
- [`design/graph-model.md`](design/graph-model.md) — 气室图设计
