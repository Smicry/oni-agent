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

/// 一条升级请求。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Escalation {
    pub id: u64,
    pub kind: Kind,
    /// 涉及的节点（如果有）
    #[serde(default)]
    pub node: String,
    /// 人话描述：慢系统要读懂这句话
    pub detail: String,
    /// 快系统给出的**建议**（慢系统可采纳也可否决）
    #[serde(default)]
    pub suggestion: String,
    /// 当前周期
    pub cycle: i64,
    /// 人口
    pub dupes: usize,
    /// 氧气 kg
    pub oxygen_kg: f64,
    /// 食物 kcal
    pub food_kcal: f64,
    /// 升级时的 unix 秒
    pub ts: i64,
    /// 是否已被慢系统处理
    #[serde(default)]
    pub acked: bool,
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

/// C: 放行冷静期 —— ack 后同一 kind+node 静默 N 个周期，防止误报无限重生。
/// 为什么需要：needs_ai 的条件（如膀胱涨）在放行后依然存在，
/// 下一轮 priority::direct() 又判 needs_ai=true → 又 raise 同一条 →
/// escalation_hold 永远为 true → 世界反复冻结，其它看门全被压制。
const SILENCE_CYCLES: i64 = 3;

fn silence_path() -> PathBuf {
    dir().join("silence.json")
}

/// silence key 用 Debug 名（稳定）+ node，避免依赖 label 文案。
fn silence_key(kind: Kind, node: &str) -> String {
    format!("{kind:?}|{node}")
}

/// 纯函数：当前 cycle 是否落在该 key 的静默期内。
fn should_silence(silence: &HashMap<String, i64>, key: &str, cycle: i64) -> bool {
    cycle < *silence.get(key).unwrap_or(&0)
}

fn read_silence() -> HashMap<String, i64> {
    std::fs::read_to_string(silence_path())
        .ok()
        .and_then(|t| serde_json::from_str::<HashMap<String, i64>>(&t).ok())
        .unwrap_or_default()
}

fn write_silence(s: &HashMap<String, i64>) {
    let _ = std::fs::write(silence_path(), serde_json::to_string_pretty(s).unwrap_or_default());
}

/// 历史归档（追加写）。
fn history_path() -> PathBuf {
    dir().join("history.jsonl")
}

/// 写一条升级请求（同一 kind+node 的未处理请求会被替换，不刷屏）。
pub fn raise(kind: Kind, node: &str, detail: String, suggestion: String, ctx: Ctx) -> u64 {
    // C: 放行冷静期内不再重报（返回 0 = 跳过，调用方一律 let _ = 忽略）。
    if should_silence(&read_silence(), &silence_key(kind, node), ctx.cycle) {
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
        ts: now(),
        acked: false,
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
    // 写待处理（同 kind+node 覆盖）
    let mut list = read_pending();
    list.retain(|e| !(e.kind == kind && e.node == node));
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
    // C: 写静默水位 —— 放行后 SILENCE_CYCLES 个周期内同一 kind+node 不再重报。
    let key = silence_key(target.kind, &target.node);
    let until = target.cycle + SILENCE_CYCLES;
    let mut silence = read_silence();
    // 取 max：多条同 key 放行时静默期不缩短。
    let entry = silence.entry(key).or_insert(until);
    *entry = (*entry).max(until);
    write_silence(&silence);

    let remaining: Vec<Escalation> = list.into_iter().filter(|e| e.id != id).collect();
    let _ = std::fs::write(
        pending_path(),
        serde_json::to_string_pretty(&remaining).unwrap_or_default(),
    );
    true
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

    /// C: 放行冷静期 —— 只有同 kind+node 且在静默水位内才跳过。
    #[test]
    fn silence_is_scoped_and_expires() {
        let mut s = HashMap::new();
        s.insert(silence_key(Kind::StrategyMismatch, ""), 10);
        s.insert(silence_key(Kind::NoProgress, ""), 10);
        // 同 key 在静默期内 → 跳过
        assert!(should_silence(&s, &silence_key(Kind::StrategyMismatch, ""), 9));
        // 到期后 → 恢复上报
        assert!(!should_silence(&s, &silence_key(Kind::StrategyMismatch, ""), 10));
        // 不同 key 不受影响（工单积压看门不能被膀胱放行压制）
        assert!(!should_silence(&s, &silence_key(Kind::NoProgress, "node_x"), 9));
        // 空表 → 不静默
        assert!(!should_silence(&HashMap::new(), &silence_key(Kind::StrategyMismatch, ""), 9));
    }
}
