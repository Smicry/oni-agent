//! 实时优先级调度器：每轮根据**当前真实状态**算出"最该做什么"，
//! 能打断静态依赖图（plan.json）。
//!
//! 为什么需要：
//!   /2/3/5/6 的共同根因是**代理看不见人** —— 观察的是材料/建筑/气体总量，
//!   没有"每个人现在怎么样"。等 plan.json 反应过来时人已经没了。
//!   所以调度必须**实时**：每轮重新评估，能抢占当前节点。
//!
//! 分级
//!
//! | Level | 触发 | 动作 |
//! |---|---|---|
//!  铁律：氧气续航告急时**禁止扩张性挖掘** ——
//!   挖开会暴露地下 CO₂，在缺氧时是负收益。必须先产氧，不能越缺氧越挖地表。
//!
//! | 3 救命 | 有人失去行动能力 | 救援：排毒 / 拖到有空气处 / 治疗 |
//! | 2 紧急 | 有人呼吸 < 50，或所在格不可呼吸 | 先把他们弄到能呼吸的地方 |
//! | 1 关注 | 氧气消耗归零 / stress 飙升 / 空气比例低 / 食物不足 | 对应计划节点 |
//! | 0 正常 | 无异常 | 按 plan 依赖图推进 |

use crate::judge::{Judgment, Verdict};
use crate::sense::Snapshot;

/// 优先级等级。数值越大越紧急。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Normal = 0,
    Watch = 1,
    Urgent = 2,
    Rescue = 3,
}

/// 一条调度决策。
#[derive(Debug, Clone)]
pub struct Directive {
    pub level: Level,
    /// 人话解释（写进日志/报告）
    pub reason: String,
    /// 建议动作类别，供 agent 匹配计划节点
    pub action: Action,
    /// **禁止扩张性挖掘**（挖地表/开拓新矿层）。
    ///
    /// ：缺氧时挖掘暴露地下 CO₂ 是负收益，会让氧气掉得更快。
    ///   为 true 时 agent 必须跳过换气/地表/挖矿类节点，只做产氧/救援/食物。
    pub forbid_expansion: bool,
    /// Semantic decision needed: freeze + escalate to the AI slow system instead of acting.
    pub needs_ai: bool,
}

/// 建议动作类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// 按计划正常推进
    FollowPlan,
    /// 救援：清理污染物 / 医疗 / 把人挪到能呼吸的地方
    Rescue,
    /// 换气：打通到地表或干净空气区
    Ventilate,
    /// 产氧：造制氧设备
    ProduceOxygen,
    /// 补给：搞食物
    ForageFood,
    /// 供电：接电网
    Power,
    /// 建厕所
    BuildToilet,
}

impl Action {
    /// 动作的中文标签（用于日志与节点匹配）。
    pub fn label(&self) -> &'static str {
        match self {
            Action::FollowPlan => "按计划推进",
            Action::Rescue => "救援",
            Action::Ventilate => "换气",
            Action::ProduceOxygen => "产氧",
            Action::ForageFood => "补给食物",
            Action::Power => "供电",
            Action::BuildToilet => "建厕所",
        }
    }
}

/// 呼吸值低于此值 = 危险（与 judge 的红线一致）。
pub const URGENT_BREATH: f64 = 50.0;
/// 氧气续航低于此周期数 = 禁止扩张性挖掘，产氧优先（）。
pub const OXYGEN_CYCLES_CRITICAL: f64 = 3.0;
/// food runway below this -> forage first (incident 8).
pub const FOOD_CYCLES_CRITICAL: f64 = 0.5;
/// 膀胱超过此百分比且**没有厕所** → 判定必须建厕所。
///
/// 根因链：没厕所 → 膀胱满 → 随地排泄 → 污染水
///   三人泡在污染水里→ 差点重演/9。
///   厕所是**污染的一级预防**，不是舒适度需求。
pub const BLADDER_CRITICAL: f64 = 80.0;

