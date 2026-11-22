//! LEDGER —— 资产负债表：把瞬时值换算成"还能撑几周期"，排出"谁会先杀死我们"。

use crate::judge::KCAL_PER_DUPE_CYCLE;
use crate::sense::Snapshot;
use crate::trend::Trend;

#[derive(Debug, Clone)]
pub struct Ledger {
    pub dupe_count: usize,
    pub food_cycles: Option<f64>,
    pub oxygen_cycles: Option<f64>,
    pub power_gap_w: f64,
    pub first_death_cause: String,
    pub warnings: Vec<String>,
}

pub fn ledger(s: &Snapshot, t: &Trend) -> Ledger {
    let n = s.dupes.len().max(1) as f64;
    let food_cycles = if s.food_kcal() > 0.0 {
        Some(s.food_kcal() / (n * KCAL_PER_DUPE_CYCLE))
    } else {
        None
    };
    let oxygen_cycles = t.oxygen_cycles_left;
    let power_gap_w = s
        .power
        .get("consumerNeededWatts")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0)
        - s.power.get("netActiveWatts").and_then(|v| v.as_f64()).unwrap_or(0.0);

    let mut warnings = Vec::new();
    let mut first = String::new();
    let mut best = f64::MAX;
    if let Some(fc) = food_cycles {
        warnings.push(format!("食物 ≈ {:.1} 周期", fc));
        if fc < best {
            best = fc;
            first = format!("断粮（{fc:.1} 周期）");
        }
    }
    if let Some(oc) = oxygen_cycles {
        warnings.push(format!("氧气 ≈ {:.1} 周期（实测速率）", oc));
        if oc < best {
            best = oc;
            first = format!("缺氧（{oc:.1} 周期）");
        }
    }
    // ★★ 生活区空气（封闭气室）—— 2026-11-22 新增。
    //   全局氧气把殖民地够不到的气室也算进去 → 上一局就是在「指标正常」中闷死的。
    //   这里用「生活区氧气量 ÷ 实测每周期耗氧」算续航，比全局口径更接近真实死因。
    let living_cycles = s
        .living_oxygen_kg()
        .zip(t.oxygen_per_cycle)
        .filter(|(_, per)| *per > 0.0)
        .map(|(kg, per)| kg / per);
    if let Some(lc) = living_cycles {
        warnings.push(format!("生活区空气 ≈ {lc:.1} 周期（封闭气室 · 全局指标看不见）"));
        if lc < best {
            best = lc;
            first = format!("生活区缺氧（{lc:.1} 周期）");
        }
    }
    if power_gap_w > 0.0 {
        warnings.push(format!("电力缺口 {:.0} W", power_gap_w));
    }
    if first.is_empty() {
        first = "暂无明确死因".into();
    }
    Ledger { dupe_count: s.dupes.len(), food_cycles, oxygen_cycles, power_gap_w, first_death_cause: first, warnings }
}
