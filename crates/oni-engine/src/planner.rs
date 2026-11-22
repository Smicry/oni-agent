//! 自动开局规划器：地形特征 → 策略族 → 依赖图。
//!
//! 为什么必须有：
//!   之前的 plan.json 是人（AI）手写的 → 换存档就全废 → 跨局污染，
//!   而且"这一局该怎么玩"每次都要重新想一遍。
//!   本模块把"怎么玩"变成规则：读世界特征 → 匹配策略族 → 生成依赖图。
//!
//! 通用性从哪来：
//!   ONI 的死因只有 5 类（窒息 / 污染 / 饥饿 / 脱水 / 过热），
//!   所以通用助手不需要创造力，只需要把开局特征映射到正确的策略族。
//!
//! 硬约束：生成的节点里不出现硬编码坐标 —— 坐标由世界模型运行时算
//!   。

use oni_mcp::McpSession;
use serde_json::{json, Value};

use crate::decide::{Node, Plan};
use crate::exec::Action;

/// 策略族。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    /// 先建材型：开局地上零建材 → 梯子/砖/厕所/床全部造不了。
    /// → 第一目标：采建材（横向挖，低风险）；梯子本身也要建材才能挖竖井。
    MaterialFirst,
    /// 憋死型：可站区域只有一行高（复制人被封在走廊里）
    /// → 第一目标：垂直竖井 + 梯子 → 开阔气室；禁止向下深挖。
    Suffocate,
    /// 近地表型：到地表很近 → 第一目标：打通地表（永久换气）。
    NearSurface,
    /// 富氧型：开局附近有氧石 → 第一目标：产氧器（氧石）。
    RichOxygen,
    /// 富矿型：附近有煤 + 铜 → 第一目标：供电链。
    RichOre,
    /// 无植物型：开局没有可食植物 → 食物必须靠种植（收割只能应急）。
    Plantless,
    /// 常规型。
    Standard,
}

impl Strategy {
    pub fn label(&self) -> &'static str {
        match self {
            Strategy::MaterialFirst => "先建材型（开局零建材）",
            Strategy::MaterialFirst => "先建材型（开局零建材）",
            Strategy::Suffocate => "憋死型（可站区域只有一行高）",
            Strategy::NearSurface => "近地表型（到地表很近）",
            Strategy::RichOxygen => "富氧型（附近有氧石）",
            Strategy::RichOre => "富矿型（附近有煤与铜）",
            Strategy::Plantless => "无植物型（食物只能靠种植）",
            Strategy::Standard => "常规型",
        }
    }
}

/// 开局世界事实（由 CLI 从游戏采集后传入）。
#[derive(Debug, Clone, Default)]
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

/// 匹配策略族。
pub fn classify(f: &WorldFacts) -> Strategy {
    // 0) 零建材优先：梯子/砖/厕所/床全都要建材 → 不先采建材，后面全是必然失败。
    if f.building_materials_kg < 300.0 {
        return Strategy::MaterialFirst;
    }
    // 1) 憋死型：人上不去，什么都做不了。
    if f.base_row_span <= 1 && f.reachable_cells <= 20 {
        return Strategy::Suffocate;
    }
    // 2) 食物彻底没有且没有植物 → 必须种。
    if f.plants_near == 0 && f.food_kcal < 12000.0 {
        return Strategy::Plantless;
    }
    // 3) 地表很近 → 打通地表最省事（永久换气）。
    if let Some(d) = f.surface_distance {
        if d <= 12 {
            return Strategy::NearSurface;
        }
    }
    // 4) 有氧石 → 先产氧。
    if f.oxy_rock_near >= 2 {
        return Strategy::RichOxygen;
    }
    // 5) 有煤有铜 → 先供电。
    if f.coal_near >= 1 && f.copper_near >= 2 {
        return Strategy::RichOre;
    }
    Strategy::Standard
}

fn node(
    id: &str,
    name: &str,
    requires: &[&str],
    unlocks: &[&str],
    actions: Vec<Action>,
) -> Node {
    Node {
        id: id.to_string(),
        name: name.to_string(),
        status: "todo".to_string(),
        requires: requires.iter().map(|s| s.to_string()).collect(),
        unlocks: unlocks.iter().map(|s| s.to_string()).collect(),
        actions,
        success_check: None,
        success: String::new(),
        rescue: false,
        note: String::new(),
        priority: None,
        deferred: None,
        deferred_at: None,
    }
}

