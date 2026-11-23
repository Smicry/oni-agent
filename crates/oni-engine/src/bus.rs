//! **协同总线（轻量事件中间件）** —— 快慢系统之间的统一事件流。
//!
//! 为什么不引入 DDS 这类重型中间件：
//!   DDS 的核心价值是「**多节点/多进程的实时数据分发** + QoS + 服务发现」。
//!   我们的场景是「单游戏 + 单快进程 + 一个慢系统」：
//!     - 没有多节点分发问题 → DDS 的核心能力用不上；
//!     - 多一个常驻进程 = 多一个故障点，而我们的系统已经因"慢系统离线"吃过亏
//!       ；
//!     - 事件量极小→ 文件通道完全够。
//!   ⇒ 结论：**不引入 DDS**，但**借鉴它的三个设计**，做一个零进程、零依赖的总线：
//!     ① **发布/订阅**（而不是点对点请求-响应）：双方都只发布/订阅事件，互不直接调用；
//!     ② **QoS 语义**：优先级 / 时效(TTL) / 去重 / 批量窗口；
//!     ③ **可回放的事件日志**（append-only）→ 支持慢系统"回放这一局做复盘"。
//!
//! 相比原来的三个专用通道（escalate / commands / audit）：
//!   - **统一时间线**：慢系统在一个地方看到所有协同事件；
//!   - **生命周期闭环**：raised → acknowledged → resolved（修「只有单向确认」的弱点）；
//!   - **时效**：过期的问题自动失效（持续卡住的旧报告没有意义）。

use std::io::Write;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// 事件种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Kind {
    /// 快系统检测到卡住
    StuckDetected,
    /// 快系统喊人（升级）
    Escalated,
    /// 用户下发指令
    CommandIssued,
    /// 慢系统改了计划
    PlanUpdated,
    /// 慢系统巡检发现
    AuditFinding,
    /// 问题已解决（正反馈）
    Resolved,
}

impl Kind {
    pub fn label(&self) -> &'static str {
        match self {
            Kind::StuckDetected => "卡住",
            Kind::Escalated => "升级",
            Kind::CommandIssued => "用户指令",
            Kind::PlanUpdated => "计划变更",
            Kind::AuditFinding => "巡检发现",
            Kind::Resolved => "已解决",
        }
    }
}

/// 优先级（借鉴 DDS 的 QoS 思想）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Priority {
    Info,
    Warn,
    Critical,
}

impl Priority {
    pub fn label(&self) -> &'static str {
        match self {
            Priority::Info => "提示",
            Priority::Warn => "警告",
            Priority::Critical => "紧急",
        }
    }
}

/// 事件状态（生命周期闭环）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum State {
    /// 待处理
    Open,
    /// 慢系统已接手
    Acknowledged,
    /// 已确认解决
    Resolved,
}

/// 一个协同事件。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub id: u64,
    pub ts: i64,
    pub kind: Kind,
    pub priority: Priority,
    /// 时效（秒）：超过就自动失效（持续卡住的旧报告没意义）
    pub ttl_secs: i64,
    pub node: String,
    pub detail: String,
    /// 上下文（人口/氧气/食物等）
    pub payload: Value,
    pub state: State,
    /// 解决/确认时的备注
    pub note: String,
}

impl Event {
    pub fn expired(&self, now: i64) -> bool {
        self.ttl_secs > 0 && now - self.ts > self.ttl_secs
    }
}

fn dir() -> PathBuf {
    let fp = oni_mcp::save::current()
        .map(|s| s.fingerprint)
        .unwrap_or_else(|| "unknown".into());
    oni_mcp::save::save_dir(&fp).join("bus")
}

fn log_path() -> PathBuf {
    dir().join("events.jsonl")
}

/// 发布一个事件（append-only，可回放）。
pub fn publish(kind: Kind, priority: Priority, node: &str, detail: &str, payload: Value, ttl_secs: i64) -> u64 {
    let id = now() as u64;
    let ev = Event {
        id,
        ts: now(),
        kind,
        priority,
        ttl_secs,
        node: node.to_string(),
        detail: detail.to_string(),
        payload,
        state: State::Open,
        note: String::new(),
    };
    if let Some(d) = dir().parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let _ = std::fs::create_dir_all(dir());
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path())
    {
        if let Ok(l) = serde_json::to_string(&ev) {
            let _ = writeln!(f, "{l}");
        }
    }
    id
}

