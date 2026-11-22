//! OPENING —— 开局规划器：每局的战略由 LLM 依据**本局世界事实**生成。
//!
//! 设计初衷（慢系统契约 §4：慢系统 = 战略层，决定"往哪走"）：
//!   旧实现：plan.json 来自 plan.template.json（硬编码 playbook），
//!   且模板自带 author=ai / locked=true 的**假身份** →
//!   机械 planner（按世界事实分类的策略族）被永久锁死（saveswitch 拒写）→
//!   换一张地图（水世界/金属星/沼泽）节点图就不适用 → 代理带着错误的计划全灭
//!   （实测：拉德多局模板没有产氧节点，"挖通地表换气"又被假 done 提前宣告完成）。
//!
//! 新契约（三段兜底链）：
//!   L1 **LLM 开局规划**：快系统采世界事实（opening_brief.json），
//!      慢系统 LLM 生成节点图（author=ai，名副其实）→ 强校验 → 落盘；
//!   L2 **机械兜底**：LLM 失败/校验不过 → planner::build_plan（按世界事实分类
//!      的策略族，author=auto）走 suggestions 通道供慢系统审阅；
//!   L3 **模板基线**：模板不再锁死机械 planner（author=template），作为最后基线。
//!
//! 通用性关键：prompt 里**只放跨局通用原则**（厕所前置/材料先行/换气判据用真实
//! 可达上界/缺料自举/造出来≠在运转/产氧需电）+ **本局事实**；节点结构由 LLM
//! 依据事实决定（水世界→藻类箱/电解器，陆地→氧石扩散器/挖掘换气）。

use crate::decide::{Node, Plan};
use crate::exec::Action;
use crate::slow::SlowConfig;
use serde_json::Value;

/// 判据 kind 白名单（`check_success` 支持的全部机器可判判据）。
pub const CHECK_KINDS: &[&str] = &[
    "breath_min", "building_min", "dupe_min", "food_min", "generator_fueled",
    "loose_any_min", "loose_min", "loose_multi_min", "oxygen_min", "oxygen_producing",
    "power_net", "power_operational", "prefab_min", "reachable_min", "rooms_min",
    "storage_mass_min", "surface_open", "scout_done", "food_cycles_min",
    "breath_ratio_min", "manual",
];

/// 计划节点允许使用的动作 kind（**禁止带坐标的 build/dig/order/auto_connect**——
/// 写死坐标换个存档就废，这是"playbook 不可通用"的老坑）。
pub const ACTION_KINDS: &[&str] = &["auto", "auto_build", "scout", "call"];

/// 计划不是 AI 生的（模板/机械兜底）→ 需要 LLM 开局规划。
pub fn opening_needed(plan_path: &str) -> bool {
    let Ok(text) = std::fs::read_to_string(plan_path) else { return true; };
    let Ok(v) = serde_json::from_str::<Value>(&text) else { return true; };
    v.get("author").and_then(|a| a.as_str()) != Some("ai")
}

/// 从 LLM 输出里提取 JSON（容忍 ``` 栅栏与前后废话）。
pub fn extract_json(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if end < start { return None; }
    Some(&text[start..=end])
}

