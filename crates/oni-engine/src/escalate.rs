//! **快系统 → 慢系统的交接通道（escalation）**。
//!
//! 为什么必须有：
//!   双系统架构里，代理（快系统）负责**每 10 秒**的感知与执行，
//!   但它**不该**独自承担"这一局该怎么打"的大判断。
//!   遇到规则无法归类的情况（策略不匹配、资源挖不到、连续失败、未知危险），
//!   代理应该：**明确喊人 + 停手**，而不是自己瞎试或无限重试。
//!
//! 快慢分工（本项目的定稿）：
//!   | | 慢系统（AI / 主 agent） | 快系统（代理 / 守护进程） |
//!   |---|---|---|
//!   | 节奏 | 几秒~几分钟一次 | 每 10 秒一轮 |
//!   | 职责 | 开局大规划、跨周期换方向、异常接管、规则沉淀 | 感知、判危、执行、局部微调、安全降级 |
//!   | 产物 | plan.json（战略层）、代码、AGENTS.md 规则 | 工单、梯子、砖、接线、收割、选点 |
//!   | 遇到卡点 | —— | **escalation：喊人 + 停手** |
//!
//! 一条铁律：**喊人之后不许继续瞎试**。
//!   旧行为是"保命节点失败就重置计数 → 下一轮再试"→ 无限重试、白烧周期；
//!   现在失败到阈值就**升级上报并停手**，等慢系统改计划。

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 升级请求的类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    /// 节点连续失败（不是研究锁这种已知原因）
    NodeFailing,
    /// 自举/语义目标找不到任何可行解 → 代理不知道该干什么了
    NoFeasibleTarget,
    /// 依赖图死锁：没有任何可执行节点
    Deadlock,
    /// 出现未识别的危险模式（多人同时倒地等）
    UnknownHazard,
    /// 连续多轮零进展
    NoProgress,
    /// 策略族与实际不符
    StrategyMismatch,
}

impl Kind {
    pub fn label(&self) -> &'static str {
        match self {
            Kind::NodeFailing => "节点反复失败",
            Kind::NoFeasibleTarget => "找不到任何可行目标",
            Kind::Deadlock => "依赖图死锁",
            Kind::UnknownHazard => "未识别的危险模式",
            Kind::NoProgress => "连续零进展",
            Kind::StrategyMismatch => "策略与实际不符",
        }
    }
}

/// 快系统给出的候选动作 —— 慢系统**选**，不手算坐标。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Candidate {
    pub id: String,
    /// 动作语义名：extend_ladder / switch_column / switch_goal / cancel_orders ...
    pub action: String,
    /// 动作目标（坐标或列号，语义由 action 决定）
    pub target: Option<(i32, i32)>,
    pub note: String,
}

/// 结构化诊断上下文 —— 快系统提供足够信息，慢系统据此决策。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EscContext {
    /// 卡点类型：unreachable_target / dead_orders / no_material / strategy_stale ...
    pub obstacle: String,
    /// 当前目标（语义名）
    pub goal: String,
    /// 卡住的目标格
    pub target: Option<(i32, i32)>,
    /// 可达包络 (x1,y1,x2,y2)
    pub reach_bounds: Option<(i32, i32, i32, i32)>,
    /// 候选动作（慢系统选其一）
    pub candidates: Vec<Candidate>,
}

/// 一条升级请求。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Escalation {
    pub id: u64,
    pub kind: Kind,
    /// 涉及的节点（如果有）
    pub node: String,
    /// 人话描述：慢系统要读懂这句话
    pub detail: String,
    /// 快系统给出的**建议**（慢系统可采纳也可否决）
    pub suggestion: String,
    /// 当前周期
    pub cycle: i64,
    /// 人口
    pub dupes: usize,
    /// 氧气 kg
    pub oxygen_kg: f64,
    /// 食物 kcal
    pub food_kcal: f64,
    /// 最低呼吸值（静默恶化判定用）
    pub breath: f64,
    /// 最高膀胱值（静默恶化判定用）
    pub bladder: f64,
    /// 升级时的 unix 秒
    pub ts: i64,
    /// 是否已被慢系统处理
    pub acked: bool,
    /// 结构化诊断（快系统提供，慢系统据此决策）
    pub context: EscContext,
    /// 语义问题类别（来自 fast 的 Action，用于静默去重）
    pub category: String,
}

/// escalation 文件所在目录（按存档隔离）。
fn dir() -> PathBuf {
    let base = oni_mcp::save::save_dir(
        &oni_mcp::save::current()
            .map(|s| s.fingerprint)
            .unwrap_or_else(|| "unknown".into()),
    );
    base.join("escalations")
}

/// 待处理列表文件。
fn pending_path() -> PathBuf {
    dir().join("pending.json")
}