/// 读全部事件（时间线）。
pub fn timeline() -> Vec<Event> {
    std::fs::read_to_string(log_path())
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<Event>(l).ok())
        .collect()
}

/// 待处理的事件（QoS 过滤：过期剔除 + 按 (kind,node) 去重 + 按优先级排序）。
///
/// 这就是「批量 + 时效 + 去重」的落点：慢系统一次读到的是
///   **当前仍然成立的问题集合**，而不是一堆重复的历史报告。
pub fn inbox() -> Vec<Event> {
    let now = now();
    let mut seen: std::collections::HashSet<(Kind, String)> = std::collections::HashSet::new();
    let mut out: Vec<Event> = Vec::new();
    for e in timeline() {
        if e.state != State::Open {
            continue;
        }
        if e.expired(now) {
            continue;
        }
        if !seen.insert((e.kind, e.node.clone())) {
            continue;
        }
        out.push(e);
    }
    out.sort_by(|a, b| b.priority.cmp(&a.priority).then(a.ts.cmp(&b.ts)));
    out
}

/// 确认接手。
pub fn acknowledge(id: u64, note: &str) -> bool {
    set_state(id, State::Acknowledged, note)
}

/// 确认已解决（**正反馈回路**：修「升级只有单向确认」的弱点）。
pub fn resolve(id: u64, note: &str) -> bool {
    set_state(id, State::Resolved, note)
}

fn set_state(id: u64, st: State, note: &str) -> bool {
    let mut all = timeline();
    let mut hit = false;
    for e in all.iter_mut() {
        if e.id == id {
            e.state = st;
            e.note = note.to_string();
            hit = true;
        }
    }
    if !hit {
        return false;
    }
    let mut buf = String::new();
    for e in &all {
        if let Ok(l) = serde_json::to_string(e) {
            buf.push_str(&l);
            buf.push('\n');
        }
    }
    let _ = std::fs::write(log_path(), buf);
    true
}

/// 回放（慢系统复盘用）：取某时间点之后的事件。
///
/// 修「慢系统复盘节奏未定」的弱点：慢系统可以按周期回放这一局的事件流，
///   看问题是怎么出现、升级、被处理的 —— 而不是只看当前快照。
pub fn replay(since_ts: i64) -> Vec<Event> {
    timeline().into_iter().filter(|e| e.ts >= since_ts).collect()
}

/// 人话摘要。
pub fn summary() -> String {
    let inbox = inbox();
    if inbox.is_empty() {
        return "📡 协同总线：没有待处理事件".to_string();
    }
    inbox
        .iter()
        .map(|e| {
            format!(
                "{}·{} [{}] {}{}",
                match e.priority {
                    Priority::Critical => "🔴",
                    Priority::Warn => "🟡",
                    Priority::Info => "⚪",
                },
                e.kind.label(),
                e.node,
                e.detail,
                if e.ttl_secs > 0 {
                    format!("（时效 {}s）", e.ttl_secs)
                } else {
                    String::new()
                }
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 便捷：发布一条带上下文的卡住事件。
pub fn publish_stuck(node: &str, detail: &str, ctx: (i64, usize, f64, f64), ttl_secs: i64) -> u64 {
    let (cycle, dupes, o2, food) = ctx;
    publish(
        Kind::StuckDetected,
        Priority::Warn,
        node,
        detail,
        json!({"cycle": cycle, "dupes": dupes, "oxygen_kg": o2, "food_kcal": food}),
        ttl_secs,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priority_ordering() {
        assert!(Priority::Critical > Priority::Warn);
        assert!(Priority::Warn > Priority::Info);
    }

    #[test]
    fn ttl_expiry() {
        let e = Event {
            id: 1, ts: 100, kind: Kind::Escalated, priority: Priority::Warn,
            ttl_secs: 10, node: String::new(), detail: String::new(),
            payload: Value::Null, state: State::Open, note: String::new(),
        };
        assert!(!e.expired(105));
        assert!(e.expired(120));
    }
}
