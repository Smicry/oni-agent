//! OPENING —— 开局规划器：每局的战略由 LLM 依据**本局世界事实**生成。
//!
//! 设计初衷（慢系统契约 §4：慢系统 = 战略层，决定"往哪走"）：
//!   旧实现：plan.json 来自 plan.template.json（硬编码 playbook），
//!   且模板自带 author=ai / locked=true 的**假身份** →
//!   机械 planner（按世界事实分类的策略族）被永久锁死（saveswitch 拒写）→
//!   换一张地图（水世界/金属星/沼泽）节点图就不适用 → 代理带着错误的计划全灭
//!   （实测：拉德多局模板没有产氧节点，"挖通地表换气"又被假 done 提前宣告完成）。
//!
//! 新契约（无本地兜底，战略层全面 LLM 化）：
//!   快系统采世界事实（opening_brief.json），
//!   慢系统 LLM 生成节点图（author=ai，名副其实）→ 强校验 → 落盘。
//!   **没有本地兜底**（用户决策：硬编码 playbook 换地图就不适用，已删除）：
//!   LLM 失败/校验不过 → 本轮无战略计划（快系统保命层照常跑），
//!   冷却 10 分钟后重试 LLM；失败原因记入 decisions.jsonl 可见。
//!   原 planner.rs 的机械策略族（playbook）已随本契约删除。
//! 通用性关键：prompt 里**只放跨局通用原则**（厕所前置/材料先行/换气判据用真实
//! 可达上界/缺料自举/造出来≠在运转/产氧需电）+ **本局事实**；节点结构由 LLM
//! 依据事实决定（水世界→藻类箱/电解器，陆地→氧石扩散器/挖掘换气）。

use crate::decide::{Node, Plan};
use crate::exec::Action;
use crate::slow::SlowConfig;
use oni_mcp::McpSession;
use serde::Serialize;
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

