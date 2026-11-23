//! **快系统「卡住」的单一真相来源**。
//!
//! 为什么要集中：
//!   之前"卡住"的判定散落在 agent.rs 的七八个 `if` 里 → **漏一个就是一个致命缺口**
//!   （保命节点无限重试、找不到落点只会暂缓、死工单被当成空闲…）。
//!   现在：**每轮跑一遍 `detect()`**，产出统一的问题清单；
//!   清单非空 → 写 escalation 升级给慢系统 + 暂缓相关节点。
//!   **世界继续跑**（卡住时冻结会让已有工单永远不被执行）。**没有零散分支可漏。**
//!
//! 快慢分工：
//!   快系统只回答"我现在卡住了吗、卡在哪"（本模块，纯检测、无决策）；
//!   慢系统回答"那该怎么办"（改计划 / 下工单 / 改代码）。

use crate::exec::ExecOutcome;
use crate::sense::Snapshot;

/// 卡住的种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StuckKind {
    /// 预检不过：找不到落点 / 支撑 / 落脚点
    PreflightRejected,
    /// 语义目标找不到任何可行解（surface / oxygen / element 都找不到）
    NoFeasibleTarget,
    /// 材料缺失，且自举也找不到任何可挖的材料
    NoMaterial,
    /// 工单在位（地图有标记）但没人执行 → 死工单
    OrderNotExecuted,
    /// 节点反复失败
    NodeFailing,
    /// 依赖图没有可执行节点
    Deadlock,
    /// 复制人失去行动能力（倒在地上）
    Downed,
    /// 复制人待在不可呼吸的格子里（呼吸在掉）
    Unbreathable,
    /// 人口下降
    PopulationDrop,
    /// 连续多轮零进展
    NoProgress,
}

impl StuckKind {
    pub fn code(&self) -> &'static str {
        match self {
            StuckKind::PreflightRejected => "stuck.preflight",
            StuckKind::NoFeasibleTarget => "stuck.no_target",
            StuckKind::NoMaterial => "stuck.no_material",
            StuckKind::OrderNotExecuted => "stuck.dead_order",
            StuckKind::NodeFailing => "stuck.node_failing",
            StuckKind::Deadlock => "stuck.deadlock",
            StuckKind::Downed => "stuck.downed",
            StuckKind::Unbreathable => "stuck.unbreathable",
            StuckKind::PopulationDrop => "stuck.pop_drop",
            StuckKind::NoProgress => "stuck.no_progress",
        }
    }
}

/// 一条"卡住"。
#[derive(Debug, Clone)]
pub struct Stuck {
    pub kind: StuckKind,
    /// 人话描述
    pub detail: String,
    /// 关联的计划节点
    pub node: String,
}

/// 检测结果。
#[derive(Debug, Clone, Default)]
pub struct Report {
    pub items: Vec<Stuck>,
}

impl Report {
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
    /// 摘要（写进 escalation 的 detail）。
    pub fn summary(&self) -> String {
        self.items
            .iter()
            .map(|s| format!("[{}] {}", s.kind.code(), s.detail))
            .collect::<Vec<_>>()
            .join("；")
    }
    /// 是否属于"必须让世界继续跑"的紧急卡住（倒地：暂停会阻断救援）。
    pub fn needs_running_world(&self) -> bool {
        self.items.iter().any(|s| s.kind == StuckKind::Downed)
    }
}

/// 该差事是否属于副本**正在主动自救/适应环境**（不是被困）。
///
/// AGENTS.md §6.1：「恢复呼吸 / 寻找安全区域」是 CO₂ 洞穴的**常态差事**，不是危急信号。
///   把这些差事当成危险 → 误报 → 误冻结世界 → 白白中断施工。
fn is_self_rescuing(chore: &str) -> bool {
    const SELF_RESCUING: &[&str] = &[
        "休息", "睡觉", "放松", "擦", "如厕", "寻找安全", "恢复呼吸", "呼吸恢复", "待命", "进食",
        "吃", "午餐", "晚餐", "早餐",
    ];
    SELF_RESCUING.iter().any(|k| chore.contains(k))
}

