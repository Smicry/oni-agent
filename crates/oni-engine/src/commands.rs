//! **用户指令通道**：用户直接给助手下指令 → 慢系统翻译成计划/工单 → 快系统执行。
//!
//! ★ 为什么要显式做一个队列（2026-11-22 用户定调）：
//!   "我可以通过下发指令，慢系统规划，来协助完成这个目标"
//!   —— 用户不必等代理自己撞墙，也不必让我猜。
//!
//! ★ 分工：
//!   - **入队**：用户/调用方写一句自然语言（"先挖铁矿"、"别再往下挖"、"多建 2 个厕所"）
//!   - **翻译**：慢系统（AI）读队列 → 判断是「改计划」「直接下工单」还是「改代码」
//!   - **回执**：处理完标记 done + 写一句"我做了什么"，用户能看到指令是否被理解与执行
//!
//! ★ 队列按存档隔离（saves/<指纹>/commands.jsonl）。

use std::io::Write;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// 一条用户指令。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Command {
    pub id: u64,
    /// 指令原文
    pub text: String,
    /// 来源：user / agent
    #[serde(default = "default_source")]
    pub source: String,
    pub ts: i64,
    /// 是否已由慢系统处理
    #[serde(default)]
    pub done: bool,
    /// 慢系统的回执（怎么做的 / 为什么没做）
    #[serde(default)]
    pub result: String,
}

fn default_source() -> String {
    "user".to_string()
}

fn dir() -> PathBuf {
    let fp = oni_mcp::save::current()
        .map(|s| s.fingerprint)
        .unwrap_or_else(|| "unknown".into());
    oni_mcp::save::save_dir(&fp)
}

fn path() -> PathBuf {
    dir().join("commands.jsonl")
}

/// 读全部指令。
pub fn read_all() -> Vec<Command> {
    std::fs::read_to_string(path())
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<Command>(l).ok())
        .collect()
}

/// 未处理的指令（用户/慢系统最先看这个）。
pub fn pending() -> Vec<Command> {
    read_all().into_iter().filter(|c| !c.done).collect()
}

/// 入队一条指令。
pub fn push(text: &str, source: &str) -> u64 {
    let id = now() as u64;
    let cmd = Command {
        id,
        text: text.to_string(),
        source: source.to_string(),
        ts: now(),
        done: false,
        result: String::new(),
    };
    if let Some(d) = dir().parent() {
        let _ = std::fs::create_dir_all(d);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path())
    {
        if let Ok(line) = serde_json::to_string(&cmd) {
            let _ = writeln!(f, "{line}");
        }
    }
    // ★ 同时发到协同总线（用户指令也要进统一时间线）
    let _ = crate::bus::publish(
        crate::bus::Kind::CommandIssued,
        crate::bus::Priority::Critical,
        "user",
        text,
        serde_json::json!({"source": source}),
        7200,
    );
    id
}

/// 标记处理完成 + 写回执。
pub fn complete(id: u64, result: &str) -> bool {
    let mut all = read_all();
    let mut hit = false;
    for c in all.iter_mut() {
        if c.id == id {
            c.done = true;
            c.result = result.to_string();
            hit = true;
        }
    }
    if !hit {
        return false;
    }
    rewrite(all);
    true
}

fn rewrite(all: Vec<Command>) {
    let mut buf = String::new();
    for c in all {
        if let Ok(l) = serde_json::to_string(&c) {
            buf.push_str(&l);
            buf.push('\n');
        }
    }
    let _ = std::fs::write(path(), buf);
}

/// 人话摘要（给慢系统看）。
pub fn summary() -> String {
    let p = pending();
    if p.is_empty() {
        return "📋 无待处理的用户指令".to_string();
    }
    p.iter()
        .map(|c| format!("📌 #{} [{}] {}", c.id, c.source, c.text))
        .collect::<Vec<_>>()
        .join("\n")
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
