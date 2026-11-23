//! WINDOW —— 滚动窗口规划器（v3 架构第二层，MPC 式）。
//!
//! 为什么不是静态全局 DAG（v2 的 plan.json 7 节点长链）：
//!   ONI 是**部分可观察 + 随机**的世界——挖开一块石头才知道后面是 CO₂ 还是水。
//!   静态长链把"现在的判断"固化成"未来的约束"，世界一变就成枷锁：
//!   实测 T1 判据一坏 → 整条 requires 链锁死 → 7 个节点全部 done/blocked/deferred
//!   → 殖民地失去方向，只能靠 ai_goals 队列应激驱动。
//!
//! 滚动窗口的做法（和自动驾驶的轨迹规划一样）：
//!   - LLM 每次只规划 **horizon = 3 步**（短视）
//!   - 每步带 pre/post 条件
//!   - **世界变了就重算**：窗口过期（周期推进 / 全部 done / 枷锁）→ 下次心跳重新生成
//!   - 只执行第一步；第二步以后是"意图"，不是"承诺"
//!
//! 保留的全局东西不是"路线"，是"交通规则"（目标函数 + 约束），那在 SYSTEM_PROMPT 与安全壳里。
//!
//! 落盘格式与 v2 兼容（仍是 plan.json 的 nodes 数组），所以 decide.rs / 快系统不用改：
//!   窗口 = 3 个节点的短 DAG，decide() 的选点自然只剩这 3 个可选。

use crate::decide::Plan;
use crate::slow::SlowConfig;
use serde_json::Value;

/// 窗口长度（步数）。3 步 = "现在做 / 下一步 / 再下一步"，够形成方向感又不至于固化。
pub const HORIZON: usize = 3;

/// 窗口失效条件（任一成立 → 下次心跳重算）。
pub fn window_stale(plan: &Plan, snap_cycle: i64) -> Option<String> {
    let nodes = &plan.nodes;
    if nodes.is_empty() {
        return Some("窗口为空".into());
    }
    if nodes.iter().all(|n| n.is_done()) {
        return Some("窗口全部完成".into());
    }
    if nodes.iter().all(|n| n.is_done() || n.is_blocked() || n.is_deferred()) {
        return Some("窗口全部 done/blocked/deferred（枷锁效应）".into());
    }
    let born = plan.window_cycle;
    if born == 0 {
        // window_cycle=0 且非空 → 这是 v2 时代的**静态 DAG**（不是滚动窗口），
        // 必须升级：它的 requires 长链在世界变化后就是枷锁。
        return Some("旧静态 plan（非滚动窗口）→ 升级为 horizon-3 窗口".into());
    }
    if snap_cycle - born >= 2 {
        return Some(format!("窗口已跨 {} 周期", snap_cycle - born));
    }
    None
}

/// 组装滚动窗口的 prompt：本局事实 + 上一窗口的执行结果（反馈闭环）+ 硬约束。
fn build_window_prompt(brief: &str, prev_outcomes: &str) -> (String, String) {
    let system = "你是《缺氧》殖民地的滚动窗口规划器。每次只规划未来 3 步（不是全局计划）。世界是部分可观察的：挖开才知道后面是什么。所以**不要写长链**——只写从当前局面出发、下一步最该做的 3 件事，每步带前置条件（pre）与达成条件（post）。只输出 JSON，不要任何其它文字。".to_string();
    let kinds = crate::opening::CHECK_KINDS.iter().map(|k| format!("`{k}`")).collect::<Vec<_>>().join("/");
    let user = format!(
        "== 本局世界事实 ==\n{brief}\n\n== 上一窗口的执行结果（反馈闭环）==\n{prev_outcomes}\n\n== 硬约束 ==\n1. 恰好 {HORIZON} 个节点，id 为 W1/W2/W3；W1 必须是**现在就能执行**的（pre 已满足）；\n2. 每个节点：{{id, name, requires:[...], actions:[{{kind:\"auto\",goal:\"...\",why:\"...\"}}], success_check:{{...}}, success:\"人类可读\", pre:\"执行前必须成立的条件\", post:\"达成后应成立\"}}；\n3. 语义 goal 只能取：scout/material/surface/oxygen/expand/open_chamber/power/research/forage/mop_dirty/toilet/element:<元素名>；\n4. 判据 kind 只能取：{kinds}；字段格式：loose_min{{element,kg}} / building_min{{key,n}} / prefab_min{{prefab,n}} / oxygen_producing{{}} / power_operational{{}} / reachable_min{{n}} / food_cycles_min{{cycles}} / surface_open{{minY}}；\n5. 不写坐标；AutoBuild 的 region 一律 \"auto\"；\n6. **不要重复上一窗口里已失败的目标**（除非事实变了让它重新可行）——换一条路。\n7. 卫生/氧气/食物/居住/电力五条生命线：哪条最危急就排进 W1，其余按需排进 W2/W3。\n输出 JSON：{{\"objective\":\"一句话\",\"author\":\"ai\",\"locked\":true,\"nodes\":[...]}}",
    );
    (system, user)
}