/// **每轮检测一次**。
///
/// 参数：
/// - `snap`：当前快照（人/气/资源/建筑）
/// - `out`：本轮动作的执行结果（判"动作失败"）
/// - `node`：本轮选中的节点
/// - `deadlock`：依赖图是否没有可执行节点
/// - `failure_streak`：动作连续失败次数
/// - `no_progress_streak`：连续零进展轮数
/// - `dig_markers`：地图上在位的挖掘标记数（死工单判定用）
/// - `last_pop`：上一轮人口
pub fn detect(
    snap: &Snapshot,
    out: Option<&ExecOutcome>,
    node: &str,
    deadlock: bool,
    failure_streak: usize,
    no_progress_streak: usize,
    dig_markers: usize,
    last_pop: usize,
) -> Report {
    let mut r = Report::default();

    // ---- 1) 动作明确失败 → 分类原因 ----
    if let Some(out) = out {
        let failed: Vec<&crate::exec::ActionResult> =
            out.results.iter().filter(|x| !x.ok).collect();
        if !failed.is_empty() {
            let detail = failed
                .iter()
                .map(|x| x.detail.clone())
                .collect::<Vec<_>>()
                .join(" | ");
            let kind = if detail.contains("找不到能放")
                || detail.contains("auto 内找不到")
                || detail.contains("预检未通过")
                || detail.contains("不是空的")
                || detail.contains("不实心")
            {
                StuckKind::PreflightRejected
            } else if detail.contains("在当前地图上找不到")
                || detail.contains("语义目标")
                || detail.contains("找不到任何")
            {
                StuckKind::NoFeasibleTarget
            } else if detail.contains("缺建材")
                || detail.contains("材料")
                || detail.contains("不可用")
            {
                StuckKind::NoMaterial
            } else {
                StuckKind::NodeFailing
            };
            r.items.push(Stuck {
                kind,
                detail,
                node: node.to_string(),
            });
        }
    }

    // ---- 2) 节点反复失败 ----
    if failure_streak >= 2 {
        r.items.push(Stuck {
            kind: StuckKind::NodeFailing,
            detail: format!("动作连续失败 {failure_streak} 次"),
            node: node.to_string(),
        });
    }

    // ---- 3) 依赖图死锁 ----
    if deadlock {
        r.items.push(Stuck {
            kind: StuckKind::Deadlock,
            detail: "依赖图里没有可执行节点".into(),
            node: String::new(),
        });
    }

    // ---- 4) 死工单：地图上有挖掘标记，但没有任何待办、也没有在建 ----
    //    mod 的 dig 标记**不进** pendingOrders
    //      代理会误判"全员空闲"→ 冻回 → 工单永远不执行。
    if dig_markers > 0
        && snap.pending_orders.unwrap_or(0) == 0
        && snap.under_construction == 0
        && snap.dupes.iter().all(|d| d.chore.contains("空闲") || d.chore.contains("休息") || d.chore.contains("睡觉"))
    {
        r.items.push(Stuck {
            kind: StuckKind::OrderNotExecuted,
            detail: format!("地图上有 {dig_markers} 个挖掘标记，但无待办工单且全员空闲 → 死工单（工单没人能做）"),
            node: node.to_string(),
        });
    }

    // ---- 5) 复制人倒地 ----
    let downed = snap.downed_count();
    if downed > 0 {
        r.items.push(Stuck {
            kind: StuckKind::Downed,
            detail: format!("{downed} 个复制人失去行动能力（他们不会执行任何差事；暂停会阻断救援）"),
            node: String::new(),
        });
    }

    // ---- 6) 复制人在不可呼吸的格子里 ----
    // 修正（误报冻结）：
    //      「在 CO₂ 主导的格子里」是 ONI 的**常态差事** —— 复制人会在那儿
    //      睡觉、擦洗、找安全区、呼吸恢复。
    //      单点判「呼吸 < 95」→ 睡觉的人呼吸 90 也被当成危险 → 误冻结世界。
    //      正解：① 阈值收紧到 60；② **排除正在自救的差事**（休息/睡觉/擦洗/
    //      寻找安全区域/恢复呼吸）—— 那些差事说明副本在主动应对，不是被困。
    let bad = snap
        .dupes
        .iter()
        .filter(|d| {
            d.in_unbreathable()
                && d.breath.unwrap_or(100.0) < 60.0
                && !is_self_rescuing(&d.chore)
        })
        .count();
    if bad > 0 {
        r.items.push(Stuck {
            kind: StuckKind::Unbreathable,
            detail: format!("{bad} 个复制人待在不可呼吸的格子里（呼吸 < 60）"),
            node: String::new(),
        });
    }

    // ---- 7) 人口下降 ----
    let pop = snap.dupes.len();
    if last_pop > 0 && pop < last_pop {
        r.items.push(Stuck {
            kind: StuckKind::PopulationDrop,
            detail: format!("人口从 {last_pop} 掉到 {pop}"),
            node: String::new(),
        });
    }

    // ---- 8) 长期零进展 ----
    if no_progress_streak >= 3 {
        r.items.push(Stuck {
            kind: StuckKind::NoProgress,
            detail: format!("连续 {no_progress_streak} 轮零进展"),
            node: node.to_string(),
        });
    }

    r
}
