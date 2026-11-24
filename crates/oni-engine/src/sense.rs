//! SENSE —— 一次采全全局局面。

use std::collections::BTreeMap;
use std::time::Instant;

use oni_mcp::McpSession;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 观察源清单。**新增源必须同时加进这里**，否则"全局"名不副实。
pub const SOURCES: &[(&str, &str)] = &[
    ("diagnostics", "oni://colony/diagnostics"),
    ("status", "oni://colony/status"),
    ("todos", "oni://dupes/todos"),
    ("status_check", "oni://dupes/status-check"),
    ("atmos", "oni://atmosphere/breathability"),
    ("gas", "oni://fluid/gas/summary"),
    ("rooms", "oni://rooms/list"),
    ("power", "oni://power/summary"),
    ("food", "oni://food/summary"),
    ("storage", "oni://storage/list"),
    ("inventory", "oni://resources/inventory"),
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Dupe {
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub chore: String,
    pub breath: Option<f64>,
    pub stress: Option<f64>,
    pub bladder: Option<f64>,
    pub reachable: Option<i64>,
    pub element: String,
    pub blocked: i64,
    pub schedule: String,
    /// 官方 `step_facts` 的 `workScheduled`：此人是否在**工作日程块**里。
    ///
    /// 为什么用它而不是 chore 文本：日程块和当前动作会不一致（实测 schedule=工作 而 chore=睡觉），
    ///   而两份"休息差事名单"（sense 的 resting_count / stuck 的 is_self_rescuing）互不一致，
    ///   "放松"漏算 → 娱乐时段被当成全员真空闲 → 触发自救。None = 传感器没报（按"在上班"保守处理）。
    #[serde(default)]
    pub work_scheduled: Option<bool>,
}

impl Dupe {
    /// 所在格是否**不可呼吸**（污染水 / CO₂ / 氯气 / 污染氧 / 真空）。
    ///
    /// 为什么需要：三次都发生在"人泡在不可呼吸的格里"。
    ///   代理以前只看得见全局气体比例，看不见**人具体泡在哪** ——
    ///   这一项把人变成一等公民：判定看的是"人所在格"，不是地图平均。
    pub fn in_unbreathable(&self) -> bool {
        matches!(
            self.element.as_str(),
            "污染水" | "DirtyWater" | "二氧化碳" | "CarbonDioxide" | "氯气" | "ChlorineGas"
                | "污染氧" | "ContaminatedOxygen" | "真空" | "Vacuum" | "污水"
        )
    }
    /// 是否处于倒地状态（失去行动能力）—— 不会执行任何差事。
    pub fn is_downed(&self) -> bool {
        self.chore.contains("失去行动")
            || self.chore.contains("倒地")
            || self.chore.contains("Downing")
            || self.chore.contains("Unconscious")
    }
}

/// 传感器健康：这份快照能不能信。
///
/// 为什么需要：mod 对**长连接 session** 的"可见世界扫描"会在游戏长时间暂停后停止更新，
///   而同一响应里的实时字段还在变（实测 14 小时 hold 后 serve 持续 25 分钟看 cycle 1/厕所 0）。
///   光靠定时重连是盲目的；要用**必然递增的心跳**判断数据有没有过期，并让所有层看见。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SensorHealth {
    #[default]
    Ok,
    /// 部分数据源读失败 / 数据源之间不一致：可用，但对应数字不可信（见 health_notes）
    Degraded,
    /// mod 心跳未推进：整份快照可能是缓存，**不要据此决策**（sense 已重连 session）
    Stale,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Snapshot {
    pub ts: i64,
    pub cycle: i64,
    pub paused: bool,
    /// 游戏速度（来自 oni://colony/status 的 gameSpeed）。
    ///
    /// 为什么需要：`isPaused` 只表示"暂停按钮"，而**速度 0 也等于停着**
    ///   （mod 报 gameSpeed=0.0 但 isPaused=false 的组合出现过）。
    ///   判定"世界在不在跑"要同时看这两个：任一为停 → 判停。
    ///   两者来自同一次响应，零额外请求成本。
    pub game_speed: f64,
    pub dupes: Vec<Dupe>,
    pub diagnostics: Value,
    pub atmos: Value,
    pub gas: Value,
    pub rooms: Value,
    pub power: Value,
    pub food: Value,
    pub storage: Value,
    pub inventory: Value,
    /// prefabId → 数量（官方建筑索引）
    pub buildings: Value,
    pub reach_bounds: Option<(i32, i32, i32, i32)>,
    /// **官方可达格数**（reachability 的 reachableCells）。
    ///
    /// 用途：估算**生活区**（复制人真正待着的封闭气室）的空气量 ——
    ///   全局氧气把殖民地根本够不到的气室也算进去 → 空气红线永远看不见。
    pub reachable_cells: usize,
    pub latency_ms: u128,
    pub errors: Vec<String>,
    /// 传感器健康（见 [`SensorHealth`]）
    pub health: SensorHealth,
    /// 健康/降级/过期的具体原因（给大脑和日志看）
    pub health_notes: Vec<String>,
    /// mod 心跳：游戏进程真实运行秒数（`game_control time` 的 realtimeSinceStartup），必然递增
    pub probe: Option<f64>,
    /// 游戏时钟 (cycle, timeOfDayPercent)：**只能前进**；回退 = 读档/回到保存点
    pub clock: Option<(i64, f64)>,
    /// 官方原生空闲人数（step_facts 的 workIdleCount）—— 比"差事名"可靠得多
    pub native_idle: Option<usize>,
    /// 官方待办工单数（dig+build）：>0 说明**有活排队**，不算空跑
    pub pending_orders: Option<i64>,
    /// **施工中**的建筑数（梯子/砖块等在施工队列里）。
    ///
    /// 为什么单独统计：建筑施工**不是**工单（不在 remainingTrackedOrders 里），
    ///   所以"全员空闲 + 无工单"会误判成空跑 → 代理停机 → 梯子永远铺不完。
    ///   梯子在施工时复制人确实闲着，但那是**有意义的等待**，不能停机。
    pub under_construction: i64,
}

impl Default for Snapshot {
    fn default() -> Self {
        Self {
            ts: 0,
            cycle: 0,
            paused: false,
            game_speed: -1.0,
            dupes: Vec::new(),
            diagnostics: Value::Null,
            atmos: Value::Null,
            gas: Value::Null,
            rooms: Value::Null,
            power: Value::Null,
            food: Value::Null,
            storage: Value::Null,
            inventory: Value::Null,
            buildings: Value::Null,
            reach_bounds: None,
            reachable_cells: 0,
            latency_ms: 0,
            errors: Vec::new(),
            health: SensorHealth::Ok,
            health_notes: Vec::new(),
            probe: None,
            clock: None,
            native_idle: None,
            pending_orders: None,
            under_construction: 0,
        }
    }
}

/// 上一次快照的 mod 心跳（进程内；快系统顺序采，CLI 单次读没有基线 → 不判）。
static PROBE_PREV: std::sync::Mutex<Option<(f64, std::time::Instant)>> = std::sync::Mutex::new(None);
/// 上一次快照的游戏时钟 (cycle, timeOfDayPercent)。
static CLOCK_PREV: std::sync::Mutex<Option<(i64, f64)>> = std::sync::Mutex::new(None);

/// 心跳是**整数秒**：两次快照落在同一秒内会读成相等。间隔不足这个数就不判，避免误报 stale。
pub const PROBE_MIN_GAP_SECS: u64 = 3;

/// 从 `game_control time` 文本解析 mod 心跳（realtimeSinceStartup = 游戏进程真实运行秒数）。
pub fn parse_probe(text: &str) -> Option<f64> {
    text.lines()
        .find(|l| l.trim_start().starts_with("realtimeSinceStartup:"))
        .and_then(|l| l.split(':').nth(1))
        .and_then(|v| v.trim().parse::<f64>().ok())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeVerdict {
    /// 没有基线或本次没读到：不判
    NoBaseline,
    /// 心跳推进：数据是活的
    Advancing,
    /// 心跳**不变**：响应是缓存/过期（mod 对旧 session 的扫描停止更新，或传输层缓存）
    Stale,
    /// 心跳**回退**：游戏进程重启了（世界可能已经换了）
    Restarted,
}

/// 心跳判定。为什么用心跳而不是 cycle/时间：cycle 600s 才跳一次、timeOfDay 随暂停停走；
/// `realtimeSinceStartup` 是游戏进程的墙钟，暂停也走、换档也走，只有"响应是旧的"才会不变。
pub fn probe_verdict(prev: Option<f64>, now: Option<f64>, gap_secs: u64) -> ProbeVerdict {
    match (prev, now) {
        (Some(p), Some(n)) if n > p => ProbeVerdict::Advancing,
        // 相等但间隔太短（同一秒内两次读）→ 没法判，不算 stale
        (Some(p), Some(n)) if n == p && gap_secs >= PROBE_MIN_GAP_SECS => ProbeVerdict::Stale,
        (Some(p), Some(n)) if n == p => ProbeVerdict::NoBaseline,
        (Some(_), Some(_)) => ProbeVerdict::Restarted,
        _ => ProbeVerdict::NoBaseline,
    }
}

/// 从 `game_control time` 文本解析游戏时钟 (cycle, timeOfDayPercent)。
pub fn parse_clock(text: &str) -> Option<(i64, f64)> {
    let num = |key: &str| -> Option<f64> {
        text.lines()
            .find(|l| l.trim_start().starts_with(&format!("{key}:")))
            .and_then(|l| l.splitn(2, ':').nth(1))
            .and_then(|v| v.trim().parse::<f64>().ok())
    };
    Some((num("cycle")? as i64, num("timeOfDayPercent")?))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClockVerdict { NoBaseline, Forward, Backward }

/// 游戏时钟只能前进：任何回退（哪怕同一周期内）= 读档/回到保存点，世界被改写了。
pub fn clock_verdict(prev: Option<(i64, f64)>, now: Option<(i64, f64)>) -> ClockVerdict {
    match (prev, now) {
        (Some(p), Some(n)) => if n >= p { ClockVerdict::Forward } else { ClockVerdict::Backward },
        _ => ClockVerdict::NoBaseline,
    }
}

/// 各数据源交叉印证：同一时刻、同一世界的快照，这些数字必须一致。
///   不一致 → 读到了两个时刻/两个世界的数据（换档过渡期 / 部分源缓存），对应字段不可信。
pub fn cross_check(status: &Value, diagnostics: &Value, dupes_len: usize) -> Vec<String> {
    let mut r = Vec::new();
    if let (Some(a), Some(b)) = (
        status.get("cycle").and_then(|v| v.as_i64()),
        diagnostics.get("cycle").and_then(|v| v.as_i64()),
    ) {
        if a != b { r.push(format!("周期不一致：status={a} diagnostics={b}")); }
    }
    if let Some(n) = status.get("duplicantCount").and_then(|v| v.as_i64()) {
        if n as usize != dupes_len { r.push(format!("人口不一致：status={n} 名单={dupes_len}")); }
    }
    r
}

/// 从 `step_facts` 文本里解析每人的 `workScheduled`（name → 是否在工作日程块）。
///
/// 表格式（mod 文档化的固定列，按表头取列而不是数逗号）：
/// `duplicants[3]{name,idle,workScheduled,scheduleGroupId,workScheduleGroupId}:` 后每行
/// `名字,idle,workScheduled,groupId,workGroupId`。解析不到就返回空表（上层按"未知"处理）。
pub fn parse_work_scheduled(facts_text: &str) -> std::collections::HashMap<String, bool> {
    let mut out = std::collections::HashMap::new();
    let mut lines = facts_text.lines().skip_while(|l| !l.contains("duplicants[") || !l.contains('{'));
    let Some(header) = lines.next() else { return out };
    let cols: Vec<&str> = header
        .split('{').nth(1).and_then(|s| s.split('}').next())
        .map(|s| s.split(',').map(str::trim).collect())
        .unwrap_or_default();
    let (Some(ni), Some(wi)) = (cols.iter().position(|c| *c == "name"), cols.iter().position(|c| *c == "workScheduled")) else { return out };
    let width = cols.len();
    for l in lines {
        let f: Vec<&str> = l.split(',').map(str::trim).collect();
        if f.len() < width || f.iter().any(|x| x.is_empty()) { break; }
        if let Some(name) = f.get(ni) {
            out.insert(name.to_string(), f.get(wi).map(|v| *v == "true").unwrap_or(false));
        }
    }
    out
}

impl Snapshot {
    pub fn food_kcal(&self) -> f64 {
        self.food_kcal_opt().unwrap_or(0.0)
    }
    /// 食物读数（None = 传感器没报，**不等于 0**）。
    ///
    /// 为什么要区分：给大脑的事实摘要把"没报"编成 0 → 大脑按假饥荒排优先级
    ///   （实测周期 1 食物 16000kcal 被报成 0）。判死用 0 兜底可以，给大脑看不行。
    pub fn food_kcal_opt(&self) -> Option<f64> {
        // P0-58：inventory 全量 totalCaloriesKcal 求和是权威源。
        //   两次误判"收割无效"就是因为只读 diagnostics.foodKcal 这个派生单点
        //   。权威源优先，派生值只做 fallback。
        let mut total = 0.0;
        let mut found = false;
        if let Some(items) = self.inventory.get("items").and_then(|i| i.as_array()) {
            for it in items {
                if let Some(kcal) = it.get("totalCaloriesKcal").and_then(|v| v.as_f64()) {
                    total += kcal;
                    found = true;
                }
            }
        }
        if found {
            return Some(total);
        }
        self.diagnostics.get("foodKcal").and_then(|v| v.as_f64())
    }
    pub fn oxygen_kg(&self) -> f64 {
        self.oxygen_kg_opt().unwrap_or(0.0)
    }
    /// 氧气读数（None = 传感器没报，不等于 0；见 [`food_kcal_opt`](Self::food_kcal_opt)）。
    pub fn oxygen_kg_opt(&self) -> Option<f64> {
        self.diagnostics.get("oxygenKgVisible").and_then(|v| v.as_f64())
    }
    pub fn max_stress(&self) -> f64 {
        self.diagnostics.get("maxStress").and_then(|v| v.as_f64()).unwrap_or(0.0)
    }
    /// **电力摘要**：`oni://power/summary` 的真实结构是
    /// `{circuits[], diagnostics, summary}` —— 计数都在 `summary` 里。
    /// 之前读顶层 `generatorOperational` 永远是空 → "发电机没燃料"这条告警
    /// **从来没触发过**（真 bug）。
    pub fn power_summary(&self) -> &Value {
        self.power.get("summary").unwrap_or(&self.power)
    }
    pub fn power_num(&self, key: &str) -> f64 {
        self.power_summary().get(key).and_then(|v| v.as_f64()).unwrap_or(0.0)
    }
    pub fn power_int(&self, key: &str) -> i64 {
        self.power_summary().get(key).and_then(|v| v.as_i64()).unwrap_or(0)
    }
    /// 地上的散装资源质量（按元素 ID）：建造前先确认材料够不够。
    pub fn loose_kg(&self, element: &str) -> f64 {
        self.inventory
            .get("items")
            .and_then(|i| i.as_array())
            .map(|a| {
                a.iter()
                    .filter(|it| it.get("elementId").and_then(|v| v.as_str()) == Some(element))
                    .filter_map(|it| it.get("totalMassKg").and_then(|m| m.as_f64()))
                    .sum()
            })
            .unwrap_or(0.0)
    }
    // ---- 大气质量（`oni://atmosphere/breathability`）----
    pub fn atmos_str(&self, key: &str) -> String {
        self.atmos.get(key).and_then(|v| v.as_str()).unwrap_or("").to_string()
    }
    pub fn atmos_num(&self, key: &str) -> f64 {
        self.atmos.get(key).and_then(|v| v.as_f64()).unwrap_or(0.0)
    }
    /// 窒息风险等级：nominal / poor / critical
    pub fn suffocation_risk(&self) -> String {
        self.atmos_str("suffocationRisk")
    }
    /// 最高的膀胱百分比。
    ///
    /// 为什么要它：没有厕所时膀胱满 → 复制人随地排泄
    ///   → 地面生成**污染水** → 三人泡在污染水里→ 差点重演/9。
    ///   所以膀胱是"污染"的**一级预警信号**，必须进调度。
    pub fn max_bladder(&self) -> f64 {
        self.dupes
            .iter()
            .filter_map(|d| d.bladder)
            .fold(0.0f64, f64::max)
    }

    /// 排除"正在如厕"者的最高膀胱。
    ///
    /// 为什么需要：膀胱 100% 的复制人往往**正是正在如厕的那个**
    ///（chore="休息时间：如厕"/"使用厕所"）—— 拿他当告警源会每轮误报，
    /// 反复复活建厕所节点、抢占真正该做的采食物/换气。
    /// 真危机是"有人膀胱满且**没在如厕**、且没有厕所可用"。
    pub fn max_active_bladder(&self) -> f64 {
        self.dupes
            .iter()
            .filter(|d| {
                let c = d.chore.as_str();
                !c.contains("如厕") && !c.contains("厕所")
            })
            .filter_map(|d| d.bladder)
            .fold(0.0f64, f64::max)
    }

    pub fn air_quality(&self) -> String {
        self.atmos_str("airQuality")
    }
    pub fn co2_kg(&self) -> f64 {
        self.atmos_num("carbonDioxideMassKg")
    }
    /// CO₂ 与氧气的质量比：> 1 说明**全局**在中毒（不只是某个坑）
    pub fn co2_ratio(&self) -> f64 {
        let o = self.atmos_num("oxygenMassKg");
        let c = self.co2_kg();
        if o + c <= 0.0 { 0.0 } else { c / o }
    }
    /// 按 prefabId 统计建筑数量（来自官方建筑索引）。
    /// 计划里要用"梯子≥1 / 砖块≥N"这种判据时靠它。
    pub fn building_count(&self, prefab: &str) -> i64 {
        self.buildings
            .get(prefab)
            .and_then(|v| v.as_i64())
            .unwrap_or(0)
    }
    /// **可呼吸气体比例** = 氧气 / (氧气 + CO₂)。
    ///
    /// 为什么用比例而不是绝对值：密闭空间里氧气总量可以还很可观，
    ///   但只要 CO₂ 占比高，复制人照样窒息。
    pub fn breath_ratio(&self) -> f64 {
        let o = self.atmos_num("oxygenMassKg");
        let c = self.co2_kg();
        if o + c <= 0.0 {
            return 1.0;
        }
        o / (o + c)
    }
    /// 可呼吸格占可见格的比例
    pub fn breathable_ratio(&self) -> f64 {
        let b = self.atmos_num("breathableCells");
        let t = self.atmos_num("totalVisibleCells");
        if t <= 0.0 {
            return 1.0;
        }
        b / t
    }
    /// **进展指纹**：把"殖民地有没有变好"压成一个可比较的串。
    ///
    /// 为什么需要：上一轮代理连续几十轮重复同一条无效命令
    ///   （挖了但材料不涨、工单被 ONI 取消），看起来"在跑"其实零进展。
    ///   有指纹才能识别"卡住"，进而熔断换方案。
    pub fn progress_fingerprint(&self) -> String {
        let mut mat = 0.0;
        if let Some(items) = self.inventory.get("items").and_then(|i| i.as_array()) {
            for it in items {
                mat += it.get("totalMassKg").and_then(|m| m.as_f64()).unwrap_or(0.0);
            }
        }
        let buildings: i64 = self
            .buildings
            .as_object()
            .map(|m| m.values().filter_map(|v| v.as_i64()).sum())
            .unwrap_or(0);
        // 只保留"真的变好"的信号：氧气质量每轮都在波动，放进指纹会让
        // 熔断永远不触发（这正是上一轮"卡住却一直重复"的机制原因）。
        format!(
            "mat={:.0}|bld={}|pop={}|air={:.2}",
            mat,
            buildings,
            self.dupes.len(),
            (self.breath_ratio() * 20.0).round() / 20.0
        )
    }
    /// **生活区（复制人真正待着的封闭气室）的氧气量**估算。
    ///
    /// 为什么必须有这个指标：
    ///   全局氧气 394kg、比例 99% —— 看着非常安全；
    ///   但生活区是个**封闭气室**。
    ///   全局指标把「殖民地根本够不到的气室」也算进去 → 空气红线**永远看不见**。
    ///   上一局就是这么死的：全局比例正常，人在封闭区里窒息到 0。
    ///   口径：全局氧气密度（oxygenKgVisible / 可呼吸格）× 官方可达格数。
    pub fn living_oxygen_kg(&self) -> Option<f64> {
        let cells = self.reachable_cells;
        if cells == 0 {
            return None;
        }
        let breath_cells = self
            .diagnostics
            .get("breathableCellsVisible")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        if breath_cells <= 0.0 {
            return None;
        }
        let density = self.oxygen_kg() / breath_cells;
        Some(density * cells as f64)
    }

    pub fn building(&self, key: &str) -> i64 {
        self.diagnostics
            .get("buildings")
            .and_then(|b| b.get(key))
            .and_then(|v| v.as_i64())
            .unwrap_or(0)
    }
    pub fn idle_count(&self) -> usize {
        // 官方原生判定优先：差事名会把"已预约工单但还在走"的人算成空闲
        //   误判成全员空闲 → 守护进程反复停机 → 反而在空跑。
        if let Some(n) = self.native_idle {
            return n;
        }
        self.dupes.iter().filter(|d| d.chore == "空闲" || d.chore == "IdleChore").count()
    }
    /// **不在工作日程**的人数（睡觉 / 娱乐 / 卫生时段；不算"真的没活干"）。
    ///
    /// 为什么需要：
    ///   官方的 workIdleCount 语义是"不在工作日程里" → **睡觉的复制人也算 idle**。
    ///   于是 observe 常年显示"全员空闲 3/3"，看起来像代理卡住，其实他们只是在睡觉。
    ///   这个误判会让守护进程误停机（空跑）或误判无进展（熔断好节点）。
    /// 口径：官方 `step_facts.workScheduled == false`（见 [`parse_work_scheduled`]），
    ///   不用 chore 文本匹配 —— 旧实现漏了"放松"，且和 stuck.rs 的名单不一致。
    pub fn resting_count(&self) -> usize {
        self.dupes.iter().filter(|d| d.work_scheduled == Some(false)).count()
    }
    /// **有没有人在工作日程**。没人上班时工单没人执行是正常的，不算"零进展"。
    ///   传感器没报（全是 None）→ 按"有"保守处理（维持计数，不放宽）。
    pub fn anyone_on_duty(&self) -> bool {
        self.dupes.is_empty() || self.dupes.iter().any(|d| d.work_scheduled != Some(false))
    }
    /// 给大脑看的日程事实：`工作 1 / 非工作 2`（未知单列）；全员不在工作时段时明说"不是卡住"。
    pub fn schedule_summary(&self) -> String {
        let on = self.dupes.iter().filter(|d| d.work_scheduled == Some(true)).count();
        let off = self.resting_count();
        let unknown = self.dupes.len() - on - off;
        let mut s = format!("工作 {on} / 非工作 {off}");
        if unknown > 0 { s.push_str(&format!(" / 未知 {unknown}")); }
        if on == 0 && off > 0 {
            s.push_str(" → 全员不在工作时段（睡觉/娱乐）：工单要等开工才执行，不是卡住；不要因此重排窗口或换目标");
        }
        s
    }
    /// **真的没活干**的人数：在 idle 里排除掉正在休息的。
    ///
    /// 这是"代理是不是空转"的正确判据：
    ///   全员睡觉 ≠ 全员闲着没事干（他们醒来会干活）；只有真空闲才说明没工单可做。
    pub fn truly_idle_count(&self) -> usize {
        let idle = self.idle_count();
        let resting = self.resting_count();
        idle.saturating_sub(resting)
    }
    /// 是否有复制人**倒地/失去行动能力**。
    ///
    /// 为什么必须单独识别：
    ///   倒地的复制人**不会去执行任何差事** —— 代理还在下挖掘工单、以为工单会被完成，
    ///   结果三个人在 CO₂ 泡到死。看见倒地就必须立刻转入救援（排毒/拖到有空气处/治疗）。
    pub fn downed_count(&self) -> usize {
        self.dupes
            .iter()
            .filter(|d| {
                d.chore.contains("失去行动")
                    || d.chore.contains("倒地")
                    || d.chore.contains("Downing")
                    || d.chore.contains("Unconscious")
            })
            .count()
    }
    /// 是否有人倒地（需要救援）。
    pub fn has_downed(&self) -> bool {
        self.downed_count() > 0
    }
    pub fn min_breath(&self) -> Option<f64> {
        self.dupes.iter().filter_map(|d| d.breath).fold(None, |a: Option<f64>, b| Some(a.map_or(b, |x| x.min(b))))
    }
    pub fn alerts(&self) -> Vec<String> {
        self.diagnostics
            .get("alerts")
            .and_then(|a| a.as_array())
            .map(|arr| {
                arr.iter()
                    .map(|a| format!("[{}] {}", a.get("severity").and_then(|v| v.as_str()).unwrap_or("?"), a.get("message").and_then(|v| v.as_str()).unwrap_or("")))
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// 拉取全局面。资源并行（rayon），任一源失败只记 error 不中断——
/// **局部失败不能变成"看起来正常"**。
pub fn sense(session: &McpSession) -> Snapshot {
    let t0 = Instant::now();
    let mut errors: Vec<String> = Vec::new();

    // 官方原生空闲事实：step_facts 的 workIdleCount（不恢复游戏，只读事实）
    // 官方建筑索引 → prefabId 计数（计划的"梯子/砖块数量"判据靠它）
    let buildings: Value = session
        .call_text(
            "world_editor",
            serde_json::json!({
                "command": "read", "path": "/active/buildings/index.md",
                "confirm": true, "task": "循环: 官方建筑索引"
            }),
        )
        .ok()
        .map(|txt| {
            let mut map = serde_json::Map::new();
            for line in txt.lines() {
                let cols: Vec<&str> = line.split('|').map(|s| s.trim()).collect();
                if cols.len() < 4 || cols[2].is_empty() {
                    continue;
                }
                // 跳过表头/分隔行：
                //   表头 `| Building | Prefab | ID | Position | ... |` 的 cols[2]="Prefab"
                //   → 被当成一个叫 "Prefab" 的建筑计入 → 计数污染，
                //   observe 还会显示成 "建筑 Prefab 4"。
                if cols[2] == "Prefab" || cols[2].chars().all(|c| c == '-') {
                    continue;
                }
                let e = map.entry(cols[2].to_string()).or_insert(Value::from(0));
                let n = e.as_i64().unwrap_or(0) + 1;
                *e = Value::from(n);
            }
            Value::Object(map)
        })
        .unwrap_or(Value::Null);

    // 施工中的建筑：从地图 token 里找 `...UnderConstruction:优先级#材料@(x,y)`
    let under_construction: i64 = session
        .call_text(
            "world_editor",
            serde_json::json!({
                "command": "read", "path": "/active/map/viewport.md",
                "format": "edit", "compact": false,
                "confirm": true, "task": "循环: 施工中建筑"
            }),
        )
        .ok()
        .map(|txt| {
            txt.lines()
                .filter(|l| l.contains("UnderConstruction"))
                .flat_map(|l| l.split_whitespace())
                .filter(|t| t.contains("UnderConstruction"))
                .count() as i64
        })
        .unwrap_or(0);

    // ★ mod 心跳：两次快照之间 realtimeSinceStartup 必须推进；不变 = 响应是缓存（过期）→ 标 stale
    //   并立即重连 session；回退 = 游戏进程重启 → 重连，等 saveswitch 重新识别存档。
    let time_text = session
        .call_text("game_control", serde_json::json!({
            "domain": "speed", "action": "time", "task": "SENSE: mod 心跳 + 游戏时钟" }))
        .ok();
    let probe = time_text.as_deref().and_then(parse_probe);
    // ★ 游戏时钟连续性：回退 = 读档（哪怕周期只退 1 或同一周期内）→ 世界被改写，重连 session 并标注。
    let clock = time_text.as_deref().and_then(parse_clock);
    let clock_prev = CLOCK_PREV.lock().ok().and_then(|g| *g);
    let clock_rewind = clock_verdict(clock_prev, clock) == ClockVerdict::Backward;
    if let Ok(mut g) = CLOCK_PREV.lock() { *g = clock; }
    let (probe_prev, probe_prev_at) = PROBE_PREV.lock().ok().map(|g| (g.map(|x| x.0), g.map(|x| x.1))).unwrap_or((None, None));
    let gap = probe_prev_at.map(|t| t.elapsed().as_secs()).unwrap_or(0);
    let verdict = probe_verdict(probe_prev, probe, gap);
    let mut health = SensorHealth::Ok;
    let mut health_notes: Vec<String> = Vec::new();
    match verdict {
        ProbeVerdict::Advancing | ProbeVerdict::NoBaseline => {}
        ProbeVerdict::Stale => {
            health = SensorHealth::Stale;
            health_notes.push("mod 心跳未推进（realtimeSinceStartup 不变）→ 响应可能是缓存，数据不可信".into());
            if session.refresh().is_ok() { health_notes.push("已重连 session，下一拍复核".into()); }
        }
        ProbeVerdict::Restarted => {
            health_notes.push("游戏进程重启（心跳回退）→ 已重连 session，等待重新识别存档".into());
            let _ = session.refresh();
        }
    }
    if clock_rewind {
        health_notes.push(format!(
            "检测到时钟回退（读档/回到保存点）：周期 {} {}% → {} {}% → 世界被改写；已重连 session，近期决策/工单可能已失效",
            clock_prev.map(|c| c.0).unwrap_or(0), clock_prev.map(|c| c.1 as i64).unwrap_or(0),
            clock.map(|c| c.0).unwrap_or(0), clock.map(|c| c.1 as i64).unwrap_or(0)));
        let _ = session.refresh();
    }
    if !matches!(verdict, ProbeVerdict::Stale) {
        if let Ok(mut g) = PROBE_PREV.lock() { *g = probe.map(|p| (p, std::time::Instant::now())); }
    }

    // 待办工单数：与 native_idle 同一个 step_facts 响应里读
    let facts_text = session
        .call_text(
            "game_control",
            serde_json::json!({ "domain": "speed", "action": "step_facts",
                               "task": "循环: 读取原生空闲/待办工单事实" }),
        )
        .ok();
    let native_idle = facts_text.as_ref().and_then(|txt| {
        txt.lines()
            .find(|l| l.trim_start().starts_with("workIdleCount:"))
            .and_then(|l| l.split(':').nth(1))
            .and_then(|v| v.trim().parse::<usize>().ok())
    });
    let pending_orders = facts_text.as_ref().and_then(|txt| {
        txt.lines()
            .find(|l| l.trim_start().starts_with("remainingTrackedOrders:"))
            .and_then(|l| l.split(':').nth(1))
            .and_then(|v| v.trim().parse::<i64>().ok())
    });
    // 删掉了重复的 step_facts 调用（原来调了两次，白花一次 MCP 往返）。

    let mut raw: BTreeMap<&str, Value> = BTreeMap::new();
    let results: Vec<(&str, &str, Result<Value, String>)> = SOURCES
        .par_iter()
        .map(|(name, uri)| {
            let r = session.read_json(uri).map_err(|e| e.to_string());
            (*name, *uri, r)
        })
        .collect();
    for (name, uri, r) in results {
        match r {
            Ok(v) => {
                raw.insert(name, v);
            }
            Err(e) => errors.push(format!("{uri}: {e}")),
        }
    }

    // 可达包络（虚拟文件）
    // 同一个文件里顺便取「可达格数」→ 用来估算生活区空气量（不额外加 MCP 压力）
    let reach_text = session
        .call_text(
            "world_editor",
            serde_json::json!({ "command": "read", "path": "/active/dupes/reachability.md", "confirm": true, "task": "SENSE: 可达包络" }),
        )
        .ok();
    // 可达格数（官方 reachableCells，取第一处 = 全体并集）
    let reachable_cells = reach_text
        .as_deref()
        .and_then(|t| {
            let i = t.find("reachableCells")?;
            let rest = t[i + "reachableCells".len()..].trim_start();
            let rest = rest.strip_prefix(':')?.trim_start();
            let end = rest.find(|c: char| !c.is_ascii_digit())?;
            rest[..end].trim().parse::<usize>().ok()
        })
        .unwrap_or(0);
    let reach_bounds = reach_text
        .as_deref()
        .and_then(|t| {
            let i = t.find("\"bounds\":[")?;
            let rest = &t[i + 10..];
            let end = rest.find(']')?;
            let v: Vec<i32> = rest[..end]
                .split(',')
                .filter_map(|s| s.trim().parse().ok())
                .collect();
            if v.len() == 4 {
                Some((v[0], v[1], v[2], v[3]))
            } else {
                None
            }
        });

    let get = |k: &str| raw.get(k).cloned().unwrap_or(Value::Null);
    let diagnostics = get("diagnostics");
    let status = get("status");
    let todos = get("todos");
    let status_check = get("status_check");

    // 合并 todos（差事/位置）与 status-check（体征/可达格）
    let mut by_id: BTreeMap<i64, &Value> = BTreeMap::new();
    if let Some(items) = status_check.get("items").and_then(|v| v.as_array()) {
        for it in items {
            if let Some(id) = it.pointer("/dupe/id").and_then(|v| v.as_i64()) {
                by_id.insert(id, it);
            }
        }
    }
    // 官方"是否在工作日程块"：与 native_idle / pending_orders 同一个 step_facts 响应，零额外请求
    let work_scheduled = facts_text.as_deref().map(parse_work_scheduled).unwrap_or_default();
    let mut dupes = Vec::new();
    if let Some(arr) = todos.get("dupes").and_then(|v| v.as_array()) {
        for d in arr {
            let id = d.get("id").and_then(|v| v.as_i64()).unwrap_or(0);
            let sc = by_id.get(&id).copied();
            let ws = d.get("name").and_then(|v| v.as_str()).and_then(|n| work_scheduled.get(n)).copied();
            let needs = sc.and_then(|v| v.get("needs")).cloned().unwrap_or(Value::Null);
            let nav = sc.and_then(|v| v.get("navigation")).cloned().unwrap_or(Value::Null);
            let env = sc.and_then(|v| v.get("environment")).cloned().unwrap_or(Value::Null);
            dupes.push(Dupe {
                name: d.get("name").and_then(|v| v.as_str()).unwrap_or("?").to_string(),
                x: d.get("x").and_then(|v| v.as_i64()).unwrap_or(0) as i32,
                y: d.get("y").and_then(|v| v.as_i64()).unwrap_or(0) as i32,
                chore: d.pointer("/current/name").and_then(|v| v.as_str()).unwrap_or("?").to_string(),
                breath: needs.get("breath").and_then(|v| v.as_f64()),
                stress: needs.get("stress").and_then(|v| v.as_f64()),
                bladder: needs.get("bladder").and_then(|v| v.as_f64()),
                reachable: nav.get("reachableCells").and_then(|v| v.as_i64()),
                element: env
                    .get("elementName")
                    .and_then(|v| v.as_str())
                    .or_else(|| env.get("element").and_then(|v| v.as_str()))
                    .unwrap_or("?")
                    .to_string(),
                blocked: d.pointer("/counts/blocked").and_then(|v| v.as_i64()).unwrap_or(0),
                schedule: d.get("currentScheduleBlock").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                work_scheduled: ws,
            });
        }
    }

    // 交叉印证 + 读失败 → 降级（stale 更严重，不被降级覆盖）
    for r in cross_check(&status, &diagnostics, dupes.len()) { health_notes.push(r); }
    if !health_notes.is_empty() && health != SensorHealth::Stale { health = SensorHealth::Degraded; }
    if !errors.is_empty() && health != SensorHealth::Stale { health = SensorHealth::Degraded; }
    if health == SensorHealth::Stale {
        eprintln!("[sensor] ⚠ 快照可能过期：{}", health_notes.join("；"));
    }

    Snapshot {
        health,
        health_notes,
        probe,
        clock,
        ts: chrono::Local::now().timestamp(),
        cycle: diagnostics.get("cycle").and_then(|v| v.as_i64()).unwrap_or(0),
        paused: status.get("isPaused").and_then(|v| v.as_bool()).unwrap_or(false),
        game_speed: status.get("gameSpeed").and_then(|v| v.as_f64()).unwrap_or(-1.0),
        dupes,
        diagnostics,
        atmos: get("atmos"),
        gas: get("gas"),
        rooms: get("rooms"),
        power: get("power"),
        food: get("food"),
        storage: get("storage"),
        inventory: get("inventory"),
        buildings,
        reach_bounds,
        reachable_cells,
        latency_ms: t0.elapsed().as_millis(),
        native_idle,
        pending_orders,
        under_construction,
        errors,
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    fn dupe(chore: &str, bladder: f64) -> Dupe {
        serde_json::from_value(serde_json::json!({
            "name": "t", "x": 0, "y": 0, "chore": chore,
            "bladder": bladder, "element": "Oxygen", "schedule": "", "blocked": 0
        })).unwrap()
    }

    fn ws(chore: &str, work: Option<bool>) -> Dupe {
        let mut v = serde_json::json!({"name": "t", "x": 0, "y": 0, "chore": chore, "bladder": 50.0,
            "element": "Oxygen", "schedule": "", "blocked": 0});
        if let Some(w) = work { v["work_scheduled"] = serde_json::json!(w); }
        serde_json::from_value(v).unwrap()
    }

    /// mod 心跳判定：不变 = 过期缓存；回退 = 游戏重启；推进 = 活的；没基线不判。
    ///   锁住的不变量：只有"心跳不变"才判 stale —— 暂停/换档时心跳照样推进，不会误判。
    #[test]
    fn probe_verdict_only_stale_when_heartbeat_does_not_advance() {
        assert_eq!(probe_verdict(None, Some(96000.0), 10), ProbeVerdict::NoBaseline);
        assert_eq!(probe_verdict(Some(96000.0), None, 10), ProbeVerdict::NoBaseline);
        assert_eq!(probe_verdict(Some(96000.0), Some(96010.5), 10), ProbeVerdict::Advancing);
        assert_eq!(probe_verdict(Some(96000.0), Some(96000.0), 10), ProbeVerdict::Stale, "两次读相同 = 响应是缓存");
        assert_eq!(probe_verdict(Some(96000.0), Some(96000.0), 1), ProbeVerdict::NoBaseline, "同一秒内两次读 → 不判");
        assert_eq!(probe_verdict(Some(96000.0), Some(12.0), 10), ProbeVerdict::Restarted, "回退 = 游戏进程重启");
        assert_eq!(parse_probe("cycle: 2\ntimeOfDayPercent: 17.3\nrealtimeSinceStartup: 96132\n"), Some(96132.0));
        assert_eq!(parse_probe("nothing"), None);
    }

    /// 游戏时钟只能前进：回退（哪怕同一周期内）= 读档。
    #[test]
    fn clock_verdict_flags_rewind() {
        assert_eq!(parse_clock("cycle: 4\ntimeOfDayPercent: 69.6\nrealtimeSinceStartup: 98784\n"), Some((4, 69.6)));
        assert_eq!(clock_verdict(None, Some((4, 69.6))), ClockVerdict::NoBaseline);
        assert_eq!(clock_verdict(Some((4, 69.6)), Some((4, 70.0))), ClockVerdict::Forward);
        assert_eq!(clock_verdict(Some((4, 99.0)), Some((5, 0.1))), ClockVerdict::Forward, "跨周期");
        assert_eq!(clock_verdict(Some((4, 69.6)), Some((4, 10.0))), ClockVerdict::Backward, "同周期内回退 = 读回保存点");
        assert_eq!(clock_verdict(Some((4, 10.0)), Some((3, 90.0))), ClockVerdict::Backward, "周期退 1");
    }

    /// 数据源交叉印证：同一快照里周期/人口必须一致，否则说明读到了两个时刻/两个世界。
    #[test]
    fn cross_check_flags_inconsistent_sources() {
        let ok = cross_check(
            &serde_json::json!({"cycle": 4, "duplicantCount": 3}),
            &serde_json::json!({"cycle": 4}), 3);
        assert!(ok.is_empty(), "{ok:?}");
        let bad = cross_check(
            &serde_json::json!({"cycle": 4, "duplicantCount": 3}),
            &serde_json::json!({"cycle": 1}), 1);
        assert_eq!(bad.len(), 2, "{bad:?}");
        assert!(bad[0].contains("周期不一致") && bad[1].contains("人口不一致"), "{bad:?}");
    }

    /// step_facts 的 duplicants 表按**表头列名**解析，不数逗号、不碰词表。
    #[test]
    fn parse_work_scheduled_reads_official_table() {
        let txt = "worldId: 0\nisPaused: false\nidleFacts:\n  count: 3\n  workIdleCount: 3\n  duplicants[3]{name,idle,workScheduled,scheduleGroupId,workScheduleGroupId}:\n    尼古拉,true,false,Sleep,Worktime\n    麦克斯,true,false,Sleep,Worktime\n    裴,false,true,Sleep,Worktime\npendingOrders:\n  kinds[2]: dig\n";
        let m = parse_work_scheduled(txt);
        assert_eq!(m.get("尼古拉"), Some(&false));
        assert_eq!(m.get("裴"), Some(&true));
        assert_eq!(m.len(), 3, "{m:?}");
        assert!(parse_work_scheduled("nothing here").is_empty(), "没有表 → 空（上层按未知）");
    }

    /// 上不上班只看官方 workScheduled，不看 chore 文本：
    ///   实测 schedule=工作 而 chore=睡觉；"放松"曾漏算 → 娱乐时段被当成全员真空闲 → 触发自救。
    #[test]
    fn resting_count_uses_official_work_scheduled_not_chore_text() {
        let snap = Snapshot { dupes: vec![ws("放松", Some(true)), ws("睡觉", Some(false)), ws("空闲", None)], ..Default::default() };
        assert_eq!(snap.resting_count(), 1, "只有官方说不在工作日程的那位");
        assert!(snap.anyone_on_duty(), "有人在工作块 + 有人未知 → 算有人上班");
        let asleep = Snapshot { dupes: vec![ws("睡觉", Some(false)), ws("放松", Some(false))], ..Default::default() };
        assert!(!asleep.anyone_on_duty(), "全员不在工作时段");
        let unknown = Snapshot { dupes: vec![ws("x", None)], ..Default::default() };
        assert!(unknown.anyone_on_duty(), "未知 → 保守算在上班");
        assert_eq!(Snapshot::default().anyone_on_duty(), true, "没人口数据 → 保守算有人");
    }

    /// 给大脑的日程事实：全员不在工作时段时必须明说"不是卡住"。
    #[test]
    fn schedule_summary_says_all_off_duty() {
        let asleep = Snapshot { dupes: vec![ws("睡觉", Some(false)), ws("放松", Some(false))], ..Default::default() };
        let s = asleep.schedule_summary();
        assert!(s.starts_with("工作 0 / 非工作 2"), "{s}");
        assert!(s.contains("不是卡住"), "{s}");
        let mixed = Snapshot { dupes: vec![ws("空闲", Some(true)), ws("睡觉", Some(false)), ws("?", None)], ..Default::default() };
        assert_eq!(mixed.schedule_summary(), "工作 1 / 非工作 1 / 未知 1");
    }

    /// 膀胱告警必须排除"正在如厕"者 — 膀胱100往往正是如厕中的表现。
    #[test]
    fn max_active_bladder_excludes_toileting() {
        let snap = Snapshot {
            dupes: vec![
                dupe("休息时间：如厕", 100.0),
                dupe("放松", 2.0),
            ],
            ..Default::default()
        };
        assert_eq!(snap.max_bladder(), 100.0, "原始最大值仍含如厕者");
        assert_eq!(snap.max_active_bladder(), 2.0, "活动最大值应排除如厕者");
    }
}
