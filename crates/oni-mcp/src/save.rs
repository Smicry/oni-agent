//! 存档身份 + **按存档隔离的状态目录**。
//!
//! 为什么：
//!   换存档后，上一局的 plan.json（全 done）、目标锁、侦察报告、趋势样本
//!   全被新局继承 → 代理拿着旧计划空转 40 分钟（"依赖图里没有可执行目标"）。
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
    Some(SaveId {
        fingerprint,
        name,
        world,
        world_id,
        anchor,
        first_seen: now(),
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
/// 踩坑：ONI 的自动存档叫「存档名 Cycle 6」，
///   读一次自动存档 = 存档名变了 = 指纹变了 = 代理以为换存档
///   **把正在跑的计划冲成模板**（就这么丢过一次定制计划）。
///   周期号是"第几个周期"，不是"哪一局" ⇒ 不能进指纹。
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