fn auto(goal: &str, why: &str) -> Action {
    Action::Auto {
        goal: goal.to_string(),
        why: why.to_string(),
    }
}

fn build(prefab: &str, why: &str) -> Action {
    Action::AutoBuild {
        prefab: prefab.to_string(),
        region: "auto".to_string(),
        material: String::new(),
        why: why.to_string(),
    }
}

fn with_check(mut n: Node, check: Value) -> Node {
    n.success_check = Some(check);
    n
}

/// 按策略族生成依赖图。
pub fn build_plan(f: &WorldFacts) -> (Plan, Strategy) {
    let s = classify(f);
    let mut nodes: Vec<Node> = Vec::new();
    let n_dupes = f.dupes.max(1) as i64;

    // ---- S0 侦察（永远第一步，只读，安全）----
    let s0 = with_check(
        node(
            "S0",
            "开局侦察：探清关键资源分布与距离",
            &[],
            &["A1"],
            vec![auto(
                "scout",
                "开局先侦察：不知道资源在哪就深挖，会挖开地下 CO₂",
            )],
        ),
        json!({"kind": "scout_done"}),
    );
    nodes.push(s0);

    // ---- A1：策略族的第一目标 ----
    let a1: Node = match s {
        Strategy::MaterialFirst => with_check(
            node(
                "A1",
                "零建材：先采建材（梯子/砖/厕所/床全都造不了）",
                &["S0"],
                &["A2", "A3", "A4", "A5", "A6", "A7"],
                vec![auto(
                    "material",
                    " 地上零建材 → 梯子、砖、厕所、床全部造不了 → 先横向采建材（低风险）",
                )],
            ),
            json!({"kind": "loose_any_min", "kg": 500,
                  "elements": ["SandStone", "Algae", "Dirt", "Clay"]}),
        ),
        Strategy::Suffocate => with_check(
            node(
                "A1",
                "憋死型第一目标：开竖井（垂直 + 梯子）到开阔气室",
                &["S0"],
                &["A2", "A3", "A4", "A5", "A6", "A7"],
                vec![auto(
                    "surface",
                    " 可站区域只有一行高 → 复制人被封住 → 必须先有能上去的路（竖井必须垂直 + 梯子链；斜向阶梯会造真空袋）",
                )],
            ),
            json!({"kind": "manual"}),
        ),
        Strategy::NearSurface => with_check(
            node(
                "A1",
                "打通地表（永久换气口）",
                &["S0"],
                &["A2", "A3", "A4", "A5", "A6", "A7"],
                vec![auto(
                    "surface",
                    " 到地表很近 → 打通地表最省事：永久换气 + 复制人能去地表采集",
                )],
            ),
            json!({"kind": "surface_open", "minY": 216}),
        ),
        Strategy::RichOxygen => with_check(
            node(
                "A1",
                "产氧：氧气扩散器（用氧石）",
                &["S0"],
                &["A2", "A3", "A4", "A5", "A6", "A7"],
                vec![auto(
                    "oxygen",
                    " 附近有氧石 → 先产氧（运行时探测哪个产氧件现在能造，不要写死 prefabId）",
                )],
            ),
            json!({"kind": "prefab_min", "prefab": "MineralDeoxidizer", "n": 1}),
        ),
        Strategy::RichOre => with_check(
            node(
                "A1",
                "供电：铺砖 + 发电机 + 接线",
                &["S0"],
                &["A2", "A3", "A4", "A5", "A6", "A7"],
                vec![auto(
                    "power",
                    " 附近有煤与铜 → 先把电供上（造出来 ≠ 有电，还要接线）",
                )],
            ),
            json!({"kind": "power_operational"}),
        ),
        Strategy::Plantless => with_check(
            node(
                "A1",
                "食物链：研究站 → 烤箱/农业站（开局无植物，只能种）",
                &["S0"],
                &["A2", "A3", "A4", "A5", "A6", "A7"],
                vec![auto(
                    "research",
                    " 没有可食植物 → 食物必须靠种植：研究站 → 基础耕种 → 土培砖 + 农业站",
                )],
            ),
            json!({"kind": "prefab_min", "prefab": "ResearchCenter", "n": 1}),
        ),
        Strategy::Standard => with_check(
            node(
                "A1",
                "采建材（梯子/砖块的材料）",
                &["S0"],
                &["A2", "A3", "A4", "A5", "A6", "A7"],
                vec![auto(
                    "material",
                    " 开局通常零建材 → 梯子/砖块/发电机全部造不了 → 先采建材",
                )],
            ),
            json!({"kind": "loose_any_min", "kg": 500,
                  "elements": ["SandStone", "Algae", "Dirt", "Clay"]}),
        ),
    };
    nodes.push(a1);

    // ---- A2 卫生----
    let a2 = with_check(
        node(
            "A2",
            "卫生：旱厕（3 人至少 3 个，且必须建在富氧区）",
            &["A1"],
            &["A3"],
            vec![build(
                "Outhouse",
                " 膀胱 100% 会随地排泄 → 生成污染水 → 淹基地。旱厕不要水，只要建材",
            )],
        ),
        json!({"kind": "building_min", "key": "toilets", "n": n_dupes}),
    );
    nodes.push(a2);

    // ---- A3 居住（床必须落在可呼吸的格）----
    let a3 = with_check(
        node(
            "A3",
            "居住：床（必须建在富氧区）",
            &["A2"],
            &["A4"],
            vec![build(
                "Bed",
                " 床不能建在 CO₂ 区；OnFloor 建筑要地板砖",
            )],
        ),
        json!({"kind": "building_min", "key": "beds", "n": n_dupes}),
    );
    nodes.push(a3);

    // ---- A4 产氧 ----
    let a4 = with_check(
        node(
            "A4",
            "产氧：氧气扩散器/电解器（运行时探测哪个能造）",
            &["A3"],
            &["A5", "A6", "A7"],
            vec![auto(
                "oxygen",
                " 产氧件的研究锁因存档而异 → 运行时问 mod「现在能不能造」，不要写死 prefabId",
            )],
        ),
        json!({"kind": "prefab_min", "prefab": "MineralDeoxidizer", "n": 1}),
    );
    nodes.push(a4);

    // ---- A5 供电 ----
    let a5 = with_check(
        node(
            "A5",
            "供电：发电机 + 接线（造出来 ≠ 有电）",
            &["A4"],
            &["A6", "A7"],
            vec![auto(
                "power",
                " 四步：挖空间 → 铺地板砖 → 建发电机 → 接线到耗电建筑",
            )],
        ),
        json!({"kind": "power_operational"}),
    );
    nodes.push(a5);

    // ---- A6 研究站（解锁烤箱/农业站）----
    let a6 = with_check(
        node(
            "A6",
            "研究站（解锁食物制备与农业）",
            &["A5"],
            &["A7"],
            vec![build(
                "ResearchCenter",
                " 研究点来自打印舱 care package；研究站是食物可持续的必经之路",
            )],
        ),
        json!({"kind": "prefab_min", "prefab": "ResearchCenter", "n": 1}),
    );
    nodes.push(a6);

    // ---- A7 食物 ----
    let (food_goal, food_why) = if f.plants_near > 0 {
        (
            "forage",
            " 有可食植物 → 每周期收割（可再生，零风险）",
        )
    } else {
        (
            "element:Clay",
            " 没有植物 → 必须种地：土培砖要黏土（Farmable 类别）",
        )
    };
    let a7 = with_check(
        node(
            "A7",
            "食物（可持续）",
            &["A6"],
            &[],
            vec![auto(food_goal, food_why)],
        ),
        json!({"kind": "food_cycles_min", "cycles": 20}),
    );
    nodes.push(a7);

    // ---- 保命节点：产氧（空气红线）----
    let plan = Plan {
        nodes,
        survival_node: Some("A4".to_string()),
        author: Some("auto".to_string()),
        locked: false,
    };
    let mut plan = plan;

    plan.nodes[0].note = format!(
        " 自动规划（策略族：{}）。特征：可达{}格/高{}行/氧石{}/煤{}/铜{}/铁{}/植物{}/食物{:.0}kcal{}{}",
        s.label(),
        f.reachable_cells,
        f.base_row_span,
        f.oxy_rock_near,
        f.coal_near,
        f.copper_near,
        f.iron_near,
        f.plants_near,
        f.food_kcal,
        if f.water_below { "/下方有水(禁深挖)" } else { "" },
        if f.co2_below { "/下方有CO₂(禁深挖)" } else { "" },
    );
    plan.nodes[0].note.push_str(
        "。打印舱领奖由代理每轮内建执行（不占依赖图：没有动作的节点会空转挡住真目标）。",
    );
    (plan, s)
}

