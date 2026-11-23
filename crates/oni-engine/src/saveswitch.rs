//! 换存档检测与**自动隔离**。
//!
//! 为什么：
//!   换存档后，上一局的 plan.json（全 done）、目标锁、侦察报告、趋势样本
//!   全被新局继承 → 代理拿着旧计划空转 40 分钟（"依赖图里没有可执行目标"）。
//!
//! 隔离动作：
//!   1. 旧存档的 plan.json 归档到 saves/<旧指纹>/plan.json（保留、可追溯、可回滚）
//!   2. 工作区的 plan.json 按模板重置（全部 todo）并打上**新存档指纹**
//!   3. 当前存档切到新的状态目录 → 目标锁/侦察报告/趋势样本/实时快照自然隔离
//!
//! 指纹写在 plan.json 里（save_fingerprint 字段）→ 判断"这份计划属于哪一局"
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
/// 重规划与人工编辑都走这一条路径 → 指纹永远正确 → 换局不会误重置。
pub fn write_plan(plan_path: &str, plan: &crate::decide::Plan, fp: Option<&str>) -> bool {
    // AI-authored plans are locked: never let the mechanical planner overwrite them.
    if let Ok(old) = std::fs::read_to_string(plan_path) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&old) {
            if v.get("locked").and_then(|x| x.as_bool()).unwrap_or(false)
                && plan.author.as_deref() != Some("ai") {
                eprintln!("⛔ plan.json is AI-authored (locked); mechanical write refused");
                return false;
            }
        }
    }
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
    // ★ 新局不再复制模板 playbook（硬编码套路换地图就不适用，已删除）：
    //   写一份「待 LLM 规划」的占位计划 —— 快系统照常跑保命层，
    //   慢系统 LLM 依据 opening_brief.json（本局世界事实）生成正式计划。
    let v = serde_json::json!({
        "author": "pending_llm",
        "locked": false,
        "save_fingerprint": fp,
        "objective": "（等待 LLM 开局规划：快系统已采集本局世界事实 opening_brief.json）",
        "nodes": [],
    });
    std::fs::write(plan_path, serde_json::to_string_pretty(&v).unwrap_or_default()).is_ok()
}

/// 清掉**当前存档**的局内状态（目标锁/侦察报告/趋势样本/实时快照）。
pub fn clear_save_state() {
    // 局内状态不只是"目标锁/侦察/快照"——决策上下文也是局内的：
    //   decisions.jsonl（LLM 决策滑动窗口）、goal_outcomes.jsonl（熔断统计）、
    //   ai_goals.jsonl（未完成语义目标）、circuit_breaks.jsonl（失败熔断记录）。
    //   不清的话：回滚存档重开同一局时，上一轮的"全灭/倒地/熔断"记录
    //   会进入 LLM 的决策窗口 → 污染新运行的决策走向
    //   （实测缺口：上一局 oxygen fail=4 的熔断会把新局的产氧目标开局就冷却封锁）。
    // ★ 用户指令**归档**而不是删除：读档重开后旧周期的指令已不适用
    //   （"彻底处理二氧化碳"是 Cycle 4 的事，这局才 Cycle 1），继续展示会误导人。
    //   但指令是用户说的，不能丢——移到 commands.archive.jsonl 保留追溯，不再进对话流。
    {
        let src_p = save::save_file("commands.jsonl");
        if let Ok(text) = std::fs::read_to_string(&src_p) {
            if !text.trim().is_empty() {
                let dst = save::save_file("commands.archive.jsonl");
                if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&dst) {
                    use std::io::Write;
                    let _ = writeln!(f, "{}", text.trim_end());
                }
                let _ = std::fs::remove_file(&src_p);
            }
        }
    }
    for name in [
        "goal_lock.json",
        "scout.json",
        "snapshots.jsonl",
        "live_snapshot.json",
        "decisions.jsonl",
        "goal_outcomes.jsonl",
        "ai_goals.jsonl",
        "circuit_breaks.jsonl",
        "opening_state.json",
        "opening_brief.json",
        "opening_brief.done",
        "suggestions.jsonl",
        "escalations/pending.json",
        "escalations/silence.json",
    ] {
        let _ = std::fs::remove_file(save::save_file(name));
    }
}

/// 计划看起来像"上一局残留"吗？
///
/// 判据：**所有节点都已是 done/blocked** ⇒ 这份计划在没有任何可做的事
///   ⇒ 极可能是上一局的残留⇒ 可以安全按模板重置。
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
    //
    // **计划文件里的指纹才是权威**。
    //   旧逻辑要求 meta 的 last_seen **和** 计划指纹同时匹配才算「同一局」；
    //   meta 一旦丢了（换机器 / 换存档读档 / 手改 plan.json 没打指纹）就会误报
    //   「检测到换存档」→ 归档 + 刷屏，
    //   让人以为计划被重置（实际只是补了个指纹）。
    if plan_fp.as_deref() == Some(new.fingerprint.as_str()) {
        save::set_current(new);
        return None;
    }
    if let Some(l) = &last {
        if l.fingerprint == new.fingerprint {
            save::set_current(new);
            return None;
        }
    }

    // 切换了
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
    //    - 计划文件不存在 → 落「待 LLM 规划」占位（新局的第一步）
    //    - 明显是上一局残留（所有节点 done/blocked）→ 按占位重置
    //    - 其它（人在用的计划）→ 只补指纹，绝不冲掉
    if !Path::new(plan_path).exists() {
        reset_plan(plan_path, &new.fingerprint);
    } else if plan_looks_stale(plan_path) {
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

