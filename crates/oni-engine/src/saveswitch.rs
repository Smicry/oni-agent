//! 换存档检测与**自动隔离**。
//!
//! ★ 为什么（2026-11-22 小星局实测）：
//!   换存档后，上一局的 plan.json（全 done）、目标锁、侦察报告、趋势样本
//!   全被新局继承 → 代理拿着旧计划空转 40 分钟（"依赖图里没有可执行目标"）。
//!
//! ★ 隔离动作（换局后**必须**做，否则新局继承旧局的局内状态）：
//!   1. 旧存档的 plan.json 归档到 saves/<旧指纹>/plan.json（保留、可追溯、可回滚）
//!   2. 工作区的 plan.json 按模板重置（全部 todo）并打上**新存档指纹**
//!   3. 当前存档切到新的状态目录 → 目标锁/侦察报告/趋势样本/实时快照自然隔离
//!
//! ★ 指纹写在 plan.json 里（save_fingerprint 字段）→ 判断"这份计划属于哪一局"
//!   不依赖外部记账，且计划文件自证出身（谁把它拷到别的存档里，一眼能看出来）。

use std::path::Path;

use oni_mcp::save::{self, SaveId};
use oni_mcp::McpSession;

/// 一次存档切换。
#[derive(Debug, Clone)]
pub struct Switch {
    pub old: Option<SaveId>,
    pub new: SaveId,
}

/// 计划文件里记录的存档指纹。
pub fn plan_fingerprint(plan_path: &str) -> Option<String> {
    let t = std::fs::read_to_string(plan_path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&t).ok()?;
    v.get("save_fingerprint")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
}

/// 给计划文件打上存档指纹（并保留其余内容）。
pub fn stamp_plan(plan_path: &str, fp: &str) -> bool {
    let Ok(t) = std::fs::read_to_string(plan_path) else {
        return false;
    };
    let Ok(mut v) = serde_json::from_str::<serde_json::Value>(&t) else {
        return false;
    };
    if let Some(obj) = v.as_object_mut() {
        obj.insert(
            "save_fingerprint".to_string(),
            serde_json::Value::from(fp.to_string()),
        );
    }
    std::fs::write(plan_path, serde_json::to_string_pretty(&v).unwrap_or_default()).is_ok()
}

/// 按模板重置计划（并打指纹）。
/// 写回计划（带存档指纹）。
///
/// ★ 重规划与人工编辑都走这一条路径 → 指纹永远正确 → 换局不会误重置。
pub fn write_plan(plan_path: &str, plan: &crate::decide::Plan, fp: Option<&str>) -> bool {
    let Ok(mut v) = serde_json::to_value(plan) else {
        return false;
    };
    if let Some(obj) = v.as_object_mut() {
        if let Some(f) = fp {
            obj.insert(
                "save_fingerprint".to_string(),
                serde_json::Value::from(f.to_string()),
            );
        }
    }
    std::fs::write(plan_path, serde_json::to_string_pretty(&v).unwrap_or_default()).is_ok()
}

pub fn reset_plan(plan_path: &str, fp: &str) -> bool {
    let tpl = save::template_path(plan_path);
    if !tpl.exists() {
        return false;
    }
    if std::fs::copy(&tpl, plan_path).is_err() {
        return false;
    }
    stamp_plan(plan_path, fp);
    true
}

/// 清掉**当前存档**的局内状态（目标锁/侦察报告/趋势样本/实时快照）。
pub fn clear_save_state() {
    for name in [
        "goal_lock.json",
        "scout.json",
        "snapshots.jsonl",
        "live_snapshot.json",
    ] {
        let _ = std::fs::remove_file(save::save_file(name));
    }
}

/// 计划看起来像"上一局残留"吗？
///
/// ★ 判据：**所有节点都已是 done/blocked** ⇒ 这份计划在本局没有任何可做的事
///   ⇒ 极可能是上一局的残留（换局未清）⇒ 可以安全按模板重置。
///   反之（还有 todo 节点）就当作**人正在用的计划** ⇒ 只补指纹，不重置。
pub fn plan_looks_stale(plan_path: &str) -> bool {
    let Ok(t) = std::fs::read_to_string(plan_path) else {
        return false;
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&t) else {
        return false;
    };
    let Some(nodes) = v.get("nodes").and_then(|n| n.as_array()) else {
        return false;
    };
    if nodes.is_empty() {
        return true;
    }
    nodes.iter().all(|n| {
        let st = n.get("status").and_then(|s| s.as_str()).unwrap_or("todo");
        st == "done" || st == "blocked" || st == "deferred"
    })
}

