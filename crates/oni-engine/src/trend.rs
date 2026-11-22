//! TREND —— 用**实测速率**换算"还能撑几周期"，并在耗尽前预警。
//!
//! 教训：只看瞬时值会"发现问题时已经晚了"。这里每轮落一个样本，
//! 用相邻快照的 Δ/Δcycle 算消耗速率，再算 runway。

use std::fs;
use std::io::Write;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::sense::Snapshot;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sample {
    pub ts: i64,
    pub cycle: i64,
    pub oxygen: f64,
    pub food: f64,
    pub stress: f64,
    pub idle: usize,
    /// 这一轮采样时**世界是否在跑**。
    ///
    /// ★ 为什么必须有（2026-11-21 事故 7）：
    ///   暂停期的样本不能用于趋势/异常判定 —— 世界没动 → 氧气纹丝不动
    ///   → 会被误判成"人已经不再呼吸"（实测：开局暂停期被误报氧气冻结 → 抢占正常计划）。
    ///   旧样本没有这个字段 → serde 默认 false → 自动被排除（也顺带排除了旧存档的样本）。
    #[serde(default)]
    pub running: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Trend {
    pub samples: usize,
    /// 每周期氧气衰减（kg/cycle，正数表示在掉）
    pub oxygen_per_cycle: Option<f64>,
    /// 每周期食物消耗（kcal/cycle）
    pub food_per_cycle: Option<f64>,
    pub oxygen_cycles_left: Option<f64>,
    pub food_cycles_left: Option<f64>,
    /// **stress 是否在上升**（窒息的早期预警）。
    ///
    /// ★ 为什么需要（2026-11-21 复盘）：死亡前 stress 从 0 飙到 3.3 → 5.1，
    ///   比"呼吸值掉到 0"早好几分钟 —— 那是唯一来得及救人的信号。
    pub stress_rising: bool,
    /// **氧气消耗是否归零**（人不动了）。
    ///
    /// 复盘证据：死亡前氧气从 t+191min 起**完全冻结**（157.3 纹丝不动）——
    /// 人已经不再呼吸。趋势模块以前只做线性外推，从不报这种异常。
    pub oxygen_frozen: bool,
}

pub fn sample_path() -> PathBuf {
    oni_mcp::save::save_file("snapshots.jsonl")
}

pub fn record(s: &Snapshot) -> std::io::Result<()> {
    let path = sample_path();
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    // ★ 存档切换 → 旧样本作废：cycle 回退（新存档从 Cycle 1 开始）就是换了存档。
    //   不清掉的话，新存档的 stress 基线会拿**旧存档**的 0.0 当基线 → 开局就误判"stress 飙升"。
    if let Ok(text) = fs::read_to_string(&path) {
        if let Some(last) = text
            .lines()
            .filter_map(|l| serde_json::from_str::<Sample>(l).ok())
            .last()
        {
            if s.cycle < last.cycle {
                let _ = fs::write(&path, "");
            }
        }
    }
    let sample = Sample {
        ts: s.ts,
        cycle: s.cycle,
        oxygen: s.oxygen_kg(),
        food: s.food_kcal(),
        stress: s.max_stress(),
        idle: s.idle_count(),
        running: !s.paused,
    };
    let mut f = fs::OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(f, "{}", serde_json::to_string(&sample).unwrap_or_default())?;
    Ok(())
}

pub fn load_samples(max: usize) -> Vec<Sample> {
    let path = sample_path();
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut v: Vec<Sample> = text
        .lines()
        .filter_map(|l| serde_json::from_str::<Sample>(l).ok())
        .collect();
    if v.len() > max {
        v.drain(0..v.len() - max);
    }
    v
}

/// stress 相对基线的**显著上升**阈值。
///
/// ★ 量纲纪律：maxStress 是 0–100 制的百分比，开局基线就有 4.4~4.7（完全正常）。
///   固定绝对阈值（曾用 3.0）会每轮误判"有人在受苦" → 抢占正常计划 → 卡死开局。
///   相对上升量才能同时做到「抓住 0→3.3 的窒息前兆」和「不误报 4.4 的常态」。
pub const STRESS_RISE_DELTA: f64 = 2.0;

/// stress 基线：最近 N 个**历史**样本的中位数。
///
/// ★ 两条纪律（2026-11-21 事故 7）：
///   1. **排除当前这一条**（按时间戳，不按周期）：当前值就是要判的量，
///      拿它当基线等于自己跟自己比。
///      ★ 为什么不能按周期排除：新存档开局整个 Cycle 1 都有几十条样本，
///        按 cycle 排除会把基线清空 → 预警永远不触发（瞎）。
///   2. **只看最近 15 分钟**：旧存档的样本（几小时前）不能当基线 ——
///      否则新存档开局会拿旧存档的 stress=0.0 当基线，
///      把正常的 4.4 误判成"飙升 4.4" → 每轮抢占正常计划 → 开局卡死（实测）。
pub const BASELINE_WINDOW_SECS: i64 = 900;

pub fn stress_baseline(samples: &[Sample], current: &Snapshot) -> Option<f64> {
    let cutoff = current.ts - BASELINE_WINDOW_SECS;
    let mut vals: Vec<f64> = samples
        .iter()
        .filter(|s| s.running && s.ts < current.ts && s.ts >= cutoff)
        .rev()
        .take(10)
        .map(|s| s.stress)
        .collect();
    if vals.is_empty() {
        return None;
    }
    vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    Some(vals[vals.len() / 2])
}

/// 用最近两个**不同周期**的样本算速率，再算 runway。
pub fn compute(samples: &[Sample], current: &Snapshot) -> Trend {
    let mut t = Trend { samples: samples.len(), ..Default::default() };
    // ★ 只用**世界真的跑过**的样本算速率：暂停期的样本没有信息量。
    let mut prev: Option<&Sample> = None;
    for s in samples.iter().rev() {
        if s.running && s.cycle != current.cycle {
            prev = Some(s);
            break;
        }
    }
    if let Some(p) = prev {
        let dc = (current.cycle - p.cycle).max(1) as f64;
        let o_drop = (p.oxygen - current.oxygen_kg()) / dc;
        let f_drop = (p.food - current.food_kcal()) / dc;
        if o_drop > 0.0 {
            t.oxygen_per_cycle = Some(o_drop);
            t.oxygen_cycles_left = Some(current.oxygen_kg() / o_drop);
        }
        if f_drop > 0.0 {
            t.food_per_cycle = Some(f_drop);
            t.food_cycles_left = Some(current.food_kcal() / f_drop);
        }
    }
    // ---- ★ 异常检测（2026-11-21 复盘新增）----
    // 趋势模块以前只做线性外推（"还够几周期"），从不说"氧气不消耗了"。
    // 死亡复盘显示：氧气完全冻结 + stress 飙升 是最早、最准的预警。
    // ★ stress 上升：**相对基线**判定（固定阈值会被常态基线骗）
    //   旧逻辑：最近 3 个样本均值 > 更早 3 个均值 + 0.5 → 开局 4.4 vs 4.7 就算"飙升"
    //   → 每轮判 Urgent → 抢占正常计划 → 卡死开局（实测）。
    if let Some(base) = stress_baseline(samples, current) {
        let cur = current.max_stress();
        t.stress_rising = cur - base >= STRESS_RISE_DELTA && cur >= 2.0;
    }
    // ★ 氧气冻结：只在**世界真的跑过**的样本里判断。
    //   旧逻辑没看 running → 暂停期氧气当然不变 → 误报"人不再呼吸"（事故 7）。
    let running: Vec<&Sample> = samples.iter().filter(|s| s.running).rev().take(6).collect();
    if running.len() >= 3 {
        let o2s: Vec<f64> = running.iter().map(|s| s.oxygen).collect();
        let o2_span = o2s.iter().cloned().fold(f64::MIN, f64::max)
            - o2s.iter().cloned().fold(f64::MAX, f64::min);
        t.oxygen_frozen = o2_span < 1.0 && current.oxygen_kg() > 1.0;
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(ts: i64, cycle: i64, stress: f64) -> Sample {
        Sample { ts, cycle, oxygen: 100.0, food: 1000.0, stress, idle: 0, running: true }
    }

    /// ★ 旧存档的样本（几小时前）不能当基线 —— 否则新存档开局会误判 stress 飙升。
    #[test]
    fn baseline_ignores_stale_samples() {
        let now = 1_000_000i64;
        let mut snap = Snapshot::default();
        snap.ts = now;
        let samples = vec![
            sample(now - 7200, 4, 0.0),   // 旧存档：2 小时前
            sample(now - 60, 1, 4.4),     // 当前存档
            sample(now - 50, 1, 4.7),
        ];
        let base = stress_baseline(&samples, &snap).expect("baseline");
        assert!(base >= 4.0, "基线必须是当前存档的常态值，实际 {base}");
    }

    /// 基线要用**当前存档**最近的样本，不能把当前这一条算进去。
    #[test]
    fn baseline_excludes_current_sample() {
        let now = 2_000_000i64;
        let mut snap = Snapshot::default();
        snap.ts = now;
        let samples = vec![
            sample(now - 100, 1, 2.0),
            sample(now - 50, 1, 2.1),
            sample(now, 1, 99.0),   // 当前这一条（异常高）
        ];
        let base = stress_baseline(&samples, &snap).expect("baseline");
        assert!(base < 10.0, "基线不能被当前样本污染，实际 {base}");
    }

    /// 没有历史样本时 → None（不猜、不用默认值当基线）。
    #[test]
    fn baseline_none_without_history() {
        let mut snap = Snapshot::default();
        snap.ts = 3_000_000;
        assert!(stress_baseline(&[], &snap).is_none());
    }
}