/// C: 放行冷静期 —— ack 后同一 kind+node+category 静默 N 秒，防止误报无限重生。
///
/// 为什么用墙钟而非 cycle：cycle 在游戏暂停/低推进时**卡住不动**，
/// 用 cycle 做水位会让静默**永不到期** → 真危机（如膀胱94%）被永久压制。
/// 墙钟一定前进 → 静默必然到期 → 误报不刷屏、真危机仍能重报。
const SILENCE_SECS: i64 = 180;

/// 静默条目：到期墙钟秒 + ack 时的指标基线（恶化打破用）。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SilenceEntry {
    /// 到期 unix 秒
    until: i64,
    /// ack 时的氧气 kg（下降超阈值 = 恶化，提前打破静默）
    oxygen_kg: f64,
    /// ack 时的食物 kcal
    food_kcal: f64,
    /// ack 时的最低呼吸值（下降超过阈值 = 恶化，提前打破静默）
    breath: f64,
    /// ack 时的最高膀胱值（上升超过阈值 = 恶化，提前打破静默）
    bladder: f64,
}

fn silence_path() -> PathBuf {
    dir().join("silence.json")
}

/// silence key 用 Debug 名（稳定）+ node + category，避免依赖 label 文案。
/// ★ category 是**语义问题类别**（来自 fast 的 Action / 卡点来源），
///   同类问题在静默窗内不重报，不同类问题互不压制（呼吸≠膀胱≠氧气冻结）。
fn silence_key(kind: Kind, node: &str, category: &str) -> String {
    format!("{kind:?}|{node}|{category}")
}

/// 指标显著恶化 → 打破静默（通用原则：静默压"重复"，不压"恶化"）。
/// 氧气/食物掉了 20%、呼吸掉 15 点、膀胱升 15 点都视为恶化。
fn deteriorated(e: &SilenceEntry, ctx: &Ctx) -> bool {
    let oxy_worse = e.oxygen_kg > 0.0 && ctx.oxygen_kg < e.oxygen_kg * 0.8;
    let food_worse = e.food_kcal > 0.0 && ctx.food_kcal < e.food_kcal * 0.8;
    // ctx 值为 0 = 本轮没采到（未知），不能当成「极低呼吸 / 空膀胱」误判恶化。
    let breath_worse = e.breath > 0.0 && ctx.breath > 0.0 && ctx.breath < e.breath - 15.0;
    let bladder_worse = e.bladder > 0.0 && ctx.bladder > 0.0 && ctx.bladder > e.bladder + 15.0;
    oxy_worse || food_worse || breath_worse || bladder_worse
}

/// 纯函数：是否处于静默期（墙钟未到期 且 指标未恶化）。
/// 兼容旧数据：旧 silence.json 存的是 cycle 号（很小），作为 unix 秒早已过期 → 自动失效。
fn should_silence(silence: &HashMap<String, SilenceEntry>, key: &str, now: i64, ctx: &Ctx) -> bool {
    match silence.get(key) {
        None => false,
        Some(e) => now < e.until && !deteriorated(e, ctx),
    }
}

fn read_silence() -> HashMap<String, SilenceEntry> {
    std::fs::read_to_string(silence_path())
        .ok()
        .and_then(|t| serde_json::from_str::<HashMap<String, SilenceEntry>>(&t).ok())
        .unwrap_or_default()
}

fn write_silence(s: &HashMap<String, SilenceEntry>) {
    let _ = std::fs::write(silence_path(), serde_json::to_string_pretty(s).unwrap_or_default());
}

/// 历史归档（追加写）。
fn history_path() -> PathBuf {
    dir().join("history.jsonl")
}

/// 写一条升级请求（同一 kind+node+category 的未处理请求会被替换，不刷屏）。
/// category 传 "" 表示不细分（兼容旧调用点）。
pub fn raise(kind: Kind, node: &str, detail: String, suggestion: String, ctx: Ctx) -> u64 {
    raise_categorized(kind, node, detail, suggestion, ctx, "", EscContext::default())
}

/// 带语义类别的 raise —— category 用于静默去重（同类不刷屏、异类互不压制）。
pub fn raise_categorized(
    kind: Kind, node: &str, detail: String, suggestion: String,
    ctx: Ctx, category: &str, context: EscContext,
) -> u64 {
    raise_impl(kind, node, detail, suggestion, ctx, category, context)
}

/// 带结构化诊断的 raise —— 快系统提供足够信息（卡点/目标/可达区/候选）。
pub fn raise_struct(
    kind: Kind, node: &str, detail: String, suggestion: String,
    ctx: Ctx, context: EscContext,
) -> u64 {
    raise_impl(kind, node, detail, suggestion, ctx, "", context)
}

