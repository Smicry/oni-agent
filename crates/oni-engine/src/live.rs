//! 跨进程共享的实时快照：守护进程写，主 agent 读。
//!
//! 为什么需要：
//!   守护进程每轮采集 11 个观察源，主 agent 的 `observe`/`plan` 又各采一遍 ——
//!   两边独立打 MCP，是"高频并发查询"的主要来源，也是 503 的推手。
//!   让守护进程当**唯一采集者**，把结果落盘；主 agent 默认读它
//!   MCP 只有一条通道，查询零压力，而且两边看到的是**同一份真值**。
//!
//! 使用：
//! - 守护进程每轮 `sense()` 之后调 [`publish`] 落盘。
//! - 主 agent 调 [`load`] 读最近快照（带新鲜度检查）。
//! - 需要**实时**数据时（要下指令前）仍可直读 —— 但那属于"要动手"，理应承担这次压力。

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::sense::Snapshot;

/// 共享快照文件路径。
pub fn live_snapshot_path() -> PathBuf {
    oni_mcp::save::save_file("live_snapshot.json")
}

/// 落盘当前快照（守护进程每轮调用）。
pub fn publish(snap: &Snapshot) -> std::io::Result<()> {
    let path = live_snapshot_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let json = serde_json::to_string_pretty(snap)
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    // 先写临时文件再改名 —— 避免读到写了一半的文件
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, &path)
}

/// 读取共享快照。`max_age` 是可接受的最大陈旧秒数；超过返回 None。
pub fn load(max_age_secs: u64) -> Option<Snapshot> {
    let path = live_snapshot_path();
    let text = std::fs::read_to_string(&path).ok()?;
    let snap: Snapshot = serde_json::from_str(&text).ok()?;
    // 用文件 mtime 判断新鲜度（比快照里的 ts 可靠）
    let mtime = std::fs::metadata(&path).ok()?.modified().ok()?;
    let age = mtime.duration_since(SystemTime::UNIX_EPOCH).ok()?.as_secs();
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    if now.saturating_sub(age) > max_age_secs {
        return None;
    }
    Some(snap)
}