/// **计划完整性校验**（防 LLM 幻觉）。返回错误清单；空 = 通过。
///
/// 校验的不是风格，是**会致命的结构问题**：
///   - 判据 kind 不在白名单 → 该节点永远无法判完成（永动或永挂）；
///   - 判据重复 → 一个 done 会连带"证明"另一个（实测 E1/AIR 同判据）；
///   - 产氧节点无供电前置 → 造出来不产氧（实测：制氧1 但净 -120W）；
///   - 缺生命线 → 代理不会去建厕所/床/产氧（实测：床位 0/3 无人建）。
pub fn validate_plan(plan: &Plan) -> Vec<String> {
    let mut errs: Vec<String> = Vec::new();
    let n = plan.nodes.len();
    if !(5..=20).contains(&n) {
        errs.push(format!("节点数 {n} 不在 5~20"));
    }

    let mut ids: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for node in &plan.nodes {
        if !ids.insert(node.id.as_str()) {
            errs.push(format!("节点 id 重复: {}", node.id));
        }
    }
    if let Some(sn) = &plan.survival_node {
        if !ids.contains(sn.as_str()) {
            errs.push(format!("survival_node 指向不存在的节点 {sn}"));
        }
    }
    for node in &plan.nodes {
        for r in &node.requires {
            if !ids.contains(r.as_str()) {
                errs.push(format!("{} requires 不存在的节点 {r}", node.id));
            }
        }
        for u in &node.unlocks {
            if !ids.contains(u.as_str()) {
                errs.push(format!("{} unlocks 不存在的节点 {u}", node.id));
            }
        }
    }

    // 判据：白名单 + 重复检测
    let mut seen: std::collections::HashMap<String, &str> = std::collections::HashMap::new();
    for node in &plan.nodes {
        let Some(v) = &node.success_check else {
            errs.push(format!("{} 缺 success_check（无法判定完成）", node.id));
            continue;
        };
        let kind = v.get("kind").and_then(|k| k.as_str()).unwrap_or("");
        if !CHECK_KINDS.contains(&kind) {
            errs.push(format!("{} 判据 kind 不在白名单: {kind:?}", node.id));
        }
        let key = serde_json::to_string(v).unwrap_or_default();
        if let Some(prev) = seen.get(&key) {
            errs.push(format!("{} 与 {prev} 判据完全重复（依赖图语义冲突）", node.id));
        } else {
            seen.insert(key, node.id.as_str());
        }
    }

    // 动作：白名单 kind + 语义 goal 可解析 + 不写死坐标
    for node in &plan.nodes {
        if node.actions.is_empty() {
            errs.push(format!("{} 无任何动作", node.id));
        }
        for a in &node.actions {
            let kind = match a {
                Action::Auto { .. } => "auto",
                Action::AutoBuild { .. } => "auto_build",
                Action::Scout { .. } => "scout",
                Action::Call { .. } => "call",
                other => {
                    errs.push(format!(
                        "{} 使用了不安全的动作 {:?}（会写死坐标，换个存档就废）",
                        node.id, other
                    ));
                    continue;
                }
            };
            if !ACTION_KINDS.contains(&kind) {
                errs.push(format!("{} 动作 kind 不在白名单: {kind}", node.id));
            }
            if let Action::Auto { goal, .. } = a {
                if crate::autoact::parse_goal(goal).is_none() {
                    errs.push(format!("{} 语义 goal 无法解析: {goal:?}", node.id));
                }
            }
            if let Action::AutoBuild { region, prefab, .. } = a {
                if region != "auto" {
                    errs.push(format!(
                        "{} AutoBuild region 必须为 auto（不得写死坐标），当前 {region:?}",
                        node.id
                    ));
                }
                if prefab.is_empty() {
                    errs.push(format!("{} AutoBuild prefab 为空", node.id));
                }
            }
        }
    }

    // 生命线覆盖（卫生/氧气/食物/床位/电力）
    fn check_of(n: &Node) -> Option<&serde_json::Value> { n.success_check.as_ref() }
    let goal_of = |n: &Node| -> String {
        n.actions.iter().find_map(|a| match a {
            Action::Auto { goal, .. } => Some(goal.clone()),
            _ => None,
        }).unwrap_or_default()
    };
    let has_toilet = plan.nodes.iter().any(|nd| {
        nd.actions.iter().any(|a| matches!(a,
            Action::AutoBuild { prefab, .. } if prefab.contains("Outhouse") || prefab.contains("Toilet")))
            || check_of(nd).map(|c| {
                let key = c.get("key").and_then(|k| k.as_str()).unwrap_or("");
                let kind = c.get("kind").and_then(|k| k.as_str()).unwrap_or("");
                kind == "building_min" && key.contains("toilet")
            }).unwrap_or(false)
    });
    let is_oxygen_node = |nd: &Node| -> bool {
        check_of(nd).map(|c| {
            let kind = c.get("kind").and_then(|k| k.as_str()).unwrap_or("");
            matches!(kind, "oxygen_producing" | "oxygen_min" | "surface_open")
        }).unwrap_or(false)
            || matches!(goal_of(nd).as_str(), "oxygen" | "surface" | "open_chamber")
    };
    let has_food = plan.nodes.iter().any(|nd| {
        check_of(nd).map(|c| {
            let kind = c.get("kind").and_then(|k| k.as_str()).unwrap_or("");
            matches!(kind, "food_cycles_min" | "food_min")
        }).unwrap_or(false)
            || matches!(goal_of(nd).as_str(), "forage")
            || goal_of(nd).starts_with("element:Algae")
    });
    let has_bed = plan.nodes.iter().any(|nd| {
        nd.actions.iter().any(|a| matches!(a,
            Action::AutoBuild { prefab, .. } if prefab.contains("Bed")))
            || check_of(nd).map(|c| {
                let key = c.get("key").and_then(|k| k.as_str()).unwrap_or("");
                let kind = c.get("kind").and_then(|k| k.as_str()).unwrap_or("");
                kind == "building_min" && key.contains("bed")
            }).unwrap_or(false)
    });
    let has_oxygen = plan.nodes.iter().any(|nd| is_oxygen_node(nd));
    if !has_toilet { errs.push("缺生命线：卫生（没有任何厕所节点/判据）".into()); }
    if !has_oxygen { errs.push("缺生命线：氧气（没有任何产氧/换气节点）".into()); }
    if !has_food { errs.push("缺生命线：食物（没有任何食物节点/判据）".into()); }
    if !has_bed { errs.push("缺生命线：居住（没有任何床位节点/判据）".into()); }

    // 耗电产氧必须有供电前置
    let producers: Vec<&str> = plan.nodes.iter()
        .filter(|nd| check_of(nd).map(|c| {
            c.get("kind").and_then(|k| k.as_str()) == Some("oxygen_producing")
        }).unwrap_or(false))
        .map(|nd| nd.id.as_str())
        .collect();
    let powers: Vec<&str> = plan.nodes.iter()
        .filter(|nd| check_of(nd).map(|c| {
            matches!(c.get("kind").and_then(|k| k.as_str()).unwrap_or(""), "power_operational" | "power_net")
        }).unwrap_or(false) || goal_of(nd) == "power")
        .map(|nd| nd.id.as_str())
        .collect();
    if !producers.is_empty() {
        if powers.is_empty() {
            errs.push("产氧节点存在但没有供电节点（耗电产氧件没电 = 废铁，实测制氧1 净-120W）".into());
        } else {
            for p in &producers {
                let node = plan.nodes.iter().find(|nd| nd.id == *p);
                let ok = node.map(|nd| {
                    nd.requires.iter().any(|r| powers.contains(&r.as_str()))
                }).unwrap_or(false);
                if !ok {
                    errs.push(format!("产氧节点 {p} 的 requires 不含供电节点（先电后氧）"));
                }
            }
        }
    }
    errs
}

