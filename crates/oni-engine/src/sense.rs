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
}

impl Dupe {
    /// 所在格是否**不可呼吸**（污染水 / CO₂ / 氯气 / 污染氧 / 真空）。
    ///
    /// ★ 为什么需要（2026-11-21 复盘）：三次全灭都发生在"人泡在不可呼吸的格里"。
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Snapshot {
    pub ts: i64,
    pub cycle: i64,
    pub paused: bool,
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
    /// ★ 用途：估算**生活区**（复制人真正待着的封闭气室）的空气量 ——
    ///   全局氧气把殖民地根本够不到的气室也算进去 → 空气红线永远看不见。
    pub reachable_cells: usize,
    pub latency_ms: u128,
    pub errors: Vec<String>,
    /// 官方原生空闲人数（step_facts 的 workIdleCount）—— 比"差事名"可靠得多
    pub native_idle: Option<usize>,
    /// 官方待办工单数（dig+build）：>0 说明**有活排队**，不算空跑
    pub pending_orders: Option<i64>,
    /// **施工中**的建筑数（梯子/砖块等在施工队列里）。
    ///
    /// ★ 为什么单独统计：建筑施工**不是**工单（不在 remainingTrackedOrders 里），
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
            native_idle: None,
            pending_orders: None,
            under_construction: 0,
        }
    }
}

impl Snapshot {
    pub fn food_kcal(&self) -> f64 {
        // ★ P0-58：inventory 全量 totalCaloriesKcal 求和是权威源。
        //   本局两次误判"收割无效"就是因为只读 diagnostics.foodKcal 这个派生单点
        //   （淤泥根 800→1600 时它仍显示旧值）。权威源优先，派生值只做 fallback。
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
            return total;
        }
        self.diagnostics.get("foodKcal").and_then(|v| v.as_f64()).unwrap_or(0.0)
    }
    pub fn oxygen_kg(&self) -> f64 {
        self.diagnostics
            .get("oxygenKgVisible")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0)
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
    /// 最高的膀胱百分比（0~100）。
    ///
    /// ★ 为什么要它（2026-11-21 实测根因链）：没有厕所时膀胱满 → 复制人随地排泄
    ///   → 地面生成**污染水** → 三人泡在污染水里（呼吸 87.5）→ 差点重演事故 7/9。
    ///   所以膀胱是"污染"的**一级预警信号**，必须进调度。
    pub fn max_bladder(&self) -> f64 {
        self.dupes
            .iter()
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
    /// ★ 为什么用比例而不是绝对值：密闭空间里氧气总量可以还很可观，
    ///   但只要 CO₂ 占比高，复制人照样窒息（开局就是这么全灭的）。
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
    /// ★ 为什么需要：上一轮代理连续几十轮重复同一条无效命令
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
    /// ★★ 为什么必须有这个指标（2026-11-22 陨石坑局复盘）：
    ///   全局氧气 394kg、比例 99% —— 看着非常安全；
    ///   但生活区是个**封闭气室**（开局只有十几格、~20kg 氧气）。
    ///   全局指标把「殖民地根本够不到的气室」也算进去 → 空气红线**永远看不见**。
    ///   上一局（小星2）就是这么死的：全局比例正常，人在封闭区里窒息到 0。
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
        // ★ 官方原生判定优先：差事名会把"已预约工单但还在走"的人算成空闲 →
        //   误判成全员空闲 → 守护进程反复停机 → 反而在空跑。
        if let Some(n) = self.native_idle {
            return n;
        }
        self.dupes.iter().filter(|d| d.chore == "空闲" || d.chore == "IdleChore").count()
    }
    /// **在休息/睡觉**的人数（不算"真的没活干"）。
    ///
    /// ★ 为什么需要（2026-11-21 优化 P1-8）：
    ///   官方的 workIdleCount 语义是"不在工作日程里" → **睡觉的复制人也算 idle**。
    ///   于是 observe 常年显示"全员空闲 3/3"，看起来像代理卡住，其实他们只是在睡觉。
    ///   这个误判会让守护进程误停机（空跑）或误判无进展（熔断好节点）。
    pub fn resting_count(&self) -> usize {
        self.dupes
            .iter()
            .filter(|d| {
                let c = d.chore.as_str();
                c.contains("休息")
                    || c.contains("睡觉")
                    || c.contains("睡眠")
                    || c.contains("Resting")
                    || c.contains("Sleeping")
                    || c.contains("Waking")
                    || c.contains("唤醒")
            })
            .count()
    }
    /// **真的没活干**的人数：在 idle 里排除掉正在休息的。
    ///
    /// ★ 这是"代理是不是空转"的正确判据：
    ///   全员睡觉 ≠ 全员闲着没事干（他们醒来会干活）；只有真空闲才说明没工单可做。
    pub fn truly_idle_count(&self) -> usize {
        let idle = self.idle_count();
        let resting = self.resting_count();
        idle.saturating_sub(resting)
    }
    /// 是否有复制人**倒地/失去行动能力**。
    ///
    /// ★ 为什么必须单独识别（2026-11-21 致命事故）：
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

    // ★ 官方原生空闲事实：step_facts 的 workIdleCount（不恢复游戏，只读事实）
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
                // ★ 跳过表头/分隔行（2026-11-21 实测 bug）：
                //   表头 `| Building | Prefab | ID | Position | ... |` 的 cols[2]="Prefab"
                //   → 被当成一个叫 "Prefab" 的建筑计入 → 计数污染，
                //   observe 还会显示成 "建筑 Prefab 4"（看起来像建了 4 个）。
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
    // ★ 删掉了重复的 step_facts 调用（原来调了两次，白花一次 MCP 往返）。

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
    // ★ 同一个文件里顺便取「可达格数」→ 用来估算生活区空气量（不额外加 MCP 压力）
    let reach_text = session
        .call_text(
            "world_editor",
            serde_json::json!({ "command": "read", "path": "/active/dupes/reachability.md", "confirm": true, "task": "SENSE: 可达包络" }),
        )
        .ok();
    // ★ 可达格数（官方 reachableCells，取第一处 = 全体并集）
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
    let mut dupes = Vec::new();
    if let Some(arr) = todos.get("dupes").and_then(|v| v.as_array()) {
        for d in arr {
            let id = d.get("id").and_then(|v| v.as_i64()).unwrap_or(0);
            let sc = by_id.get(&id).copied();
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
            });
        }
    }

    Snapshot {
        ts: chrono::Local::now().timestamp(),
        cycle: diagnostics.get("cycle").and_then(|v| v.as_i64()).unwrap_or(0),
        paused: status.get("isPaused").and_then(|v| v.as_bool()).unwrap_or(false),
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