/// 从 goal_outcomes.jsonl 读最近 N 条执行结果，作为反馈闭环注入 prompt。
fn recent_outcomes(sd: &std::path::Path, n: usize) -> String {
    let Ok(text) = std::fs::read_to_string(sd.join("goal_outcomes.jsonl")) else {
        return "（暂无执行记录）".into();
    };
    let mut lines: Vec<&str> = text.lines().collect();
    lines.reverse();
    let picked: Vec<String> = lines
        .iter()
        .take(n)
        .filter_map(|l| {
            serde_json::from_str::<Value>(l).ok().map(|o| {
                format!(
                    "{} → {}: {}",
                    o.get("goal").and_then(|v| v.as_str()).unwrap_or("?"),
                    if o.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) { "成功" } else { "失败" },
                    o.get("detail").and_then(|v| v.as_str()).unwrap_or("").chars().take(80).collect::<String>()
                )
            })
        })
        .collect();
    if picked.is_empty() { "（暂无执行记录）".into() } else { picked.join("\n") }
}

/// 校验前先**适配 LLM 的常见写法偏差**（契约层容错，而非拒绝）。
///
/// 实测：LLM 把自然语言前置条件（"存在可安全移动的复制人"）写进了 `requires` 数组，
/// 而规范是 `requires` 只放节点 ID、前置条件文本放 `pre` 字段。
/// 拒绝整份窗口会让殖民地继续跑旧 DAG（更糟）；自动迁移才是正确做法。
pub fn adapt_window(plan: &mut Plan) -> Vec<String> {
    let ids: std::collections::HashSet<String> = plan.nodes.iter().map(|n| n.id.clone()).collect();
    let mut notes = Vec::new();
    for n in &mut plan.nodes {
        let (valid, prose): (Vec<String>, Vec<String>) = n
            .requires
            .iter()
            .cloned()
            .partition(|r| ids.contains(r));
        if !prose.is_empty() {
            notes.push(format!("{}: {} 条自然语言前置从 requires 迁移到 pre", n.id, prose.len()));
            n.requires = valid;
            let merged = prose.join("；");
            n.note = if n.note.is_empty() { merged } else { format!("{}；{}", n.note, merged) };
        }
    }
    notes
}

/// 校验滚动窗口（比 opening 的全局校验轻：不要求五条生命线全覆盖，那是安全壳的事）。
pub fn validate_window(plan: &Plan) -> Vec<String> {
    let mut errs = Vec::new();
    if plan.nodes.is_empty() {
        errs.push("窗口为空".into());
        return errs;
    }
    if plan.nodes.len() > HORIZON + 2 {
        errs.push(format!("窗口过长（{} 步），最多 {}", plan.nodes.len(), HORIZON + 2));
    }
    if let Some(w1) = plan.nodes.iter().find(|n| n.id == "W1") {
        if !w1.requires.is_empty() {
            errs.push("W1 不得有 requires（它必须现在就能执行）".into());
        }
    }
    for n in &plan.nodes {
        if n.actions.is_empty() {
            errs.push(format!("{} 无动作", n.id));
        }
        if n.success_check.is_none() {
            errs.push(format!("{} 缺 success_check", n.id));
        }
        for r in &n.requires {
            if !plan.nodes.iter().any(|m| &m.id == r) {
                errs.push(format!("{} requires 不存在的节点 {r}", n.id));
            }
        }
    }
    errs
}