/// 组装 LLM 的 system / user prompt。原则只放**跨局通用**的；节点结构由事实决定。
fn build_prompts(brief_text: &str, principles: &[String]) -> (String, String) {
    let system = "你是《缺氧》(Oxygen Not Included) 殖民地的开局战略规划器。\
你的任务：根据**本局世界事实**生成一份依赖图计划（JSON）。\
地图类型千差万别（陆地/水世界/金属富集/沼泽/陨石坑…），**禁止套用固定套路**——\
节点结构必须由事实决定（例：水世界优先藻类箱/电解器，陆地富氧石优先氧气扩散器，\
地表近优先打通换气口，深埋优先竖井+梯子）。\
只输出 JSON，不要任何其它文字、不要 markdown 栅栏。".to_string();

    let user = format!(
        "== 本局世界事实 ==\n{brief_text}\n\n== 跨局通用原则（任何地图都适用）==\n{}\n\n== 硬性约束 ==\n\
1. 产氧设备（氧气扩散器/电解器）需要电力 → 供电节点的 id 必须出现在产氧节点的 requires 里；\n2. 若走研究路线（电解器/藻类箱被研究锁时），必须有建研究站的节点；\n3. 判据用终局量：产氧 = {{kind:oxygen_producing}}；换气 = {{kind:surface_open,minY:<地表Y>}}；\n   卫生 = {{kind:building_min,key:toilets,n:<人数>}}；床位 = {{kind:building_min,key:beds,n:<人数>}}；\n   供电 = {{kind:power_operational}}；食物 = {{kind:food_cycles_min,cycles:<N>}}；\n4. 不得写死坐标：AutoBuild 的 region 一律 \"auto\"；禁止 build/dig/order/auto_connect 动作；\n5. 语义 goal 只能取：scout/material/surface/oxygen/expand/open_chamber/power/research/forage/mop_dirty/element:<元素名>；\n6. 节点数 6~14；requires/unlocks 引用必须存在且构成 DAG；每个节点 1~3 个动作；\n7. 必须覆盖五条生命线：卫生、氧气、食物、居住(床)、电力；survival_node 指向换气/产氧节点。\n\n== 输出 schema（字段名必须完全一致）==\n{{\n\
  \"objective\": \"一句话\", \"survival_node\": \"<id>\", \"author\": \"ai\", \"locked\": true,\n\
  \"nodes\": [{{\"id\": \"T1\", \"name\": \"...\", \"status\": \"todo\", \"requires\": [], \"unlocks\": [],\n\
    \"actions\": [{{\"kind\": \"auto\", \"goal\": \"...\", \"why\": \"...\"]}},\n\
    \"success_check\": {{...}}, \"success\": \"人类可读\", \"note\": \"\"]}}\n}}",
        principles.iter().map(|p| format!("- {p}")).collect::<Vec<_>>().join("\n")
    );
    (system, user)
}

