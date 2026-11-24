//! 存档身份 + **按存档隔离的状态目录**。
//!
//! 为什么：状态不按存档隔离时，原存档的 plan.json（全 done）、目标锁、侦察报告、趋势样本
//!   全被新局继承 → 代理拿着旧计划空转。
//!
//! 记忆分层（三层，各司其职，别混）：
//!   1. **局内缓存**（本存档）：saves/<指纹>/ 下的 plan 归档 / 目标锁 / 侦察报告 /
//!      趋势样本 / 实时快照。换局自动丢弃，不跨局污染。
//!   2. **跨局可复用状态**（进程级）：catalog.json（建造目录）、mcp_slots（闸门）、
//!      hold / open_window（进程控制）—— 与存档无关，跨局复用。
//!   3. **通用记忆**（仓库文件）：AGENTS.md（硬规则）、docs/knowledge-base.md（机制知识）、
//!      records/optimization-backlog.md（待优化清单）。
//!      局内发现先落在 records/session-*.md 或 saves/<指纹>/notes.md，
//!      **确认可复用后**再上提到第 3 层（oni promote）。

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

use crate::session::McpSession;

/// 存档身份。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SaveId {
    /// 指纹：存档名 + 世界名 + 世界 id + 打印舱锚点 的哈希。
    ///
    /// 只用**存档内不变**的事实：换存档可能同名，
    /// 用 dupe id / 建筑数 / 周期数都会误判（dupe 会死会来，建筑会越建越多）。
    pub fingerprint: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub world: String,
    #[serde(default)]
    pub world_id: i64,
    /// 打印舱锚点（唯一新人来源，存档内位置不变）
    #[serde(default)]
    pub anchor: String,
    /// 首次见到这个存档的时间（unix 秒）
    #[serde(default)]
    pub first_seen: i64,
    /// 当前加载的**存档文件**（mod `game_control launch status` 的 activeSaveFile）。
    ///
    /// 比"存档名"强得多：同名存档 / 同一局读到更早的自动存档，文件路径都不同 → 时间线不同。
    /// 指纹公式**不含**它（改公式会让所有存档首次运行都被判成新局）；切换判定单独比这个字段。
    #[serde(default)]
    pub save_file: String,
    #[serde(default)]
    pub save_size: u64,
    #[serde(default)]
    pub save_mtime: i64,
}

/// 从 `game_control launch status` 文本解析 activeSaveFile（当前加载的存档文件路径）。
pub fn parse_active_save_file(text: &str) -> Option<String> {
    text.lines()
        .find(|l| l.trim_start().starts_with("activeSaveFile:"))
        .and_then(|l| l.splitn(2, ':').nth(1))
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// 存档文件元信息 (大小, mtime unix 秒)。文件不可达返回 (0, 0)（存档在别的机器/云路径时）。
pub fn file_meta(path: &str) -> (u64, i64) {
    std::fs::metadata(path)
        .map(|m| {
            let mt = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            (m.len(), mt)
        })
        .unwrap_or((0, 0))
}

static CURRENT: OnceLock<Mutex<Option<SaveId>>> = OnceLock::new();

fn slot() -> &'static Mutex<Option<SaveId>> {
    CURRENT.get_or_init(|| Mutex::new(None))
}

/// 进程内"当前存档"（由 ensure_save 每轮刷新）。
pub fn current() -> Option<SaveId> {
    slot().lock().ok().and_then(|g| g.clone())
}

/// 设置进程内当前存档。
pub fn set_current(id: SaveId) {
    if let Ok(mut g) = slot().lock() {
        *g = Some(id);
    }
}

/// 所有存档的状态目录根。
pub fn saves_dir() -> PathBuf {
    crate::state_dir().join("saves")
}

/// 某个存档的状态目录。
pub fn save_dir(fp: &str) -> PathBuf {
    saves_dir().join(fp)
}

/// **局内**状态文件路径（按当前存档指纹隔离）。
///
/// 还没识别出存档时退回根目录。
pub fn save_file(name: &str) -> PathBuf {
    match current() {
        Some(id) => save_dir(&id.fingerprint).join(name),
        None => crate::state_dir().join(name),
    }
}

/// **跨局可复用**的状态文件（与存档无关）。
pub fn shared_file(name: &str) -> PathBuf {
    crate::state_dir().join(name)
}

/// 上一次见到的存档（跨进程）。
pub fn last_seen() -> Option<SaveId> {
    let p = crate::state_dir().join("current.json");
    let t = std::fs::read_to_string(p).ok()?;
    serde_json::from_str::<SaveId>(&t).ok()
}

/// 记住当前存档。
pub fn remember(id: &SaveId) {
    let p = crate::state_dir().join("current.json");
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(p, serde_json::to_string_pretty(id).unwrap_or_default());
}