/// 滚动窗口编排：LLM 生成 → 校验 → 落盘（覆盖 plan.json）。
pub fn refresh_window(cfg: &SlowConfig, brief: &str, plan_path: &str, fp: Option<&str>, cycle: i64, sd: &std::path::Path) -> Result<String, String> {
    let (system, user) = build_window_prompt(brief, &recent_outcomes(sd, 6));
    let raw = crate::slow::call_llm(cfg, &system, &user, 8000)?;
    let json_text = crate::opening::extract_json(&raw).ok_or("LLM 输出里没有 JSON")?;
    let mut v: Value = serde_json::from_str(json_text).map_err(|e| format!("窗口 JSON 解析失败: {e}"))?;
    if let Some(o) = v.as_object_mut() {
        o.insert("author".into(), Value::from("ai"));
        o.insert("locked".into(), Value::from(true));
        o.insert("window_cycle".into(), Value::from(cycle));
        if let Some(f) = fp { o.insert("save_fingerprint".into(), Value::from(f.to_string())); }
    }
    let mut plan: Plan = serde_json::from_value(v.clone()).map_err(|e| format!("窗口 schema 不合法: {e}"))?;
    let repairs = crate::opening::repair_plan_checks(&mut plan);
    let adapted = adapt_window(&mut plan);
    let errs = validate_window(&plan);
    if !errs.is_empty() {
        let names: Vec<String> = plan.nodes.iter().map(|n| format!("{}({})", n.id, n.name.chars().take(10).collect::<String>())).collect();
        return Err(format!("窗口校验未通过（{} 条）：{}；节点：{}", errs.len(), errs.join("；"), names.join("、")));
    }
    // 落盘**适配后**的 plan（而不是原始 v）：否则 requires 里的自然语言会原样写回，
    //   decide() 读到 requires 不存在的节点 → 整张窗口被当成死锁。
    let mut out_v = serde_json::to_value(&plan).map_err(|e| format!("窗口序列化失败: {e}"))?;
    if let (Some(o), Some(src_o)) = (out_v.as_object_mut(), v.as_object()) {
        for k in ["window_cycle", "save_fingerprint", "objective"] {
            if let Some(x) = src_o.get(k) { o.insert(k.to_string(), x.clone()); }
        }
    }
    std::fs::write(plan_path, serde_json::to_string_pretty(&out_v).unwrap_or_default())
        .map_err(|e| format!("写盘失败: {e}"))?;
    let mut msg = format!("滚动窗口已刷新（{} 步，周期 {}）", plan.nodes.len(), cycle);
    if !repairs.is_empty() { msg.push_str(&format!("；⚠ {} 条判据降级 manual", repairs.len())); }
    if !adapted.is_empty() { msg.push_str(&format!("；适配 {} 处 requires→pre", adapted.len())); }
    Ok(msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decide::Node;

    fn node(id: &str, status: &str, req: Vec<&str>) -> Node {
        Node {
            id: id.into(), name: id.into(), status: status.into(),
            requires: req.into_iter().map(|s| s.to_string()).collect(),
            unlocks: vec![], actions: vec![], success_check: None, success: String::new(),
            rescue: false, note: String::new(), priority: None, deferred: None, deferred_at: None, stall: None,
        }
    }
    fn plan(nodes: Vec<Node>, cycle: i64) -> Plan {
        Plan { nodes, objective: String::new(), survival_node: None, window_cycle: cycle, ..Default::default() }
    }

    #[test]
    fn stale_when_empty_or_all_done_or_all_stuck() {
        assert!(window_stale(&plan(vec![], 0), 5).is_some());
        let all_done = plan(vec![node("W1", "done", vec![]), node("W2", "done", vec!["W1"])], 5);
        assert!(window_stale(&all_done, 6).is_some(), "全部完成 → 过期");
        let mut stuck = plan(vec![node("W1", "blocked", vec![]), node("W2", "todo", vec!["W1"])], 5);
        stuck.nodes[1].deferred = Some("x".into());
        assert!(window_stale(&stuck, 6).is_some(), "全部 blocked/deferred → 枷锁效应 → 过期");
    }

    #[test]
    fn stale_after_two_cycles_even_if_not_done() {
        let p = plan(vec![node("W1", "todo", vec![])], 10);
        assert!(window_stale(&p, 11).is_none(), "跨 1 周期不过期");
        assert!(window_stale(&p, 12).is_some(), "跨 2 周期 → 世界大概率变了 → 过期");
    }

    #[test]
    fn fresh_window_is_not_stale() {
        let p = plan(vec![node("W1", "todo", vec![]), node("W2", "todo", vec!["W1"]), node("W3", "todo", vec!["W2"])], 10);
        assert!(window_stale(&p, 11).is_none());
    }

    #[test]
    fn w1_must_be_executable_now() {
        let bad = plan(vec![node("W1", "todo", vec!["W2"]), node("W2", "todo", vec![])], 1);
        assert!(validate_window(&bad).iter().any(|e| e.contains("W1 不得有 requires")));
    }
}