/// 开局规划编排：LLM 生成 → 强校验 → 落盘（保留 objective/principles 等附加字段）。
/// 返回人类可读的结果摘要；Err = 失败原因（调用方自行兜底）。
pub fn author_opening_plan(
    cfg: &SlowConfig,
    brief_text: &str,
    plan_path: &str,
    fp: Option<&str>,
    principles: &[String],
) -> Result<String, String> {
    let (system, user) = build_prompts(brief_text, principles);
    let raw = crate::slow::call_llm(cfg, &system, &user)?;
    let json_text = extract_json(&raw).ok_or("LLM 输出里没有 JSON")?;
    let mut raw_v: Value = serde_json::from_str(json_text).map_err(|e| format!("计划 JSON 解析失败: {e}"))?;
    let plan: Plan = serde_json::from_value(raw_v.clone()).map_err(|e| format!("计划 schema 不合法: {e}"))?;
    let errs = validate_plan(&plan);
    if !errs.is_empty() {
        return Err(format!("计划校验未通过（{} 条）：{}", errs.len(), errs.join("；")));
    }
    // 名副其实：这份计划才是 AI 生的 → author=ai / locked=true
    if let Some(obj) = raw_v.as_object_mut() {
        obj.insert("author".into(), Value::from("ai"));
        obj.insert("locked".into(), Value::from(true));
        if let Some(f) = fp {
            obj.insert("save_fingerprint".into(), Value::from(f.to_string()));
        }
    }
    // 保留旧计划里 done 且判据仍成立的节点进度（同 id 同判据才算）
    if let (Ok(old_text), Some(nodes)) = (
        std::fs::read_to_string(plan_path),
        raw_v.get_mut("nodes").and_then(|n| n.as_array_mut()),
    ) {
        if let Ok(old) = serde_json::from_str::<Value>(&old_text) {
            for n in nodes.iter_mut() {
                let (id, check) = (
                    n.get("id").cloned(),
                    n.get("success_check").cloned(),
                );
                let (Some(id), Some(check)) = (id, check) else { continue };
                let still = old.get("nodes").and_then(|o| o.as_array()).and_then(|arr| {
                    arr.iter().find(|m| {
                        m.get("id").and_then(|i| i.as_str()) == id.as_str()
                            && m.get("status").and_then(|s| s.as_str()) == Some("done")
                            && serde_json::to_string(m.get("success_check").unwrap_or(&Value::Null)).ok()
                                == serde_json::to_string(&check).ok()
                    })
                });
                if let Some(m) = still {
                    if let Some(o) = n.as_object_mut() {
                        o.insert("status".into(), Value::from("done"));
                        let _ = o.insert("success".into(), m.get("success").cloned().unwrap_or(Value::Null));
                    }
                }
            }
        }
    }
    std::fs::write(plan_path, serde_json::to_string_pretty(&raw_v).unwrap_or_default())
        .map_err(|e| format!("写盘失败: {e}"))?;
    Ok(format!("LLM 开局规划已生成（{} 节点，判据校验通过）", plan.nodes.len()))
}