/// 指纹。
pub fn fingerprint_of(parts: &[&str]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for p in parts {
        for b in p.as_bytes() {
            h ^= *b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        h ^= 0xff;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{:016x}", h)
}

/// 识别当前存档。
pub fn identify(session: &McpSession) -> Option<SaveId> {
    let text = session
        .call_text(
            "world_editor",
            serde_json::json!({
                "command": "read",
                "path": "/active/index.md",
                "confirm": true,
                "task": "存档身份：存档名 + 世界名 + 世界 id"
            }),
        )
        .ok()?;
    let mut name = String::new();
    let mut world = String::new();
    let mut world_id = 0i64;
    for line in text.lines() {
        let l = line.trim();
        if let Some(v) = l.strip_prefix("- Save:") {
            name = normalize_save_name(v.trim());
        }
        if let Some(v) = l.strip_prefix("- Active World:") {
            let v = v.trim();
            let mut it = v.splitn(2, '(');
            world_id = it.next().unwrap_or("0").trim().parse().unwrap_or(0);
            if let Some(rest) = it.next() {
                world = rest.trim().trim_end_matches(')').trim().to_string();
            }
        }
    }
    if name.is_empty() && world.is_empty() {
        return None;
    }
    let anchor = printing_pod_anchor(session).unwrap_or_default();
    let fingerprint = fingerprint_of(&[&name, &world, &world_id.to_string(), &anchor]);
    // 存档文件身份：同名存档 / 读更早的自动存档 → 路径不同 → 时间线不同（切换判定单独比它）
    let save_file = session
        .call_text("game_control", serde_json::json!({
            "domain": "launch", "action": "status", "task": "存档身份：当前加载的存档文件" }))
        .ok()
        .and_then(|t| parse_active_save_file(&t))
        .unwrap_or_default();
    let (save_size, save_mtime) = file_meta(&save_file);
    Some(SaveId {
        fingerprint,
        name,
        world,
        world_id,
        anchor,
        first_seen: now(),
        save_file,
        save_size,
        save_mtime,
    })
}

/// 打印舱锚点：同一存档内不变、跨存档几乎必变 → 提高指纹区分度。
fn printing_pod_anchor(session: &McpSession) -> Option<String> {
    let text = session
        .call_text(
            "world_editor",
            serde_json::json!({
                "command": "read",
                "path": "/active/buildings/index.md",
                "format": "full",
                "confirm": true,
                "task": "存档身份：打印舱锚点"
            }),
        )
        .ok()?;
    for line in text.lines() {
        if !line.contains("打印舱") && !line.contains("Headquarters") {
            continue;
        }
        if let Some(p) = line.split('|').nth(4) {
            let p = p.trim();
            if p.starts_with('(') {
                return Some(p.to_string());
            }
        }
    }
    None
}

/// 存档名归一化：去掉自动存档的「 Cycle N」后缀。
///
/// ONI 的自动存档叫「存档名 Cycle 6」：周期号是"第几个周期"，不是"哪一局"，不能进指纹——
/// 否则读一次自动存档 = 指纹变了 = 代理以为换存档，正在跑的计划被当成新局。
pub fn normalize_save_name(raw: &str) -> String {
    let t = raw.trim();
    match t.rfind(" Cycle ") {
        Some(i) => {
            let tail = &t[i + 1..];
            if tail.chars().all(|c| c.is_ascii_digit()) && !tail.is_empty() {
                t[..i].trim().to_string()
            } else {
                t.to_string()
            }
        }
        None => t.to_string(),
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 列出已知的存档目录（指纹 + 首次出现时间）。
pub fn known_saves() -> Vec<(String, i64)> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(saves_dir()) else {
        return out;
    };
    for e in rd.flatten() {
        if !e.path().is_dir() {
            continue;
        }
        let fp = e.file_name().to_string_lossy().to_string();
        let meta_path = e.path().join("save.json");
        let seen = std::fs::read_to_string(meta_path)
            .ok()
            .and_then(|t| serde_json::from_str::<SaveId>(&t).ok())
            .map(|m| m.first_seen)
            .unwrap_or(0);
        out.push((fp, seen));
    }
    out.sort_by_key(|(_, t)| *t);
    out
}

/// 把存档身份写进该存档自己的目录（便于事后追溯）。
pub fn write_meta(id: &SaveId) {
    let dir = save_dir(&id.fingerprint);
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(
        dir.join("save.json"),
        serde_json::to_string_pretty(id).unwrap_or_default(),
    );
}


#[cfg(test)]
mod tests {
    use super::*;

    /// activeSaveFile 解析：取第一处、去空白、空值不算。
    #[test]
    fn parse_active_save_file_line() {
        let t = "gameInitialized: true\nloaded: true\nactiveSaveFile: /Users/x/Library/陨石坑/陨石坑.sav\nactiveSaveFolder: /Users/x\n";
        assert_eq!(parse_active_save_file(t).as_deref(), Some("/Users/x/Library/陨石坑/陨石坑.sav"));
        assert_eq!(parse_active_save_file("activeSaveFile: \n"), None);
        assert_eq!(parse_active_save_file("nope"), None);
    }

    /// 存档文件元信息：存在 → (大小, mtime)；不可达 → (0, 0)，不 panic。
    #[test]
    fn file_meta_reads_size_and_mtime() {
        let p = std::env::temp_dir().join(format!("oni-save-meta-{}", std::process::id()));
        std::fs::write(&p, b"hello").unwrap();
        let (size, mt) = file_meta(p.to_str().unwrap());
        assert_eq!(size, 5);
        assert!(mt > 1_600_000_000, "mtime={mt}");
        assert_eq!(file_meta("/definitely/not/here.sav"), (0, 0));
        let _ = std::fs::remove_file(&p);
    }
}
