//! **慢系统主动巡检（audit）**：不等快系统喊人，自己体检。
//!
//! 为什么必须有：
//!   快→慢的 escalation 是**被动**的（出问题才喊）。
//!   但很多问题是**慢慢恶化**的：CO₂ 占比在涨、梯子链悄悄断了、
//!   产氧器建好了但没投料、电网接了但没负载……
//!   等到出事再喊，已经晚了。
//!   ⇒ 慢系统要有一份**体检清单**，周期性跑一遍，主动发现并干预。
//!
//! 与 escalation 的分工：
//!   audit    = 慢系统**主动**巡检（周期性，无故障也能发现问题）
//!   escalate = 快系统**被动**喊人（遇到解不开的情况）
//!   两者都产出"问题"，由慢系统统一决策。

use crate::sense::Snapshot;

/// 严重程度。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// 记录一下
    Info,
    /// 该处理了
    Warn,
    /// 急性，再拖会死人
    Critical,
}

impl Severity {
    pub fn label(&self) -> &'static str {
        match self {
            Severity::Info => "提示",
            Severity::Warn => "警告",
            Severity::Critical => "紧急",
        }
    }
}

/// 一个体检发现的问题。
#[derive(Debug, Clone)]
pub struct Issue {
    pub severity: Severity,
    /// 稳定的机器可读标识（慢系统可据此写规则）
    pub code: String,
    /// 人话描述
    pub detail: String,
    /// 慢系统可采取的动作建议
    pub suggestion: String,
    /// 建议关联的计划节点（若有）
    pub node: String,
}

/// 体检报告。
#[derive(Debug, Clone, Default)]
pub struct Report {
    pub issues: Vec<Issue>,
}