fn raise_impl(
    kind: Kind, node: &str, detail: String, suggestion: String,
    ctx: Ctx, category: &str, context: EscContext,
) -> u64 {
    // C: 放行冷静期内不再重报（墙钟 + 恶化打破；返回 0 = 跳过）。
    //   静默 key = kind|node|category：只压「同一问题」，不误伤另一个问题。
    let key = silence_key(kind, node, category);
    if should_silence(&read_silence(), &key, now(), &ctx) {
        return 0;
    }
    let id = next_id();
    let esc = Escalation {
        id,
        kind,
        node: node.to_string(),
        detail,
        suggestion,
        cycle: ctx.cycle,
        dupes: ctx.dupes,
        oxygen_kg: ctx.oxygen_kg,
        food_kcal: ctx.food_kcal,
        breath: ctx.breath,
        bladder: ctx.bladder,
        category: category.to_string(),
        ts: now(),
        acked: false,
        context,
    };
    // 写历史
    if let Some(d) = dir().parent().map(|p| p.to_path_buf()) {
        let _ = std::fs::create_dir_all(&d);
    }
    let _ = std::fs::create_dir_all(dir());
    if let Ok(line) = serde_json::to_string(&esc) {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(history_path())
        {
            let _ = writeln!(f, "{line}");
        }
    }
    // 同时发到**协同总线**（统一时间线 + QoS + 可回放）
    let _ = crate::bus::publish(
        crate::bus::Kind::Escalated,
        crate::bus::Priority::Warn,
        node,
        &esc.detail,
        serde_json::json!({"cycle": esc.cycle, "dupes": esc.dupes, "oxygen_kg": esc.oxygen_kg, "food_kcal": esc.food_kcal}),
        1800,
    );
    // 写待处理（同 kind+node+category 覆盖：呼吸和膀胱是两条独立问题，互不覆盖）
    let mut list = read_pending();
    list.retain(|e| !(e.kind == kind && e.node == node && e.category == category));
    list.push(esc);
    let _ = std::fs::write(
        pending_path(),
        serde_json::to_string_pretty(&list).unwrap_or_default(),
    );
    id
}

/// 升级时的世界快照（让慢系统不用再去查一遍）。
#[derive(Debug, Clone, Copy, Default)]
pub struct Ctx {
    pub cycle: i64,
    pub dupes: usize,
    pub oxygen_kg: f64,
    pub food_kcal: f64,
    /// 最低呼吸值（用于静默期的恶化判定；0 = 未知）
    pub breath: f64,
    /// 最高膀胱值（用于静默期的恶化判定；0 = 未知）
    pub bladder: f64,
}

/// 读待处理列表。
pub fn read_pending() -> Vec<Escalation> {
    std::fs::read_to_string(pending_path())
        .ok()
        .and_then(|t| serde_json::from_str::<Vec<Escalation>>(&t).ok())
        .unwrap_or_default()
}

/// 标记某条为已处理（慢系统接手后调用）。
pub fn ack(id: u64) -> bool {
    let list = read_pending();
    let Some(target) = list.iter().find(|e| e.id == id) else {
        return false;
    };
    // C: 写静默条目 —— 墙钟到期 + ack 时指标基线（恶化可提前打破）。
    let key = silence_key(target.kind, &target.node, &target.category);
    let entry = SilenceEntry {
        until: now() + SILENCE_SECS,
        oxygen_kg: target.oxygen_kg,
        food_kcal: target.food_kcal,
        breath: target.breath,
        bladder: target.bladder,
    };
    let mut silence = read_silence();
    // 到期时间取 max：多条同 key 放行时静默期不缩短。
    let slot = silence.entry(key).or_insert_with(|| entry.clone());
    if entry.until > slot.until {
        *slot = entry;
    }
    write_silence(&silence);

    let remaining: Vec<Escalation> = list.into_iter().filter(|e| e.id != id).collect();
    let _ = std::fs::write(
        pending_path(),
        serde_json::to_string_pretty(&remaining).unwrap_or_default(),
    );
    // ★ 徽章翻转：history.jsonl 是 UI 的「待处理/已处理」数据源，
    //   旧代码只删 pending、不改 history → 已处理条目永远显示「待处理」。
    mark_history_acked(id);
    true
}