/// 计算本轮的调度指令。
///
/// 参数：
/// - `snap`：当前快照
/// - `judgment`：判定结果（复用已有阈值，不另立标准）
/// - `o2_rate`：氧气消耗速率（kg/周期），来自趋势模块；`None` = 数据不足
/// - `stress_rising`：stress 是否在上升
pub fn direct(
    snap: &Snapshot,
    judgment: &Judgment,
    o2_rate: Option<f64>,
    stress_rising: bool,
    oxygen_frozen: bool,
    oxygen_cycles_left: Option<f64>,
    food_cycles_left: Option<f64>,
) -> Directive {
    // ---- Level 3：有人倒地 → 最高优先级 ----
    let downed = snap.downed_count();
    if downed > 0 {
        return Directive {
            level: Level::Rescue,
            reason: format!(
                "⛑ {downed} 个复制人失去行动能力 —— 他们**不会执行任何差事**，
                施工全部让位，先救人"
            ),
            action: Action::Rescue,
            forbid_expansion: false,
            needs_ai: false,
        };
    }

    // ---- Level 2：有人呼吸偏低 / 所在格不可呼吸 ----
    if let Some(lowest) = snap.min_breath() {
        if lowest < URGENT_BREATH {
            return Directive {
                level: Level::Urgent,
                reason: format!(
                    "😨 有复制人呼吸仅 {lowest:.0}（阈值 {URGENT_BREATH}）→ 先把人弄到能呼吸的地方",
                ),
                action: Action::Rescue,
                forbid_expansion: false,
                needs_ai: true,
            };
        }
    }

    // ---- Level 2：stress **相对基线**显著上升----
    //  量纲纪律：maxStress 是 0–100 制的百分比。开局三个人的基线就有 4.4~4.7（完全正常）。
    //    旧代码用固定绝对阈值 3.0 → 每轮都判"有人在受苦" → 抢占正常计划 → 卡死开局（）。
    //  正确判据：**相对近期基线的上升量**（trend::stress_baseline + STRESS_RISE_DELTA）。
    //    事故证据：死亡前 stress 0.0 → 3.3 → 5.1—— 相对上升抓得住，
    //    绝对阈值抓不住。
    if stress_rising {
        return Directive {
            level: Level::Urgent,
            reason: format!(
                "😰 stress 相对基线明显上升（当前 {:.1}，升幅 ≥ {}）→ 有人在受苦，先改善环境",
                snap.max_stress(),
                crate::trend::STRESS_RISE_DELTA
            ),
            action: Action::Ventilate,
            forbid_expansion: false,
            needs_ai: true,
        };
    }

    // ---- Level 2：氧气完全冻结 = 人已经不再呼吸----
    //  必须有**旁证**才报警：
    //    复制人闲着/睡觉时耗氧极小 → 可见氧气读数几乎不变 → 误判"人不再呼吸"
    //    → 抢占正常计划 → 开局被劫持。
    //    真正的死亡信号是**呼吸值掉下来 / 胁迫飙升**，氧气冻结只是辅助证据。
    if oxygen_frozen && !snap.dupes.is_empty() {
        let breath_not_full = snap.min_breath().map(|b| b < 99.0).unwrap_or(false);
        if breath_not_full || stress_rising {
            return Directive {
                level: Level::Urgent,
                reason: "⏸ 氧气完全冻结 + 呼吸/胁迫异常 → 人可能已停止呼吸，立刻检查并救援".into(),
                action: Action::Rescue,
                forbid_expansion: false,
                needs_ai: true,
            };
        }
    }

    // ---- Level 1：氧气消耗归零 = 人不动了（比死亡早的信号）----
    //   有人但氧气一点不消耗 → 人已经不呼吸/已经倒了 → 立刻查人。
    if !snap.dupes.is_empty() && o2_rate.map(|r| r.abs() < 0.5).unwrap_or(false) {
        return Directive {
            level: Level::Urgent,
            reason: "⏸ 氧气几乎不再消耗但人还在 → 人可能已失去行动能力，立刻检查".into(),
            action: Action::Rescue,
            forbid_expansion: false,
            needs_ai: true,
        };
    }

    // ---- Level 1：按判定结果选主题 ----
    if judgment.verdict == Verdict::Danger {
        let action = if judgment.danger_keys.iter().any(|k| k == "oxygen") {
            // ：氧气耗尽 → 产氧优先，绝不 Ventilate（挖地表暴露 CO₂ 是负收益）
            Action::ProduceOxygen
        } else if judgment.danger_keys.iter().any(|k| k.starts_with("air") || k.starts_with("co2") || k.starts_with("suffocation")) {
            Action::Ventilate
        } else if judgment.danger_keys.iter().any(|k| k == "food") {
            Action::ForageFood
        } else {
            Action::FollowPlan
        };
        return Directive {
            level: Level::Watch,
            reason: format!(
                "⚠ 判定危险（{}）→ 按 {} 方向处理",
                judgment.danger_keys.join(","),
                action.label()
            ),
            action,
            forbid_expansion: action == Action::ProduceOxygen,
            needs_ai: false,
        };
    }

    // ---- Level 1：膀胱告急 → 必须有厕所----
    //  完整根因链（）：开局没有厕所 → 复制人膀胱涨到 97%
    //    膀胱满又没厕所 → 随地排泄 → 地面生成**污染水**（DirtyWater）
    //    污染水沿刚挖开的竖井底向西蔓延 → 三人泡在污染水里
    //    代理还在他们脚下开井 → 走���/9 的死亡路径。
    //  所以"厕所"不是舒适度需求，而是**污染的一级预防**。
    let max_bladder = snap.max_active_bladder();
    // 判据：厕所不够 + 有人膀胱满**且没在如厕**。
    //   max_active_bladder 排除了"正在如厕"者 —— 膀胱100往往正是如厕中的表现，
    //   拿它当告警会每轮误报、反复复活建厕所节点、抢占采食物/换气。
    //   排队中（没在如厕）且膀胱满才是真危机。
    if max_bladder > BLADDER_CRITICAL && snap.building("toilets") < snap.dupes.len() as i64 {
        return Directive {
        level: Level::Urgent,
        reason: format!(
            "🚽 膀胱已达 {max_bladder:.0}%（阈值 {BLADDER_CRITICAL}）且厕所数 {} < 人口 {} → \\
            随地排泄会生成污染水 → 立刻建厕所",
            snap.building("toilets"), snap.dupes.len(),
        ),
        action: Action::BuildToilet,
        forbid_expansion: false,
        needs_ai: true,
    };
    }

    // ---- Level 1：有人在不可呼吸的格子里（最精确的信号）----
    let in_bad = snap.dupes.iter().filter(|d| d.in_unbreathable()).count();
    if in_bad > 0 {
        // 人在不可呼吸格 **且** 呼吸值真的低 → Urgent（真危险，抢占救人）。
        //   人在 CO₂ 格但呼吸 100
        //   → 降级为 Watch：只记行动项，**不抢占**正常计划。
        //   为什么必须这样：
        //     旧逻辑不看 breath 值 → 每轮都判 Urgent → 抢占"换气"节点
        //     研究站（可持续产氧链）被饿死 → 代理单点反复挖 → 局面不可持续。
        // （吉恩之死）：全局 min_breath + 阈值 50 太宽 ——
        //   人在 CO₂ 里发电时呼吸值一路 100→93→88→77，代理一直判"常态差事"
        //   不抢占 → 他继续待在 CO₂ 里直到归零死亡。
        //   现在只看**身处不可呼吸环境的那部分人**，呼吸值掉到 95 以下就抢占救援；
        //   读不到呼吸值时保守判危险。
        let bad_breaths: Vec<f64> = snap
            .dupes
            .iter()
            .filter(|d| d.in_unbreathable())
            .filter_map(|d| d.breath)
            .collect();
        let breath_low = match bad_breaths.iter().cloned().fold(f64::INFINITY, f64::min) {
            f64::INFINITY => true, // 读不到呼吸值 → 保守判危险
            w => w < 95.0,
        };
        let level = if breath_low { Level::Urgent } else { Level::Watch };
        return Directive {
            level,
            reason: format!(
                "🌊 {in_bad} 个复制人在**不可呼吸**的格子里（污染水/CO₂/氯气）→ {}",
                if breath_low {
                    "呼吸值也偏低，立刻带出来（抢占）"
                } else {
                    "但呼吸值正常，属常态差事 → 记行动项，不抢占正常计划"
                }
            ),
            action: Action::Rescue,
            forbid_expansion: false,
            // Only escalate when breathing is actually low;
            // normal-breath exposure is a routine chore (Watch), not a decision.
            needs_ai: breath_low,
        };
    }

    // ---- Level 2：氧气续航告急 → 产氧优先 + 禁止扩张性挖掘（ 核心修复）----
    //  为什么不能继续 Ventilate（挖地表）：氧气快见底时，挖开会暴露地下 CO₂，
    //    让可呼吸气体掉得更快 —— 把 245kg 氧气的一局推到 149kg 死亡螺旋。
    //    正确做法：停止一切扩张，只做产氧/救援/食物。
    let o2_critical = oxygen_cycles_left
        .map(|c| c < OXYGEN_CYCLES_CRITICAL)
        .unwrap_or(false);
    if o2_critical {
        return Directive {
            level: Level::Urgent,
            reason: format!(
                "🫁 氧气仅够 {:.1} 周期（阈值 {OXYGEN_CYCLES_CRITICAL}）→ 产氧优先，**禁止扩张性挖掘**（事故8）",
                oxygen_cycles_left.unwrap()
            ),
            action: Action::ProduceOxygen,
            forbid_expansion: true,
            needs_ai: true,
        };
    }

    // ---- Level 1：食物续航告急 → 补给优先（）----
    let food_critical = food_cycles_left.map(|c| c < FOOD_CYCLES_CRITICAL).unwrap_or(false);
    if food_critical {
        return Directive {
            level: Level::Watch,
            reason: format!("🍞 食物仅够 {:.1} 周期 → 补给优先", food_cycles_left.unwrap()),
            action: Action::ForageFood,
            forbid_expansion: true,
            needs_ai: false,
        };
    }

    // ---- Level 1：空气比例低（还没到危险线，但趋势明确）----
    //   只有在**确实读到气体数据**时才用比例判断；读不到就别假设"空气差"。
    let has_atmos = snap.atmos.get("oxygenMassKg").and_then(|v| v.as_f64()).is_some();
    let breath = snap.breath_ratio();
    if has_atmos && breath < 0.75 {
        return Directive {
            level: Level::Watch,
            reason: format!(
                "🌬 可呼吸气体仅 {:.0}% → 尽快换气（CO₂ 沉底，往开阔处挖）",
                breath * 100.0
            ),
            action: Action::Ventilate,
            forbid_expansion: false,
            needs_ai: false,
        };
    }

    Directive {
        level: Level::Normal,
        reason: "✓ 无异常 → 按计划依赖图推进".into(),
        action: Action::FollowPlan,
        forbid_expansion: false,
        needs_ai: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_dupe(chore: &str, breath: f64, element: &str) -> crate::sense::Dupe {
        crate::sense::Dupe {
            name: "测试".into(),
            x: 10,
            y: 20,
            chore: chore.into(),
            breath: Some(breath),
            stress: Some(0.0),
            bladder: Some(0.0),
            reachable: Some(5),
            element: element.into(),
            blocked: 0,
            schedule: String::new(),
        }
    }

    /// 膀胱告急 + 没有厕所 → 必须抢占到"建厕所"。
    ///
    /// 根因链：没厕所 → 膀胱满 → 随地排泄 → 污染水
    /// 三人泡在污染水里→ 差点重演/9。
    #[test]
    fn high_bladder_without_toilet_triggers_build_toilet() {
        let mut snap = Snapshot::default();
        let mut d = mk_dupe("空闲", 100.0, "氧气");
        d.bladder = Some(97.0);
        snap.dupes.push(d);
        let judgment = crate::judge::judge(&snap, None);
        let dir = direct(&snap, &judgment, Some(3.0), false, false, None, None);
        assert_eq!(dir.action, Action::BuildToilet, "膀胱满且没厕所 → 必须建厕所");
        assert_eq!(dir.level, Level::Urgent);
    }

    /// 有厕所时膀胱告急**不该**抢占（厕所本身不是问题）。
    #[test]
    fn high_bladder_with_toilet_does_not_trigger() {
        let mut snap = Snapshot::default();
        let mut d = mk_dupe("空闲", 100.0, "氧气");
        d.bladder = Some(97.0);
        snap.dupes.push(d);
        // 厕所数读的是 diagnostics.buildings（与 judge 的口径一致）
        snap.diagnostics = serde_json::json!({"buildings": {"toilets": 1}});
        let judgment = crate::judge::judge(&snap, None);
        let dir = direct(&snap, &judgment, Some(3.0), false, false, None, None);
        assert_ne!(dir.action, Action::BuildToilet);
    }

    /// 厕所是**人均资源** —— 1 个旱厕给 3 个人用会排队，
    ///   膀胱照样涨到 80%+→ 随地排泄
    ///   污染水 → /9 的死亡路径。
    ///   旧逻辑只在"零厕所"时抢占 → 3 人 1 厕永远不建第二个 → 膀胱红线失守。
    #[test]
    fn high_bladder_with_one_toilet_for_three_triggers() {
        let mut snap = Snapshot::default();
        for _ in 0..3 {
            let mut d = mk_dupe("空闲", 100.0, "氧气");
            d.bladder = Some(84.0);
            snap.dupes.push(d);
        }
        snap.diagnostics = serde_json::json!({"buildings": {"toilets": 1}});
        let judgment = crate::judge::judge(&snap, None);
        let dir = direct(&snap, &judgment, Some(3.0), false, false, None, None);
        assert_eq!(dir.action, Action::BuildToilet, "1 个厕所 3 个人 → 排队 → 必须加厕所");
    }

    /// 正在如厕者的膀胱100%是正常现象，不该触发建厕所（否则每轮复活T1、抢占采食物）。
    #[test]
    fn toileting_dupe_bladder_does_not_preempt() {
        let mut snap = Snapshot::default();
        let mut a = mk_dupe("休息时间：如厕", 100.0, "氧气");
        a.bladder = Some(100.0);
        let mut b = mk_dupe("放松", 100.0, "氧气");
        b.bladder = Some(2.0);
        let mut c = mk_dupe("放松", 100.0, "氧气");
        c.bladder = Some(2.2);
        snap.dupes = vec![a, b, c];
        snap.diagnostics = serde_json::json!({"buildings": {"toilets": 2}});
        let judgment = crate::judge::judge(&snap, None);
        let dir = direct(&snap, &judgment, Some(5.0), false, false, None, None);
        assert_ne!(dir.action, Action::BuildToilet, "如厕中的膀胱100不该触发建厕所");
    }

    /// （吉恩之死）：人在**不可呼吸环境**里、呼吸值 77~93 时**必须**抢占救援。
    ///   旧逻辑用全局 min_breath < 50 → 人在 CO₂ 里慢慢窒息时一直不抢占 → 归零死亡。
    #[test]
    fn breather_in_co2_with_falling_breath_triggers_rescue() {
        let mut snap = Snapshot::default();
        let mut d = mk_dupe("发电", 88.0, "二氧化碳");
        d.breath = Some(88.0);
        snap.dupes.push(d);
        let judgment = crate::judge::judge(&snap, None);
        let dir = direct(&snap, &judgment, Some(10.0), false, false, Some(20.0), Some(5.0));
        assert_eq!(dir.level, Level::Urgent, "CO₂ 里呼吸值掉到 88 必须抢占救援");
    }

    /// 危险格判据按**浓度**校准 ——
    ///   基地 CO₂ 只有 ~1.15kg/格时复制人呼吸仍是 100（不算危险），
    ///   但污染水/真空/污染氧/氯气永远算危险。
    ///   一刀切排除 CO₂ 会让发电选址全否 → 永远造不出发电机 → 基地永远没氧气。
    #[test]
    fn hazard_cell_rules_by_concentration() {
        let mk = |el: &str, mass: f64| oni_world::cell::Cell {
            x: 0,
            y: 0,
            element: el.into(),
            element_name: String::new(),
            state: "gas".into(),
            solid: false,
            mass_kg: mass,
            buildings: vec![],
            designations: vec![],
        };
        assert!(!crate::exec::cell_is_hazardous(&mk("CarbonDioxide", 1.2)), "低浓度 CO2 安全");
        assert!(crate::exec::cell_is_hazardous(&mk("CarbonDioxide", 8.0)), "高浓度 CO2 致命");
        assert!(crate::exec::cell_is_hazardous(&mk("DirtyWater", 0.04)));
        assert!(crate::exec::cell_is_hazardous(&mk("ContaminatedOxygen", 0.1)));
        assert!(crate::exec::cell_is_hazardous(&mk("ChlorineGas", 0.1)));
        assert!(crate::exec::cell_is_hazardous(&mk("Vacuum", 0.0)));
        assert!(!crate::exec::cell_is_hazardous(&mk("Oxygen", 1.0)));
    }

    /// 调度器必须在"有人倒地"时给出最高优先级救援指令。
    #[test]
    fn downed_triggers_rescue() {
        let mut snap = Snapshot::default();
        snap.dupes.push(mk_dupe("失去行动能力", 0.0, "污染水"));
        let judgment = crate::judge::judge(&snap, None);
        let d = direct(&snap, &judgment, Some(3.0), true, false, None, None);
        assert_eq!(d.level, Level::Rescue);
        assert_eq!(d.action, Action::Rescue);
    }

    /// 呼吸值低（但没倒地）→ 紧急。
    #[test]
    fn low_breath_triggers_urgent() {
        let mut snap = Snapshot::default();
        snap.dupes.push(mk_dupe("空闲", 20.0, "二氧化碳"));
        let judgment = crate::judge::judge(&snap, None);
        let d = direct(&snap, &judgment, Some(3.0), false, false, None, None);
        assert_eq!(d.level, Level::Urgent);
    }

    /// 一切正常 → 按计划推进。
    #[test]
    fn normal_state_follows_plan() {
        let mut snap = Snapshot::default();
        snap.dupes.push(mk_dupe("空闲", 100.0, "氧气"));
        let judgment = crate::judge::judge(&snap, None);
        let d = direct(&snap, &judgment, Some(3.0), false, false, None, None);
        assert_eq!(d.level, Level::Normal);
        assert_eq!(d.action, Action::FollowPlan);
    }


    /// ：氧气续航告急时产氧优先 + 禁止扩张性挖掘。
    ///   氧气 2 周期、比例 0.5 → 必须 ProduceOxygen 且 forbid_expansion。
    #[test]
    fn low_oxygen_forbids_expansion() {
        let mut snap = Snapshot::default();
        snap.dupes.push(mk_dupe("空闲", 100.0, "氧气"));
        snap.atmos["oxygenMassKg"] = serde_json::json!(50.0);
        snap.atmos["carbonDioxideMassKg"] = serde_json::json!(50.0);
        let judgment = crate::judge::judge(&snap, None);
        let d = direct(&snap, &judgment, Some(10.0), false, false, Some(2.0), None);
        assert_eq!(d.action, Action::ProduceOxygen, "低氧必须产氧优先");
        assert!(d.forbid_expansion, "低氧必须禁止扩张性挖掘");
    }

    /// 氧气充足时不得误触发禁止扩张（否则正常挖掘被卡住）。
    #[test]
    fn healthy_oxygen_allows_expansion() {
        let mut snap = Snapshot::default();
        snap.dupes.push(mk_dupe("空闲", 100.0, "氧气"));
        let judgment = crate::judge::judge(&snap, None);
        let d = direct(&snap, &judgment, Some(10.0), false, false, Some(20.0), None);
        assert!(!d.forbid_expansion, "氧气充足不应禁止扩张");
    }

    /// 人在 CO₂ 格但呼吸正常 → 只记行动项，不抢占（避免饿死研究站）。
    #[test]
    fn unbreathable_cell_with_healthy_breath_does_not_preempt() {
        let mut snap = Snapshot::default();
        snap.dupes.push(mk_dupe("寻找安全区域", 100.0, "二氧化碳"));
        let judgment = crate::judge::judge(&snap, None);
        let d = direct(&snap, &judgment, Some(10.0), false, false, Some(20.0), Some(20.0));
        assert_eq!(d.level, Level::Watch, "呼吸正常不该抢占");
    }

    /// 人在不可呼吸格 且 呼吸低 → Urgent（真危险，必须抢占救人）。
    #[test]
    fn unbreathable_cell_with_low_breath_preempts() {
        let mut snap = Snapshot::default();
        snap.dupes.push(mk_dupe("空闲", 10.0, "二氧化碳"));
        let judgment = crate::judge::judge(&snap, None);
        let d = direct(&snap, &judgment, Some(10.0), false, false, Some(20.0), Some(20.0));
        assert_eq!(d.level, Level::Urgent, "呼吸低必须抢占");
    }

    /// 人在不可呼吸的格子里（污染水/CO₂）→ 必须能识别出来。
    #[test]
    fn unbreathable_cell_is_detected() {
        assert!(mk_dupe("空闲", 100.0, "污染水").in_unbreathable());
        assert!(mk_dupe("空闲", 100.0, "二氧化碳").in_unbreathable());
        assert!(!mk_dupe("空闲", 100.0, "氧气").in_unbreathable());
    }

    #[test]
    fn low_food_triggers_forage() {
        let mut snap = Snapshot::default();
        snap.dupes.push(mk_dupe("空闲", 100.0, "氧气"));
        let judgment = crate::judge::judge(&snap, None);
        // 阈值改成 0.5 周期（真正快饿死了）才抢占。
        let d = direct(&snap, &judgment, Some(10.0), false, false, Some(20.0), Some(0.4));
        assert_eq!(d.action, Action::ForageFood);
        assert_eq!(d.level, Level::Watch);
    }

    /// 教训：食物"还够 2 周期"时**不该**抢占主线。
    ///   旧阈值 3.0 → 只要 < 3 周期就永远抢占收割节点 A2
    ///   surface（挖地表）/供电/研究全部饿死，代理 10 秒一轮重复收割。
    ///   收割是**应急**手段，不能当成常态调度器。
    #[test]
    fn moderate_food_does_not_preempt_main_plan() {
        let mut snap = Snapshot::default();
        snap.dupes.push(mk_dupe("空闲", 100.0, "氧气"));
        let judgment = crate::judge::judge(&snap, None);
        let d = direct(&snap, &judgment, Some(10.0), false, false, Some(20.0), Some(2.0));
        assert_ne!(d.action, Action::ForageFood, "2 周期食物不该抢占主线");
    }
}
