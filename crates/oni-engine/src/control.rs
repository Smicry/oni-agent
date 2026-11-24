//! **暂停所有权**（control ownership）—— 落盘的暂停事件，跨进程重启可见。
//!
//! 唯一的用户停止信号是 **hold**（AI 总开关，Owner::Hold）：
//!   - hold：冻结世界 + 代理待命，重载/重启后保持，直到 release；
//!   - 其余 owner（agent/none/user/escalation）的暂停，代理一律自主恢复 + 开施工窗口，
//!     长线任务不被打断。用户不再需要单独暂停游戏。
//!
//! 落盘 owner + reason + since（按存档隔离，跨进程重启保留），供守护进程读取。
//! 主 agent 改代码：`oni hold` / `oni release`（与 AI 总开关同一机制）。

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// 谁暂停的（持久化到 control.json）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Owner {
    /// 用户手动 / UserPromptSubmit hook（AI 开启时**会被代理自主恢复**——
    ///   只有 `Hold` 是不可越权的停止信号）
    User,
    /// 代理自己（施工窗口到期 / 空闲停机 / 卡住停工）
    Agent,
    /// 主 agent 改代码（oni hold；只有 release 能解）
    Hold,
    /// 等慢系统（仅作审计留痕，**不构成冻结态**：代理同样会自主恢复）
    Escalation,
    /// 没人暂停（世界在跑）
    None,
}

impl Owner {
    pub fn label(&self) -> &'static str {
        match self {
            Owner::User => "用户/hook",
            Owner::Agent => "代理",
            Owner::Hold => "主 agent hold",
            Owner::Escalation => "等慢系统",
            Owner::None => "运行中",
        }
    }
}

/// 持久化的控制状态。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlState {
    pub owner: Owner,
    pub reason: String,
    #[serde(default)]
    pub since: i64,
}

impl Default for ControlState {
    fn default() -> Self {
        Self { owner: Owner::None, reason: String::new(), since: 0 }
    }
}

/// 路径必须**跨进程稳定**：CLI（`oni pause`/`window`）与守护进程是两个进程，
///   `save::current()` 是**进程内**状态 —— CLI 没跑 ensure 时是 None，
///   会把 control.json 写到 state 根，而守护进程写到 saves/<fp>/ → 两边读不到对方的 owner。
///   约束：用 `last_seen()`（从 meta 文件读，跨进程一致）拼存档目录。
fn path() -> PathBuf {
    match oni_mcp::save::last_seen() {
        Some(id) => oni_mcp::save::save_dir(&id.fingerprint).join("control.json"),
        // 还没识别存档（首次启动）→ 退回根目录，保证 CLI 与 agent 至少路径一致
        None => oni_mcp::state_dir().join("control.json"),
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 读取当前控制状态。文件不存在/损坏 → 视为 Owner::None（不臆造暂停）。
pub fn read() -> ControlState {
    std::fs::read_to_string(path())
        .ok()
        .and_then(|t| serde_json::from_str::<ControlState>(&t).ok())
        .unwrap_or_default()
}

/// 记录一次暂停（谁暂停的 + 为什么）。原子写，避免读到半个文件。
pub fn pause(owner: Owner, reason: &str) {
    let st = ControlState { owner, reason: reason.to_string(), since: now() };
    let p = path();
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let tmp = p.with_extension("json.tmp");
    if std::fs::write(&tmp, serde_json::to_string_pretty(&st).unwrap_or_default()).is_ok() {
        let _ = std::fs::rename(&tmp, &p);
    }
    // 效果回执：每次暂停都进统一事件流（可审计"谁在什么时候暂停了世界"）
    let _ = crate::bus::publish(
        crate::bus::Kind::AuditFinding,
        crate::bus::Priority::Info,
        "control",
        &format!("暂停 owner={} reason={}", owner.label(), reason),
        serde_json::json!({"owner": owner.label(), "reason": reason}),
        0,
    );
}

/// 解除暂停：只有 owner 匹配（或者 force）才清掉，防止越权恢复。
/// 返回是否真的解除了。
pub fn resume(expected: Owner) -> bool {
    let st = read();
    // hold/release is a slow-system action; allow it even if a user hook pause
    // has since overwritten the owner (both are freeze intents, not conflicting work).
    let compatible = st.owner == Owner::None || st.owner == expected
        || (expected == Owner::Hold && st.owner == Owner::User);
    if !compatible {
        eprintln!("⛔ 拒绝恢复：暂停 owner={} 与请求方 {} 不匹配", st.owner.label(), expected.label());
        return false;
    }
    let p = path();
    let _ = std::fs::write(&p, serde_json::to_string_pretty(&ControlState::default()).unwrap_or_default());
    let _ = crate::bus::publish(
        crate::bus::Kind::Resolved,
        crate::bus::Priority::Info,
        "control",
        &format!("恢复（原 owner={}）", st.owner.label()),
        serde_json::json!({"previous_owner": st.owner.label()}),
        0,
    );
    true
}

/// 无条件清除（用户显式 `oni resume` / 换存档时用）。
pub fn clear() {
    let _ = std::fs::write(path(), serde_json::to_string_pretty(&ControlState::default()).unwrap_or_default());
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unknown_is_not_paused() {
        // 没有文件时不臆造"用户暂停"（否则重启即死锁）
        assert_eq!(ControlState::default().owner, Owner::None);
    }
    #[test]
    fn owner_labels_stable() {
        assert_eq!(Owner::User.label(), "用户/hook");
        assert_eq!(Owner::Agent.label(), "代理");
    }
}