/// 回写 history.jsonl：把 id 对应的条目 acked 置 true（UI 徽章从红翻绿）。
/// history 是 append-only 的日志，这里整文件重写（条目量小，一局几十条）。
fn mark_history_acked(id: u64) {
    let Ok(text) = std::fs::read_to_string(history_path()) else { return };
    let mut lines = Vec::new();
    let mut changed = false;
    for line in text.lines() {
        match serde_json::from_str::<Value>(line) {
            Ok(mut v) if v.get("id").and_then(|x| x.as_u64()) == Some(id) => {
                if !v.get("acked").and_then(|x| x.as_bool()).unwrap_or(false) {
                    v["acked"] = Value::Bool(true);
                    changed = true;
                }
                if let Ok(s) = serde_json::to_string(&v) { lines.push(s); }
            }
            Ok(v) => {
                if let Ok(s) = serde_json::to_string(&v) { lines.push(s); }
            }
            Err(_) => lines.push(line.to_string()),
        }
    }
    if changed {
        let mut buf = lines.join("\n");
        buf.push('\n');
        let _ = std::fs::write(history_path(), buf);
    }
}

/// 清空待处理（慢系统接管全部）。
pub fn clear() {
    let _ = std::fs::remove_file(pending_path());
}

/// 记录一条「升级请求未被处理」的备注（看门狗降级时用）。
pub fn note_unhandled(reason: &str) {
    // 把「超时未处理」写进**历史**（pending 里的 Escalation 没有 result 字段）
    if let Some(d) = dir().parent().map(|p| p.to_path_buf()) {
        let _ = std::fs::create_dir_all(&d);
    }
    let _ = std::fs::create_dir_all(dir());
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(history_path())
    {
        let line = serde_json::json!({
            "id": 0,
            "kind": "WatchdogTimeout",
            "node": "",
            "detail": reason,
            "suggestion": "看门狗降级：冻结已解除，代理继续自主运行；该问题仍在待处理清单里",
            "ts": now(),
            "acked": true,
        });
        let _ = writeln!(f, "{line}");
    }
}

/// 是否有待处理的升级请求。
pub fn has_pending() -> bool {
    !read_pending().is_empty()
}

/// 人话摘要（给日志用）。
pub fn summary() -> String {
    let list = read_pending();
    if list.is_empty() {
        return "🆘 无待处理的升级请求".to_string();
    }
    list.iter()
        .map(|e| {
            format!(
                "🆘 [{}] {}{} —— {}",
                e.kind.label(),
                if e.node.is_empty() { String::new() } else { format!("（节点 {}）", e.node) },
                "",
                e.detail
            )
        })
        .collect::<Vec<_>>()
        .join("；")
}

fn next_id() -> u64 {
    now() as u64
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_labels_are_stable() {
        // 慢系统按文案判断，标签不能随意改
        assert_eq!(Kind::NodeFailing.label(), "节点反复失败");
        assert_eq!(Kind::Deadlock.label(), "依赖图死锁");
        assert_eq!(Kind::NoFeasibleTarget.label(), "找不到任何可行目标");
    }

    /// C: 墙钟静默 —— 到期内静默、到期后恢复、恶化提前打破、不同 key 隔离。
    #[test]
    fn silence_is_scoped_expires_and_breaks_on_deterioration() {
        let ctx = Ctx { cycle: 3, dupes: 3, oxygen_kg: 100.0, food_kcal: 1000.0, ..Default::default() };
        let entry = SilenceEntry { until: 200, oxygen_kg: 100.0, food_kcal: 1000.0, breath: 50.0, bladder: 50.0 };
        let mut s = HashMap::new();
        s.insert(silence_key(Kind::StrategyMismatch, "", "Rescue"), entry.clone());
        s.insert(silence_key(Kind::NoProgress, "", ""), entry.clone());
        let key = silence_key(Kind::StrategyMismatch, "", "Rescue");
        // 墙钟未到期 → 静默
        assert!(should_silence(&s, &key, 100, &ctx));
        // 墙钟到期 → 恢复上报（即使 cycle 仍卡在 3）
        assert!(!should_silence(&s, &key, 200, &ctx));
        // 指标恶化 → 未到期也打破静默
        let bad = Ctx { cycle: 3, dupes: 3, oxygen_kg: 70.0, food_kcal: 1000.0, ..Default::default() };
        assert!(!should_silence(&s, &key, 100, &bad), "氧气掉20%应打破静默");
        // 呼吸掉 20 点 → 打破静默
        let bad_breath = Ctx { breath: 30.0, ..ctx };
        assert!(!should_silence(&s, &key, 100, &bad_breath), "呼吸掉15点应打破静默");
        // 不同 category / key 不受影响（呼吸 vs 膀胱互不压制）
        assert!(!should_silence(&s, &silence_key(Kind::StrategyMismatch, "", "BuildToilet"), 100, &ctx));
        assert!(!should_silence(&s, &silence_key(Kind::NoProgress, "node_x", ""), 100, &ctx));
        // 空表 → 不静默
        assert!(!should_silence(&HashMap::new(), &key, 100, &ctx));
    }
}