/// 开局世界事实（由 CLI 从游戏采集后传入）。
#[derive(Debug, Clone, Default, Serialize)]
pub struct WorldFacts {
    pub dupes: usize,
    /// 官方可达集大小（很小 = 复制人被封住）。
    pub reachable_cells: usize,
    /// 可站区域的高度跨度（行数）。1 = 走廊型。
    pub base_row_span: i32,
    /// 到地表需要挖的格数（None = 未知）。
    pub surface_distance: Option<usize>,
    pub oxy_rock_near: usize,
    pub coal_near: usize,
    pub copper_near: usize,
    pub iron_near: usize,
    pub clay_near: usize,
    pub plants_near: usize,
    /// 基地下方是否有水（深挖会淹）。
    pub water_below: bool,
    /// 基地下方是否有大量 CO₂（深挖会放毒）。
    pub co2_below: bool,
    pub food_kcal: f64,
    /// 地上可用的建材总量（砂岩/泥土/藻类/沙/黏土）。
    ///
    /// 零建材时任何建造都必然失败（梯子/砖也要建材），
    ///   所以第一目标必须是采建材，而不是直接开竖井或建厕所。
    pub building_materials_kg: f64,
}
/// 搜索某元素/名称在地图上的匹配数（用于 brief 的资源统计）。
fn count_matches(session: &McpSession, q: &str) -> i64 {
    session
        .call_text(
            "world_editor",
            serde_json::json!({
                "command": "read",
                "path": format!("oni://world/search?query={q}&kinds=cells"),
                "confirm": true,
                "task": format!("自动规划: 搜索 {q}")
            }),
        )
        .ok()
        .and_then(|t| {
            let f = t.rfind("```json")?;
            let rel = t[f + 7..].find('{').map(|i| f + 7 + i)?;
            let end = t[rel..].rfind('}')?;
            let v: serde_json::Value = serde_json::from_str(&t[rel..=rel + end]).ok()?;
            v.get("matched").and_then(|m| m.as_i64())
        })
        .unwrap_or(0)
}
/// **采集本局世界事实**（开局规划 brief 的主体）。
/// 只采**事实**：可达区/行跨度/资源距离/基底下水与CO₂/食物/建材。
///   否则"CLI 看到的"和"代理看到的"会不一致。
pub fn collect_facts(session: &McpSession, snap: &crate::sense::Snapshot) -> WorldFacts {
    // ---- 官方可达集（去重！）----
    let mut reach_set: std::collections::BTreeSet<(i32, i32)> = std::collections::BTreeSet::new();
    if let Ok(text) = session.call_text(
        "world_editor",
        serde_json::json!({
            "command": "read", "path": "/active/dupes/reachability.md",
            "radius": 16, "sampleLimit": 200, "confirm": true,
            "task": "自动规划: 官方可达集"
        }),
    ) {
        if let Some(f) = text.rfind("```json") {
            if let Some(rel) = text[f + 7..].find('{').map(|i| f + 7 + i) {
                if let Some(end) = text[rel..].rfind('}') {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text[rel..=rel + end]) {
                        if let Some(items) = v.get("items").and_then(|x| x.as_array()) {
                            for it in items {
                                if let Some(ss) = it.get("samples").and_then(|x| x.as_array()) {
                                    for c in ss {
                                        let x = c.get("x").and_then(|v| v.as_i64()).unwrap_or(-1) as i32;
                                        let y = c.get("y").and_then(|v| v.as_i64()).unwrap_or(-1) as i32;
                                        if x >= 0 && y >= 0 {
                                            reach_set.insert((x, y));
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    let ys: Vec<i32> = reach_set.iter().map(|c| c.1).collect();
    let row_span = if ys.is_empty() {
        1
    } else {
        ys.iter().max().unwrap() - ys.iter().min().unwrap() + 1
    };
    let base_min_y = ys.iter().copied().min().unwrap_or(0);

    // ---- 地图（找基地下方的水与 CO₂）----
    let (mut water_below, mut co2_below) = (false, false);
    if let Ok(text) = session.call_text(
        "world_editor",
        serde_json::json!({
            "command": "read", "path": "/active/map/viewport.md",
            "format": "edit", "compact": false, "confirm": true,
            "task": "自动规划: 读取地图（找下方水与 CO₂）"
        }),
    ) {
        for line in text.lines() {
            let Some(rest) = line.strip_prefix('|') else { continue };
            let cols: Vec<&str> = rest.split('|').map(|c| c.trim()).collect();
            if cols.len() < 5 { continue; }
            let y: i32 = match cols[0].trim_matches('`').parse() { Ok(v) => v, Err(_) => continue };
            if y >= base_min_y { continue; }
            let element = cols[4].trim_matches('`');
            let kind = cols[3].trim_matches('`');
            if kind == "liq" && matches!(element, "Water" | "SewageWater" | "DirtyWater") {
                water_below = true;
            }
            if element == "CarbonDioxide" {
                co2_below = true;
            }
        }
    }

    WorldFacts {
        dupes: snap.dupes.len(),
        reachable_cells: reach_set.len(),
        base_row_span: row_span,
        surface_distance: None,
        oxy_rock_near: count_matches(session, "OxyRock") as usize,
        coal_near: count_matches(session, "Carbon") as usize,
        copper_near: count_matches(session, "Cuprite") as usize,
        iron_near: count_matches(session, "IronOre") as usize,
        clay_near: count_matches(session, "Clay") as usize,
        plants_near: (count_matches(session, "米虱木")
            + count_matches(session, "淤泥根")
            + count_matches(session, "诱人荆棘")) as usize,
        water_below,
        co2_below,
        food_kcal: snap.food_kcal(),
        building_materials_kg: ["SandStone", "Dirt", "Algae", "Sand", "Clay"]
            .iter()
            .map(|e| snap.loose_kg(e))
            .sum(),
    }
}
/// **跨局通用原则**（每局都成立的经验教训，与地图无关 → 属于系统知识）。
///
/// 注意区分：这些是"怎么活下来"的**规则**，不是"这一局先做什么"的**套路**。
/// 套路（先挖地表还是先产氧、走藻类箱还是电解器）必须由 LLM 依据本局事实决定。
pub const UNIVERSAL_PRINCIPLES: &[&str] = &[
    "厕所必须前置：厕所排在后面 → 膀胱 100% → 随地排泄 → 污染水 → 复制人泡在里面呼吸归零 → 全灭（两次事故）。",
    "全局气体比例会骗人：全局 O₂/(O₂+CO₂)=99% 也可能是封闭气室在闷死；换气是否达成只看「复制人真能走到的最高可达格」，且必须复核，不许声明。",
    "厕所 2×3、床 2×2 需要连续空间：找不到落点时自动腾空间（挖开占格），不是放弃。",
    "梯子/砖块只认建材（砂岩/泥土/藻类/黏土/沙）；发电机/电线/高级产氧要金属。开局地上零建材 → 第一步永远是采建材。",
    "缺材料不能阻断推进：缺什么就去挖什么（自举），不要反复下注定失败的建造单。",
    "食物预算：400 kcal/人/周期；食物续航 < 5 周期必须立刻安排食物（采集现成植物最快）。",
    "产氧设备需要电力 → **供电节点必须排在产氧节点之前**；没有电时只考虑零耗电方案。",
    "研究站（ResearchCenter）解锁电解器/藻类箱/烤炉——走研究路线必须有建研究站的节点。",
    "选址避开 CO₂ 气囊与污染区：复制人在里面工作会逐步窒息（O₂ 占比 ≈ 1-非O₂质量）。",
    "一次只下一条工单；「造出来」≠「在运转」，设备判据必须看通电/运转状态。",
    "换气判据用「复制人真能走到的最高可达格」且必须复核——不许声明完成。",
];

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
/// **判据自愈**：LLM 发明的判据 kind → 降级为 manual（可执行但永不自动完成）。
///
/// 为什么不直接拒绝整份计划：一个未知判据只影响**一个节点**的验收，
///   拒绝整份计划会让其它 10 个正确节点陪葬（实测首跑：T1 被发明 material_min，
///   整份计划被拒 → 代理继续跑旧 playbook）。降级 + 告警 = 可用且可见。
/// 返回告警清单。
pub fn repair_plan_checks(plan: &mut Plan) -> Vec<String> {
    let mut warns = Vec::new();
    for node in &mut plan.nodes {
        let Some(v) = node.success_check.as_mut() else { continue };
        let kind = v.get("kind").and_then(|k| k.as_str()).unwrap_or("").to_string();
        if !CHECK_KINDS.contains(&kind.as_str()) {
            warns.push(format!("{} 的判据 kind {:?} 不在白名单 → 降级为 manual", node.id, kind));
            *v = serde_json::json!({"kind": "manual"});
        }
    }
    warns
}

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
fn build_prompts(brief_text: &str) -> (String, String) {
    let system = "你是《缺氧》(Oxygen Not Included) 殖民地的开局战略规划器。\
你的任务：根据**本局世界事实**生成一份依赖图计划（JSON）。\
地图类型千差万别（陆地/水世界/金属富集/沼泽/陨石坑…），**禁止套用固定套路**——\
节点结构必须由事实决定（例：水世界优先藻类箱/电解器，陆地富氧石优先氧气扩散器，\
地表近优先打通换气口，深埋优先竖井+梯子）。\
只输出 JSON，不要任何其它文字、不要 markdown 栅栏。".to_string();

    // ★ 判据 kind 全集 + **每种的字段格式**（必须照抄，不要自己发明字段名）。
    //   为什么必须写清字段：LLM 看不到 check_success 的实现，只能照例子猜
    //   （实测它把 loose_min 写成 {key,n}，而代码只认 {element,kg} → 判据永远无法判定
    //     → 节点永远不算 done → 零进展 → 被熔断 → 下游节点前置断链 → 保命目标做不成）。
    //   代码侧已加别名容错兜底，但 prompt 写清楚才能让 LLM 一次就写对。
    let kinds_line = format!(
        "判据 kind 全集与**字段格式**（kind 只能从中选，字段名必须照抄）:\n\
   - building_min: {{kind, key:\"toilets|beds|...\", n:<数>}}\n\
   - prefab_min: {{kind, prefab:\"Ladder\", n:<数>}}\n\
   - loose_min: {{kind, element:\"SandStone\", kg:<数>}}\n\
   - loose_any_min: {{kind, elements:[\"SandStone\",\"Dirt\"], kg:<数>}}\n\
   - loose_multi_min: {{kind, elements:[[\"SandStone\",<kg>],[\"Cuprite\",<kg>]]}}\n\
   - oxygen_min: {{kind, kg:<数>}}\n\
   - food_min: {{kind, kcal:<数>}}\n\
   - food_cycles_min: {{kind, cycles:<数>}}\n\
   - reachable_min: {{kind, n:<数>}}\n\
   - dupe_min: {{kind, n:<数>}}\n\
   - rooms_min: {{kind, key:\"<房间名>\", n:<数>}}\n\
   - storage_mass_min: {{kind, kg:<数>}}\n\
   - power_net: {{kind, watts:<数>}}\n\
   - power_operational: {{kind}}\n\
   - generator_fueled: {{kind}}\n\
   - oxygen_producing: {{kind}}\n\
   - surface_open: {{kind, minY:<地表Y>}}\n\
   - scout_done: {{kind}}\n\
   - breath_min: {{kind, value:<数>}}\n\
   - breath_ratio_min: {{kind, pct:<数>}}\n\
   - manual: {{kind}}（人工判定，永远不会自动 done）\n\
   可选 kind 白名单: {}",
        CHECK_KINDS.iter().map(|k| format!("`{k}`")).collect::<Vec<_>>().join(", ")
    );
    let user = format!(
        "== 本局世界事实 ==\n{brief_text}\n\n== 跨局通用原则（任何地图都适用）==\n{}\n\n== 硬性约束 ==\n\
1. 产氧设备（氧气扩散器/电解器）需要电力 → 供电节点的 id 必须出现在产氧节点的 requires 里；\n2. 若走研究路线（电解器/藻类箱被研究锁时），必须有建研究站的节点；\n3. 判据用终局量：产氧 = {{kind:oxygen_producing}}；换气 = {{kind:surface_open,minY:<地表Y>}}；\n   卫生 = {{kind:building_min,key:toilets,n:<人数>}}；床位 = {{kind:building_min,key:beds,n:<人数>}}；\n   供电 = {{kind:power_operational}}；食物 = {{kind:food_cycles_min,cycles:<N>}}；\n4. 不得写死坐标：AutoBuild 的 region 一律 \"auto\"；禁止 build/dig/order/auto_connect 动作；\n5. 语义 goal 只能取：scout/material/surface/oxygen/expand/open_chamber/power/research/forage/mop_dirty/element:<元素名>；\n6. 节点数 6~14；requires/unlocks 引用必须存在且构成 DAG；每个节点 1~3 个动作；\n7. 必须覆盖五条生命线：卫生、氧气、食物、居住(床)、电力；survival_node 指向换气/产氧节点；\n
8. {kinds_line}；\n9. 每个节点**必须**带 success_check 对象（照抄第3条的例子格式），缺失会被校验拒绝。\n\n== 输出 schema（字段名必须完全一致）==\n{{\n\
  \"objective\": \"一句话\", \"survival_node\": \"<id>\", \"author\": \"ai\", \"locked\": true,\n\
  \"nodes\": [{{\"id\": \"T1\", \"name\": \"...\", \"status\": \"todo\", \"requires\": [], \"unlocks\": [],\n\
    \"actions\": [{{\"kind\": \"auto\", \"goal\": \"...\", \"why\": \"...\"}}],\n\
    \"success_check\": {{\"kind\": \"building_min\", \"key\": \"toilets\", \"n\": 3}}, \"success\": \"人类可读\", \"note\": \"\"}}\n}}",
        UNIVERSAL_PRINCIPLES.iter().map(|p| format!("- {p}")).collect::<Vec<_>>().join("\n")
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
) -> Result<String, String> {
    let (system, user) = build_prompts(brief_text);
    // 开局规划要输出 6~14 个完整节点（含 success_check/actions），4096 会被截断 → 给 16000。
    let raw = crate::slow::call_llm(cfg, &system, &user, 16000)?;
    let json_text = extract_json(&raw).ok_or("LLM 输出里没有 JSON")?;
    let mut raw_v: Value = serde_json::from_str(json_text).map_err(|e| format!("计划 JSON 解析失败: {e}"))?;
    let mut plan: Plan = serde_json::from_value(raw_v.clone()).map_err(|e| format!("计划 schema 不合法: {e}"))?;
    let repairs = repair_plan_checks(&mut plan);
    let errs = validate_plan(&plan);
    if !errs.is_empty() {
        // 失败也要可见：带上生成的节点清单（诊断 LLM 到底规划了什么）
        let nodes_brief: Vec<String> = plan.nodes.iter().map(|n| {
            let c = n.success_check.as_ref()
                .and_then(|c| c.get("kind").and_then(|k| k.as_str()))
                .unwrap_or("无判据");
            format!("{}({}:{})", n.id, n.name.chars().take(12).collect::<String>(), c)
        }).collect();
        return Err(format!(
            "计划校验未通过（{} 条）：{}；生成的节点：{}{}",
            errs.len(), errs.join("；"), nodes_brief.join("、"),
            if repairs.is_empty() { String::new() } else { format!("（已自愈 {} 条未知判据）", repairs.len()) }
        ));
    }
    // 名副其实：这份计划才是 AI 生的 → author=ai / locked=true
    if let Some(obj) = raw_v.as_object_mut() {
        obj.insert("author".into(), Value::from("ai"));
        obj.insert("locked".into(), Value::from(true));
        if let Some(f) = fp {
            obj.insert("save_fingerprint".into(), Value::from(f.to_string()));
        }
    }
    std::fs::write(plan_path, serde_json::to_string_pretty(&raw_v).unwrap_or_default())
        .map_err(|e| format!("写盘失败: {e}"))?;
    let mut msg = format!("LLM 开局规划已生成（{} 节点，校验通过）", plan.nodes.len());
    if !repairs.is_empty() {
        msg.push_str(&format!("；⚠ {} 条判据被降级为 manual：{}", repairs.len(), repairs.join("；")));
    }
    Ok(msg)
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
            ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
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
