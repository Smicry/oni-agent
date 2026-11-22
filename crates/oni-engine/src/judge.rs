//! JUDGE —— 阈值表判定，输出的是**判定**不是建议。

use crate::sense::Snapshot;
use crate::trend::Trend;

pub const BREATH_FLOOR: f64 = 35.0;
pub const STRESS_CEIL: f64 = 75.0;
pub const FOOD_FLOOR: f64 = 4000.0;
pub const OXYGEN_FLOOR: f64 = 120.0;
pub const REACH_FLOOR: i64 = 5;   // 开局只有十几格可达，别误报
/// **每人每周期消耗的热量（kcal）**。
///
/// 修正：旧值 2000 错了 5 倍。
///   ONI 官方：复制人**每周期吃 400 kcal**。
///   旧值把开局 16000kcal 算成「只够 2.7 周期」→ 账本把「断粮」排成第一死因
///   → 代理去抢收割节点 → surface/供电/研究全部饿死（）。
///   修正后同一局是 13.3 周期 → 断粮不是红线，产氧/厕所才是。
pub const KCAL_PER_DUPE_CYCLE: f64 = 400.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Danger,
    Idle,
    Stall,
    Ok,
}

impl Verdict {
    pub fn label(&self) -> &'static str {
        match self {
            Verdict::Danger => "DANGER",
            Verdict::Idle => "IDLE",
            Verdict::Stall => "STALL",
            Verdict::Ok => "OK",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Judgment {
    pub verdict: Verdict,
    pub dangers: Vec<String>,
    /// 危险**类别**（不含会变的数值）——用来判断"是不是同一个危险"。
    ///
    /// 为什么必须分���：早期把数值也拼进键里，结果呼吸值每轮都变
    ///   键每轮都变 → 紧急暂停的冷却永远不生效 → 每 10 秒重新暂停一次，
    ///   复制人反复被冻在坑里窒息。
    pub danger_keys: Vec<String>,
    pub actions: Vec<String>,
    pub notes: Vec<String>,
}

pub fn judge(s: &Snapshot, trend: Option<&Trend>) -> Judgment {
    let mut dangers = Vec::new();
    let mut danger_keys: Vec<String> = Vec::new();
    let mut actions = Vec::new();
    let mut notes = Vec::new();

    for d in &s.dupes {
        if let Some(b) = d.breath {
            if b < BREATH_FLOOR {
                dangers.push(format!("{} 呼吸仅 {b}", d.name));
                danger_keys.push(format!("breath@{}", d.name));
            }
        }
        if let Some(st) = d.stress {
            if st > STRESS_CEIL {
                dangers.push(format!("{} 压力 {st}", d.name));
                danger_keys.push(format!("stress@{}", d.name));
            }
        }
        if let Some(r) = d.reachable {
            if r < REACH_FLOOR {
                dangers.push(format!("{} 可达格仅 {r}（疑似被封）", d.name));
                danger_keys.push(format!("reach@{}", d.name));
            }
        }
    }
    // 大气质量：CO₂ 比氧气还多 = 全局中毒（不只是某个坑）
    // 空气红线：这是开局第一位死因。
    //   密闭空间里氧气"总量"够不代表够呼吸 —— 看的是**比例**。
    let breath = s.breath_ratio();
    // 空气红线必须看**复制人实际的呼吸状态**，不能只看全局气体比例。
    //   CO₂ 比氧气重 → 沉在坑底。全局比例被坑底的 CO₂ 拉低，但人住在上层空腔，
    //   呼吸值 100/100/99 完全健康。此时判 DANGER 会让代理进入紧急态
    //   → 施工节奏被腰斩，还反复重下工单。
    //   真正该判危险的是：**有人呼吸值真的偏低**。
    let worst_breath = s.min_breath();
    // 保守性修正：
    //   旧代码 `unwrap_or(false)` = **拿不到呼吸值就不判危险**。
    //   复制人泡在 CO₂ 里呼吸 0、已失去行动能力时，dupes 数据一度读失败
    //   判定变成 OK → 代理继续挖矿 → 三个复制人在 CO₂ 里全部死亡。
    //   读不到呼吸值 ≠ 安全。拿不到就当**可能有危险**（至少不判 OK）。
    let someone_struggling = match worst_breath {
        Some(b) => b < BREATH_FLOOR,
        None => !s.dupes.is_empty(),   // 有复制人但读不到呼吸值 → 保守判危险
    };
    if someone_struggling {
        dangers.push(format!("有复制人呼吸值仅 {:?}（阈值 {}）→ 随时可能窒息", worst_breath, BREATH_FLOOR));
        danger_keys.push("air".into());
    } else if breath < 0.55 {
        // 比例低但人人呼吸正常 = CO₂ 沉在坑底 → 是**行动项**不是**危险项**
        actions.push(format!("可呼吸气体比例仅 {:.0}%（CO₂ 沉在低处，人暂时安全）→ 尽快产氧/挖通地表换气", breath * 100.0));
        notes.push(format!("CO₂ 占比高但复制人呼吸正常 → 暂不判危险，继续施工换气"));
    } else if breath < 0.75 {
        actions.push(format!("可呼吸气体降到 {:.0}% → 尽快产氧/挖通地表换气", breath * 100.0));
    }
    // 人口为 0 的判定必须**先确认数据可信**：
    //   观察源读取失败时 dupes 列表会空，但人还活着。
    //   误判成"人口为 0" → 代理完全停手 → 施工全停。
    //   数据不可信时只记 notes，不判危险、不停手。
    let dupes_read_failed = s.errors.iter().any(|e| {
        e.contains("dupes") || e.contains("todos") || e.contains("status-check")
    });
    // 倒地的复制人 = 最高优先级：他们**不会执行任何差事**，
    //   代理还在下工单也白搭 —— 必须立刻转入救援。
    if s.has_downed() {
        dangers.push(format!(
            "⛑ {} 个复制人失去行动能力（倒地）→ 不会干活，必须立刻救援（排毒/拖到有空气处/治疗）",
            s.downed_count()
        ));
        danger_keys.push("downed".into());
    }    if s.dupes.is_empty() && dupes_read_failed {
        notes.push("复制人数据读取失败（观察源错误）→ 无法确认人口，**不判人口为 0**，继续施工".into());
    } else if s.dupes.is_empty() {
        dangers.push("人口为 0 —— 殖民地已无人".into());
        danger_keys.push("no_population".into());
    }
    let ratio = s.co2_ratio();
    // 阈值按开局校准，且**必须结合复制人实际状态**：
    //   3 个人的殖民地 CO₂/O₂ > 1 是常态（人呼吸就产 CO₂，密闭空间必然累积）。
    //   只看全局比例 → 每次都判危险 → 代理反复进入紧急态
    //   → 施工节奏被腰斩，换气反而更慢。
    //   真正危险的信号是**有人呼吸值低**或 mod 报窒息风险 —— 那两个已经在上面判过了。
    //   所以这里降级为行动项；只有比例极端高 (>2.0) 才算危险。
    if ratio > 2.0 {
        dangers.push(format!("CO₂ 质量是氧气的 {:.1} 倍 → 全局严重缺氧", ratio));
        danger_keys.push("co2_global".into());
    } else if ratio > 1.0 {
        actions.push(format!("CO₂/氧气 = {:.2}（空气在变脏，但人人呼吸正常）→ 继续挖通地表换气", ratio));
    }
    let risk = s.suffocation_risk();
    if !risk.is_empty() && risk != "nominal" {
        dangers.push(format!("全局窒息风险 = {risk}"));
        danger_keys.push(format!("suffocation:{risk}"));
    }
    // 数据可信度：读不到 food 数据时**不能**判"断粮"。
    //   快照里 food 是从 `oni://resources/food` 解析的；源失败时它是 Null → 0。
    //   0 会被当成"食物耗尽"判危险 —— 与"读不到呼吸值"是同一类错误。
    let has_food_data = s.food.get("totalKcal").is_some()
        || s.food.get("foods").is_some()
        || s.diagnostics.get("foodKcal").is_some();
    if s.food_kcal() < FOOD_FLOOR && has_food_data {
        dangers.push(format!("食物仅 {:.0} kcal", s.food_kcal()));
        danger_keys.push("food".into());
    } else if s.food_kcal() < FOOD_FLOOR {
        notes.push("食物数据不可读 → 暂不判断粮（不把读不到当成耗尽）".into());
    }
    // 同理：读不到氧气数据时不能判"缺氧"。
    let has_o2_data = s.diagnostics.get("oxygenKgVisible").is_some()
        || s.atmos.get("oxygenMassKg").is_some();
    if s.oxygen_kg() < OXYGEN_FLOOR && has_o2_data {
        dangers.push(format!("可见氧气仅 {:.0} kg", s.oxygen_kg()));
        danger_keys.push("oxygen".into());
    }
    for a in s.alerts() {
        if a.starts_with("[critical]") {
            dangers.push(format!("严重警报: {a}"));
            danger_keys.push(format!("alert:{}", a.chars().skip(1).take(12).collect::<String>()));
        } else {
            notes.push(a);
        }
    }
    if !s.errors.is_empty() {
        notes.push(format!("⚠ {} 个观察源读取失败（局部失败≠正常）", s.errors.len()));
    }

    // 区分"在休息/睡觉"与"真的没活干"：
    //   官方 workIdleCount 把睡觉的人也算 idle → 常显"全员空闲 3/3"，
    //   看起来像代理卡住，其实他们只是在睡觉 → 会导致误停机/误熔断。
    let idle = s.idle_count();
    let resting = s.resting_count();
    let truly_idle = s.truly_idle_count();
    if !s.dupes.is_empty() && truly_idle == s.dupes.len() {
        notes.push(format!("全员无差事 {truly_idle}/{}", s.dupes.len()));
        actions.push("世界无有效指令 → 按依赖图取下一个目标（不要加轮次）".into());
    } else if !s.dupes.is_empty() && idle == s.dupes.len() && resting > 0 {
        notes.push(format!(
            "全员空闲 {idle}/{}（其中 {resting} 人在休息/睡觉 → 醒来会干活，不是空跑）",
            s.dupes.len()
        ));
    }
    if s.air_quality() == "poor" {
        actions.push(" 空气质量差 → 优先挖通地表换气（CO₂ 比氧气重，会沉在坑底）".into());
    }
    if s.building("toilets") == 0 {
        actions.push("没有厕所（厕所=0）".into());
    }
    if s.building("beds") < s.dupes.len() as i64 {
        actions.push(format!("床位不足 {}/{}", s.building("beds"), s.dupes.len()));
    }
    // 电力字段在 `summary` 里，不在顶层
    if s.power_int("batteryCount") == 0 {
        actions.push("没有电池 → 发电的电没有去处".into());
    }
    if s.power_int("generatorEmpty") == 1 {
        actions.push(" 发电机没有燃料 → 实际 0 瓦（必须送煤进去）".into());
    }
    if s.power_int("generatorCount") > 0 && s.power_int("generatorOperational") == 0 {
        actions.push(" 发电机存在但没在发电（缺燃料或没人操作）".into());
    }
    if let Some(t) = trend {
        if let Some(cycles) = t.oxygen_cycles_left {
            notes.push(format!("氧气按实测速率还够 ~{cycles:.1} 个周期"));
            if cycles < 2.0 {
                actions.push(" 氧气 2 周期内见底 → 立刻改计划（先做产氧/换气）".into());
            }
        }
        if let Some(cycles) = t.food_cycles_left {
            notes.push(format!("食物按实测速率还够 ~{cycles:.1} 个周期"));
            if cycles < 2.0 {
                actions.push(" 食物 2 周期内见底 → 立刻改计划".into());
            }
        }
    }
    if s.paused {
        notes.push("游戏处于暂停".into());
    }

    let verdict = if !dangers.is_empty() {
        Verdict::Danger
    } else if idle == s.dupes.len() && !s.dupes.is_empty() {
        Verdict::Idle
    } else {
        Verdict::Ok
    };
    Judgment { verdict, dangers, danger_keys, actions, notes }
}

/// 人口为 0（殖民地已无人）的判定。
///
/// 为什么单独抽成函数：人口为 0 时**紧急态占空比帮不上忙**——
/// 没人能靠"跑 120s"自救（mod 无拖人/治疗工单，倒地/死亡不可自愈），
/// 占空比只会白烧游戏周期并持续唤醒代理。判定必须放在紧急态分支**之前**。
pub fn is_no_population(danger_keys: &[String]) -> bool {
    danger_keys.iter().any(|k| k == "no_population")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_population_detected() {
        assert!(is_no_population(&["no_population".to_string()]));
        assert!(is_no_population(&[
            "air_low".to_string(),
            "no_population".to_string()
        ]));
    }

    #[test]
    fn no_population_absent() {
        assert!(!is_no_population(&[]));
        assert!(!is_no_population(&["downed".to_string()]));
        assert!(!is_no_population(&["breath@博特".to_string()]));
        // 前缀相同但不是同一个键 → 不能误判
        assert!(!is_no_population(&["no_population_x".to_string()]));
    }
}