/// 从模板读跨局通用原则（作为 prompt 素材；模板的**节点图**不再被复制为计划）。
pub fn collect_principles(template_path: &str) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(template_path) else { return Vec::new(); };
    let Ok(v) = serde_json::from_str::<Value>(&text) else { return Vec::new(); };
    v.get("principles")
        .and_then(|p| p.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

/// 计划是否不是 AI 生的（用于慢系统触发开局规划）。
pub fn plan_author_is_ai(plan_text: &str) -> bool {
    serde_json::from_str::<Value>(plan_text).ok()
        .and_then(|v| v.get("author").and_then(|a| a.as_str()).map(|s| s == "ai"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decide::Plan;

    fn node(id: &str, check: Option<Value>) -> Node {
        Node {
            id: id.into(),
            name: id.into(),
            status: "todo".into(),
            requires: vec![],
            unlocks: vec![],
            actions: vec![serde_json::from_value(serde_json::json!({
                "kind": "auto", "goal": "material", "why": ""
            })).unwrap()],
            success_check: check,
            success: String::new(),
            rescue: false,
            note: String::new(),
            priority: None,
            deferred: None,
            deferred_at: None,
            stall: None,
        }
    }

    fn toilet_check() -> Value {
        serde_json::json!({"kind": "building_min", "key": "toilets", "n": 2})
    }
    fn power_check() -> Value {
        serde_json::json!({"kind": "power_operational"})
    }
    fn oxygen_producing_check() -> Value {
        serde_json::json!({"kind": "oxygen_producing"})
    }

    /// 合法计划：五条生命线齐 + 产氧带供电前置 → 校验通过。
    #[test]
    fn valid_plan_passes() {
        let mut toilet = node("T1", Some(toilet_check()));
        toilet.actions = vec![serde_json::from_value(serde_json::json!({
            "kind": "auto_build", "prefab": "Outhouse", "region": "auto", "why": ""
        })).unwrap()];
        let mut power = node("P1", Some(power_check()));
        power.actions = vec![serde_json::from_value(serde_json::json!({
            "kind": "auto", "goal": "power", "why": ""
        })).unwrap()];
        let mut oxygen = node("O1", Some(oxygen_producing_check()));
        oxygen.requires = vec!["P1".into()];
        oxygen.actions = vec![serde_json::from_value(serde_json::json!({
            "kind": "auto_build", "prefab": "MineralDeoxidizer", "region": "auto", "why": ""
        })).unwrap()];
        let mut bed = node("B1", Some(serde_json::json!({"kind": "building_min", "key": "beds", "n": 3})));
        bed.actions = vec![serde_json::from_value(serde_json::json!({
            "kind": "auto_build", "prefab": "Bed", "region": "auto", "why": ""
        })).unwrap()];
        let mut food = node("F1", Some(serde_json::json!({"kind": "food_cycles_min", "cycles": 5})));
        food.actions = vec![serde_json::from_value(serde_json::json!({
            "kind": "auto", "goal": "forage", "why": ""
        })).unwrap()];
        let plan = Plan {
            nodes: vec![toilet, power, oxygen, bed, food],
            survival_node: Some("O1".into()),
            author: Some("ai".into()),
            locked: true,
        };
        let errs = validate_plan(&plan);
        assert!(errs.is_empty(), "合法计划不应报错: {errs:?}");
    }

    /// 产氧节点缺供电前置 → 必须报错（实测：制氧1 净 -120W 不产氧）。
    #[test]
    fn oxygen_without_power_dependency_rejected() {
        let mut oxygen = node("O1", Some(oxygen_producing_check()));
        oxygen.actions = vec![serde_json::from_value(serde_json::json!({
            "kind": "auto_build", "prefab": "MineralDeoxidizer", "region": "auto", "why": ""
        })).unwrap()];
        let mut toilet = node("T1", Some(toilet_check()));
        toilet.actions = vec![serde_json::from_value(serde_json::json!({
            "kind": "auto_build", "prefab": "Outhouse", "region": "auto", "why": ""
        })).unwrap()];
        let mut bed = node("B1", Some(serde_json::json!({"kind": "building_min", "key": "beds", "n": 1})));
        bed.actions = vec![serde_json::from_value(serde_json::json!({
            "kind": "auto_build", "prefab": "Bed", "region": "auto", "why": ""
        })).unwrap()];
        let mut food = node("F1", Some(serde_json::json!({"kind": "food_cycles_min", "cycles": 5})));
        food.actions = vec![serde_json::from_value(serde_json::json!({
            "kind": "auto", "goal": "forage", "why": ""
        })).unwrap()];
        let plan = Plan {
            nodes: vec![toilet, oxygen, bed, food],
            survival_node: None,
            author: None,
            locked: false,
        };
        let errs = validate_plan(&plan);
        assert!(errs.iter().any(|e| e.contains("供电")),
            "产氧节点缺供电前置必须报错: {errs:?}");
    }

    /// 判据重复 → 必须报错（实测 E1/AIR 同判据，一个假 done 连坐）。
    #[test]
    fn duplicate_criteria_rejected() {
        let mut a = node("AIR", Some(serde_json::json!({"kind": "surface_open", "minY": 216})));
        a.actions = vec![serde_json::from_value(serde_json::json!({
            "kind": "auto", "goal": "surface", "why": ""
        })).unwrap()];
        let mut b = node("E1", Some(serde_json::json!({"kind": "surface_open", "minY": 216})));
        b.actions = vec![serde_json::from_value(serde_json::json!({
            "kind": "auto", "goal": "surface", "why": ""
        })).unwrap()];
        let mut toilet = node("T1", Some(toilet_check()));
        toilet.actions = vec![serde_json::from_value(serde_json::json!({
            "kind": "auto_build", "prefab": "Outhouse", "region": "auto", "why": ""
        })).unwrap()];
        let mut bed = node("B1", Some(serde_json::json!({"kind": "building_min", "key": "beds", "n": 1})));
        bed.actions = vec![serde_json::from_value(serde_json::json!({
            "kind": "auto_build", "prefab": "Bed", "region": "auto", "why": ""
        })).unwrap()];
        let mut oxygen = node("O1", Some(oxygen_producing_check()));
        oxygen.requires = vec!["P1".into()];
        oxygen.actions = vec![serde_json::from_value(serde_json::json!({
            "kind": "auto_build", "prefab": "MineralDeoxidizer", "region": "auto", "why": ""
        })).unwrap()];
        let mut power = node("P1", Some(power_check()));
        power.actions = vec![serde_json::from_value(serde_json::json!({
            "kind": "auto", "goal": "power", "why": ""
        })).unwrap()];
        let mut food = node("F1", Some(serde_json::json!({"kind": "food_cycles_min", "cycles": 5})));
        food.actions = vec![serde_json::from_value(serde_json::json!({
            "kind": "auto", "goal": "forage", "why": ""
        })).unwrap()];
        let plan = Plan {
            nodes: vec![a, b, toilet, bed, oxygen, power, food],
            survival_node: None,
            author: None,
            locked: false,
        };
        let errs = validate_plan(&plan);
        assert!(errs.iter().any(|e| e.contains("重复")),
            "判据重复必须报错: {errs:?}");
    }

    /// 写死坐标的动作 → 必须报错（playbook 不可通用的老坑）。
    #[test]
    fn hardcoded_region_rejected() {
        let mut bad = node("X1", Some(toilet_check()));
        bad.actions = vec![serde_json::from_value(serde_json::json!({
            "kind": "auto_build", "prefab": "Outhouse", "region": "124,194,137,197", "why": ""
        })).unwrap()];
        let mut bed = node("B1", Some(serde_json::json!({"kind": "building_min", "key": "beds", "n": 1})));
        bed.actions = vec![serde_json::from_value(serde_json::json!({
            "kind": "auto_build", "prefab": "Bed", "region": "auto", "why": ""
        })).unwrap()];
        let mut oxygen = node("O1", Some(oxygen_producing_check()));
        oxygen.requires = vec!["P1".into()];
        oxygen.actions = vec![serde_json::from_value(serde_json::json!({
            "kind": "auto_build", "prefab": "MineralDeoxidizer", "region": "auto", "why": ""
        })).unwrap()];
        let mut power = node("P1", Some(power_check()));
        power.actions = vec![serde_json::from_value(serde_json::json!({
            "kind": "auto", "goal": "power", "why": ""
        })).unwrap()];
        let mut food = node("F1", Some(serde_json::json!({"kind": "food_cycles_min", "cycles": 5})));
        food.actions = vec![serde_json::from_value(serde_json::json!({
            "kind": "auto", "goal": "forage", "why": ""
        })).unwrap()];
        let plan = Plan {
            nodes: vec![bad, bed, oxygen, power, food],
            survival_node: None,
            author: None,
            locked: false,
        };
        let errs = validate_plan(&plan);
        assert!(errs.iter().any(|e| e.contains("auto")),
            "写死 region 必须报错: {errs:?}");
    }

    /// 缺生命线（无氧气节点）→ 必须报错。
    #[test]
    fn missing_oxygen_lifeline_rejected() {
        let mut toilet = node("T1", Some(toilet_check()));
        toilet.actions = vec![serde_json::from_value(serde_json::json!({
            "kind": "auto_build", "prefab": "Outhouse", "region": "auto", "why": ""
        })).unwrap()];
        let mut bed = node("B1", Some(serde_json::json!({"kind": "building_min", "key": "beds", "n": 1})));
        bed.actions = vec![serde_json::from_value(serde_json::json!({
            "kind": "auto_build", "prefab": "Bed", "region": "auto", "why": ""
        })).unwrap()];
        let mut power = node("P1", Some(power_check()));
        power.actions = vec![serde_json::from_value(serde_json::json!({
            "kind": "auto", "goal": "power", "why": ""
        })).unwrap()];
        let mut food = node("F1", Some(serde_json::json!({"kind": "food_cycles_min", "cycles": 5})));
        food.actions = vec![serde_json::from_value(serde_json::json!({
            "kind": "auto", "goal": "forage", "why": ""
        })).unwrap()];
        let plan = Plan {
            nodes: vec![toilet, bed, power, food],
            survival_node: None,
            author: None,
            locked: false,
        };
        let errs = validate_plan(&plan);
        assert!(errs.iter().any(|e| e.contains("氧气")),
            "缺氧气生命线必须报错: {errs:?}");
    }

    /// opening_needed：无文件 / 非 AI 计划 → true；AI 计划 → false。
    #[test]
    fn opening_needed_semantics() {
        let tmp = std::env::temp_dir().join(format!("oni_opening_{}.json", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        assert!(opening_needed(tmp.to_str().unwrap()), "无计划文件 → 需要开局规划");
        std::fs::write(&tmp, r#"{"author": "template"}"#).unwrap();
        assert!(opening_needed(tmp.to_str().unwrap()), "模板计划 → 需要开局规划");
        std::fs::write(&tmp, r#"{"author": "ai"}"#).unwrap();
        assert!(!opening_needed(tmp.to_str().unwrap()), "AI 计划 → 不需要");
        let _ = std::fs::remove_file(&tmp);
    }


    /// extract_json：容忍栅栏与前后废话。
    #[test]
    fn extract_json_tolerates_fences() {
        let fenced = "这是计划：\n\n{ \"goal\": 1 }";
        assert_eq!(extract_json(fenced), Some("{ \"goal\": 1 }"));
        let plain = "prefix { \"ok\": true } suffix";
        assert_eq!(extract_json(plain), Some("{ \"ok\": true }"));
        assert_eq!(extract_json("no braces here"), None);
    }


}