/// 每轮调用：识别当前存档 → 发现切换就自动隔离。
///
/// 返回 Some 表示**这次切换了存档**（调用方应该打一条显眼日志）。
pub fn ensure(session: &McpSession, plan_path: &str) -> Option<Switch> {
    let new = save::identify(session)?;
    let plan_fp = plan_fingerprint(plan_path);
    let last = save::last_seen();

    // 同一存档：只刷新进程内的当前存档指针
    if let Some(l) = &last {
        if l.fingerprint == new.fingerprint && plan_fp.as_deref() == Some(new.fingerprint.as_str()) {
            save::set_current(new);
            return None;
        }
    }

    // 切换了（或计划文件没有出身信息 → 按可疑处理）
    save::set_current(new.clone());
    save::write_meta(&new);

    // 1) 归档旧计划（按**旧指纹**存；没记录过就退回"无名"目录）
    let old_fp = last
        .as_ref()
        .map(|l| l.fingerprint.clone())
        .or_else(|| plan_fp.clone());
    if let Some(fp) = old_fp {
        let dir = save::save_dir(&fp);
        let _ = std::fs::create_dir_all(&dir);
        if Path::new(plan_path).exists() {
            let _ = std::fs::copy(plan_path, dir.join("plan.json"));
        }
    }

    // 2) 重置计划 + 打指纹
    //    ★★ 只有**明显是上一局残留**的计划（所有节点都已 done/blocked）才重置。
    //      否则（人手写的计划没打指纹）**只补打指纹，绝不冲掉人的计划**。
    //      （2026-11-22 实测：一次自动存档读档就把定制计划冲成了模板。）
    if plan_looks_stale(plan_path) {
        reset_plan(plan_path, &new.fingerprint);
    } else {
        stamp_plan(plan_path, &new.fingerprint);
    }

    // 3) 新存档的局内状态本来就是空的（按目录隔离）；老存档的状态留在老目录里
    save::remember(&new);
    Some(Switch { old: last, new })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> String {
        let p = std::env::temp_dir().join(format!("oni-saveswitch-{name}-{}", std::process::id()));
        p.to_string_lossy().to_string()
    }

    #[test]
    fn fingerprint_is_stable_and_discriminating() {
        let a = oni_mcp::save::fingerprint_of(&["saveA", "worldX", "0", "(1,2)"]);
        let b = oni_mcp::save::fingerprint_of(&["saveA", "worldX", "0", "(1,2)"]);
        let c = oni_mcp::save::fingerprint_of(&["saveB", "worldX", "0", "(1,2)"]);
        let d = oni_mcp::save::fingerprint_of(&["saveA", "worldY", "0", "(1,2)"]);
        let e = oni_mcp::save::fingerprint_of(&["saveA", "worldX", "1", "(1,2)"]);
        let f = oni_mcp::save::fingerprint_of(&["saveA", "worldX", "0", "(3,4)"]);
        assert_eq!(a, b, "同样的输入必须得到同样的指纹");
        assert_ne!(a, c, "存档名不同 → 指纹不同");
        assert_ne!(a, d, "世界名不同 → 指纹不同");
        assert_ne!(a, e, "世界 id 不同 → 指纹不同");
        assert_ne!(a, f, "打印舱锚点不同 → 指纹不同");
    }

    #[test]
    fn template_path_derives_from_plan_path() {
        assert_eq!(
            oni_mcp::save::template_path("plan.json"),
            std::path::PathBuf::from("plan.template.json")
        );
        assert_eq!(
            oni_mcp::save::template_path("/x/y/myplan.json"),
            std::path::PathBuf::from("/x/y/myplan.template.json")
        );
    }

    #[test]
    fn stamp_and_read_plan_fingerprint() {
        let p = tmp("stamp");
        std::fs::write(&p, r#"{"nodes":[]}"#).unwrap();
        assert_eq!(plan_fingerprint(&p), None);
        assert!(stamp_plan(&p, "abc123"));
        assert_eq!(plan_fingerprint(&p).as_deref(), Some("abc123"));
        // 计划内容不能被破坏
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert!(v.get("nodes").is_some());
        let _ = std::fs::remove_file(&p);
    }
}

