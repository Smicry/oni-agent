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
./target/release/oni agent --interval 10 --window 120  # 守护进程
./target/release/oni window 300      # 施工窗口
./target/release/oni hold "<原因>" / oni release
```

## 架构

- **快系统**（守护进程，每 10 秒）：感知、判危、执行、安全降级。
- **慢系统**（AI agent，按需）：战略、重规划、异常处理、规则更新。

守护进程是唯一采集者并落盘共享快照；慢系统写 `plan.json` 与精确工单。
详见 [`docs/coordination.md`](docs/coordination.md)。

## 关键机制

- 官方事实优先：可达性/空闲/电力/材料/建筑以游戏 API 为准。
- 效果判据：用终局指标（氧气/食物/人口/可达格）而非中间存量。
- 语义动作：计划只写意图，不含硬编码坐标。
- 写操作必过预检 + dry-run + 复读。
- 暂停所有权显式记录，跨进程重启保留。
- 慢系统决策写入 `decisions.jsonl`（依据 + 预期效果 + 下轮回填）。

## MCode 接入（Hook + Skill）

仓库附带项目级 Hook 与 Skill（用于 MCode）：

- `.minimax/hooks.json`：用户新消息时自动 `oni pause` 冻结世界
- `.minimax/scripts/pause-on-prompt.mjs`：静默执行，失败不阻塞对话
- `.minimax/skills/oni-colony-ops/SKILL.md`：殖民地接管流程（冻结→观察→重规划→执行→验证）

Hook 需在 MCode 设置 → 钩子 → 项目 里开启；只对本仓库生效。

## 文档
- [`AGENTS.md`](AGENTS.md) — agent 工程规则
- [`docs/knowledge-base.md`](docs/knowledge-base.md) — 带出处的游戏机制
- [`design/graph-model.md`](design/graph-model.md) — 气室图设计