/// 规划是否需要重规划（事件驱动）。
///
/// 触发事件（全部踩到）：
///   人口下降 / 氧气告急 / 节点连续失败。
pub fn should_replan(
    last_pop: usize,
    now_pop: usize,
    failed_streak: usize,
    oxygen_cycles_left: Option<f64>,
) -> Option<&'static str> {
    if last_pop > 0 && now_pop < last_pop {
        return Some("人口下降");
    }
    if failed_streak >= 3 {
        return Some("节点连续失败");
    }
    if let Some(c) = oxygen_cycles_left {
        if c < 2.0 {
            return Some("氧气续航告急");
        }
    }
    None
}


/// 从游戏采集开局事实（CLI 与 agent 共用）。
///
/// 为什么放在 engine：事件驱动重规划（agent 主循环）也要用同一套事实采集，
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

/// 世界搜索里匹配到的数量（元素搜索）。
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

/// 保留已完成状态：把旧计划里 done 的节点标成 done。
pub fn merge_done(new_plan: &mut Plan, old: &Plan) {
    for n in new_plan.nodes.iter_mut() {
        if let Some(o) = old.nodes.iter().find(|o| o.id == n.id) {
            if o.is_done() {
                n.status = "done".to_string();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn material_first_strategy_when_no_materials() {
        let f = WorldFacts {
            dupes: 3,
            reachable_cells: 12,
            base_row_span: 1,
            building_materials_kg: 0.0,
            ..Default::default()
        };
        assert_eq!(classify(&f), Strategy::MaterialFirst);
        let (p, _) = build_plan(&f);
        let a1 = p.nodes.iter().find(|n| n.id == "A1").unwrap();
        assert!(
            format!("{:?}", a1.actions).contains("material"),
            "零建材时第一目标必须是采建材"
        );
    }

    #[test]
    fn suffocate_strategy_when_base_is_one_row() {
        let f = WorldFacts {
            dupes: 3,
            reachable_cells: 12,
            base_row_span: 1,
            building_materials_kg: 800.0,
            ..Default::default()
        };
        assert_eq!(classify(&f), Strategy::Suffocate);
        let (p, _) = build_plan(&f);
        // 憋死型的第一目标必须是"开路"（surface），不是产氧/供电
        let a1 = p.nodes.iter().find(|n| n.id == "A1").unwrap();
        assert!(format!("{:?}", a1.actions).contains("surface"));
    }

    #[test]
    fn plantless_strategy_when_no_plants_and_low_food() {
        let f = WorldFacts {
            dupes: 3,
            reachable_cells: 60,
            base_row_span: 6,
            plants_near: 0,
            food_kcal: 8000.0,
            building_materials_kg: 800.0,
            ..Default::default()
        };
        assert_eq!(classify(&f), Strategy::Plantless);
    }

    #[test]
    fn plan_has_no_hardcoded_rects() {
        let f = WorldFacts {
            dupes: 3,
            reachable_cells: 12,
            base_row_span: 1,
            ..Default::default()
        };
        let (p, _) = build_plan(&f);
        let s = serde_json::to_string(&p).unwrap();
        assert!(
            !s.contains("\"rect\""),
            "自动规划出的计划里不允许有硬编码坐标（会跨局污染）：{s}"
        );
    }

    #[test]
    fn every_node_has_actions_or_is_rescue() {
        let f = WorldFacts::default();
        let (p, _) = build_plan(&f);
        for n in &p.nodes {
            assert!(
                !n.actions.is_empty(),
                "节点 {} 没有动作 → 会空转挡住真目标（AGENTS.md 13.39.4）",
                n.id
            );
        }
    }

    #[test]
    fn replan_triggers_on_population_drop() {
        assert_eq!(should_replan(3, 2, 0, Some(10.0)), Some("人口下降"));
        assert_eq!(should_replan(3, 3, 0, Some(10.0)), None);
        assert_eq!(should_replan(3, 3, 3, Some(10.0)), Some("节点连续失败"));
        assert_eq!(should_replan(3, 3, 0, Some(1.0)), Some("氧气续航告急"));
    }
}