impl Report {
    pub fn worst(&self) -> Option<Severity> {
        self.issues.iter().map(|i| i.severity).max()
    }
    pub fn critical_count(&self) -> usize {
        self.issues.iter().filter(|i| i.severity == Severity::Critical).count()
    }
    /// 人话摘要（给慢系统看 / 写进日志）。
    pub fn summary(&self) -> String {
        if self.issues.is_empty() {
            return "🩺 体检：未发现问题".to_string();
        }
        self.issues
            .iter()
            .map(|i| {
                format!(
                    "{}[{}] {} → {}",
                    match i.severity {
                        Severity::Critical => "🔴",
                        Severity::Warn => "🟡",
                        Severity::Info => "⚪",
                    },
                    i.code,
                    i.detail,
                    i.suggestion
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// 慢系统主动能力：把体检发现持久化到协同总线（可回放、可去重、有 TTL）。
///   此前 audit 只在 CLI 打印，发现过眼就忘；写入 bus 后慢系统一处看全、不丢事件。
///   用 code 当 node，命中 bus::inbox 的 (kind,node) 去重（同一问题不刷屏）。
///   返回本次新发布的事件数。
pub fn persist(report: &Report) -> usize {
    let mut n = 0;
    for issue in &report.issues {
        let (prio, ttl) = match issue.severity {
            Severity::Critical => (crate::bus::Priority::Critical, 1800i64),
            Severity::Warn => (crate::bus::Priority::Warn, 3600),
            Severity::Info => (crate::bus::Priority::Info, 7200),
        };
        let payload = serde_json::json!({
            "code": issue.code,
            "suggestion": issue.suggestion,
            "severity": issue.severity.label(),
        });
        let id = crate::bus::publish(
            crate::bus::Kind::AuditFinding,
            prio,
            &issue.code, // 用 code 当 node → 去重
            &issue.detail,
            payload,
            ttl,
        );
        if id > 0 { n += 1; }
    }
    n
}

/// 慢系统主动能力：判断体检报告是否含"该立刻暂停调整"的红线。
///   只返回是否需要慢系统介入，**不改计划、不下工单、不 pause** —— 决策权仍在慢系统。
///   红线 = 有 Critical 级问题，或人口下降/断氧这两类急性问题。
pub fn redline(report: &Report) -> bool {
    report.issues.iter().any(|i| {
        i.severity == Severity::Critical
            || i.code == "pop.drop"
            || i.code == "air.no_oxygen"
    })
}

/// **跑一次体检**。
pub fn run(
    snap: &Snapshot,
    t: i64,
    last_pop: usize,
    food_cycles: Option<f64>,
    power: Option<&serde_json::Value>,
) -> Report {
    let mut r = Report::default();
    let pop = snap.dupes.len() as i64;

    if last_pop > 0 && pop < last_pop as i64 {
        r.issues.push(Issue {
            severity: Severity::Critical,
            code: "pop.drop".into(),
            detail: format!("人口从 {last_pop} 掉到 {pop} —— 有人没了"),
            suggestion: "立刻查：谁死的、死在哪（污染水？CO₂？坠落？），并回滚存档".into(),
            node: String::new(),
        });
    }

    let toilets = snap.building("toilets");
    let max_bladder = snap.max_active_bladder();
    if max_bladder > 80.0 && toilets < pop {
        r.issues.push(Issue {
            severity: if max_bladder >= 95.0 { Severity::Critical } else { Severity::Warn },
            code: "hygiene.no_toilet".into(),
            detail: format!(
                "膀胱最高 {max_bladder:.0}%（阈值 80）且厕所 {toilets} < 人口 {pop}（含排队中）→ 会随地排泄 → 污染水"
            ),
            suggestion: "优先建厕所（人均 1 个，且必须建在富氧区、底边要有地板砖）".into(),
            node: String::new(),
        });
    }

    let beds = snap.building("beds");
    if beds < pop {
        r.issues.push(Issue {
            severity: Severity::Warn,
            code: "sleep.no_bed".into(),
            detail: format!("床位 {beds} < 人口 {pop} → 睡地上累积压力"),
            suggestion: "建床（2x2，要地板砖 + 富氧格）".into(),
            node: String::new(),
        });
    }

    let ratio = snap.breath_ratio();
    if ratio < 0.75 {
        r.issues.push(Issue {
            severity: if ratio < 0.6 { Severity::Critical } else { Severity::Warn },
            code: "air.co2_ratio".into(),
            detail: format!("空气质量比 O₂/(O₂+CO₂) = {:.0}%（应 ≥ 80%）", ratio * 100.0),
            suggestion: "打通地表或拓宽通风井（1 格宽竖井排不掉 CO₂）；居住区搬到上方富氧层".into(),
            node: String::new(),
        });
    }

    let oxy_buildings = snap.building_count("MineralDeoxidizer")
        + snap.building_count("Electrolyzer")
        + snap.building_count("AlgaeHabitat");
    if oxy_buildings > 0 && ratio < 0.8 {
        r.issues.push(Issue {
            severity: Severity::Warn,
            code: "oxygen.stalled".into(),
            detail: format!(
                "有 {oxy_buildings} 台产氧设备，但空气比仍只有 {:.0}% → 可能没投料/没接线/泡在污染区",
                ratio * 100.0
            ),
            suggestion: "查产氧器：① 投入口有氧石吗 ② 通电了吗 ③ 建在富氧格吗（三件套）".into(),
            node: String::new(),
        });
    }

    if let Some(p) = power {
        let gen = p.get("generatorOperational").and_then(|v| v.as_i64()).unwrap_or(0);
        let used = p.get("consumerUsedWatts").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let consumers = p.get("consumerCount").and_then(|v| v.as_i64()).unwrap_or(0);
        if gen > 0 && consumers > 0 && used <= 0.0 {
            r.issues.push(Issue {
                severity: Severity::Warn,
                code: "power.not_connected".into(),
                detail: "发电机在运转但 consumerUsedWatts=0 → 耗电建筑没接上电网（造出来 ≠ 有电）".into(),
                suggestion: "接线：发电机 → 每个耗电建筑（auto_connect）".into(),
                node: String::new(),
            });
        }
    }

    if let Some(c) = food_cycles {
        if c < 3.0 {
            r.issues.push(Issue {
                severity: if c < 1.0 { Severity::Critical } else { Severity::Warn },
                code: "food.runway".into(),
                detail: format!("食物还能撑 {c:.1} 个周期（应 ≥ 3）"),
                suggestion: "收割可食植物（零风险应急）+ 尽早建农场（土培砖要黏土/泥土）".into(),
                node: String::new(),
            });
        }
    }

    let polluted = snap.atmos_num("contaminatedOxygenKg");
    if polluted > 40.0 {
        r.issues.push(Issue {
            severity: Severity::Warn,
            code: "hygiene.polluted_air".into(),
            detail: format!("污染氧 {polluted:.0}kg → 有复制人在脏环境里（污染水/未擦洗）"),
            suggestion: "区域擦洗污染格；把居住区搬离污染源".into(),
            node: String::new(),
        });
    }

    if t > 3 && snap.oxygen_kg() <= 0.0 {
        r.issues.push(Issue {
            severity: Severity::Critical,
            code: "air.no_oxygen".into(),
            detail: "氧气读数为 0 → 殖民地已无氧".into(),
            suggestion: "立刻产氧 + 挖通地表（这是红线）".into(),
            node: String::new(),
        });
    }

    r
}
