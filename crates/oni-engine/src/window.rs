//! WINDOW —— 滚动窗口规划器（v3 架构第二层，MPC 式）。
//!
//! 为什么不用静态全局 DAG：ONI 是**部分可观察 + 随机**的世界——挖开一块石头才知道后面是 CO₂ 还是水。
//!   静态长链把"现在的判断"固化成"未来的约束"，世界一变就成枷锁（上游判据一坏，整条 requires 链锁死）。
//!
//! 滚动窗口的做法（和自动驾驶的轨迹规划一样）：
//!   - LLM 每次只规划 **horizon = 3 步**（短视）
//!   - 每步带 pre/post 条件
//!   - **世界变了就重算**：窗口过期（周期推进 / 全部 done / 枷锁）→ 下次心跳重新生成
//!   - 只执行第一步；第二步以后是"意图"，不是"承诺"
//!
//! 全局保留的不是"路线"，是"交通规则"（目标函数 + 约束），在 SYSTEM_PROMPT 与安全壳里。
//!
//! 落盘格式仍是 plan.json 的 nodes 数组，decide.rs / 快系统不用改：
//!   窗口 = 3 个节点的短 DAG，decide() 的选点自然只剩这 3 个可选。

use crate::decide::Plan;
use crate::slow::SlowConfig;
use serde_json::Value;

/// 窗口长度（步数）。3 步 = "现在做 / 下一步 / 再下一步"，够形成方向感又不至于固化。
pub const HORIZON: usize = 3;

/// 窗口失效条件（任一成立 → 下次心跳重算）。
pub fn window_stale(plan: &Plan, snap_cycle: i64) -> Option<String> {
    window_stale_with(plan, snap_cycle, None)
}

/// 带 goal_outcomes 目录的版本：慢系统调用时传 sd，能检测 W1 轨迹不可行。
pub fn window_stale_with(plan: &Plan, snap_cycle: i64, sd: Option<&std::path::Path>) -> Option<String> {
    let nodes = &plan.nodes;
    if nodes.is_empty() {
        return Some("窗口为空".into());
    }
    if nodes.iter().all(|n| n.is_done()) {
        return Some("窗口全部完成".into());
    }
    if nodes.iter().all(|n| n.is_done() || n.is_blocked() || n.is_deferred()) {
        return Some("窗口全部 done/blocked/deferred（枷锁效应）".into());
    }
    // ★ MPC 本职：W1 的轨迹**不可行**（同一 goal 连续失败 ≥3 次）→ 重算，
    //   而不是让 LLM 每轮对着一个推不动的 W1 反复试。
    if let Some(sd) = sd {
        if let Some(reason) = w1_trajectory_infeasible(plan, sd) {
            return Some(reason);
        }
    }
    let born = plan.window_cycle;
    if born == 0 {
        // window_cycle=0 且非空 → 不是滚动窗口（**静态 DAG** 的 requires 长链在世界变化后就是枷锁），必须升级。
        return Some("旧静态 plan（非滚动窗口）→ 升级为 horizon-3 窗口".into());
    }
    if snap_cycle - born >= 2 {
        return Some(format!("窗口已跨 {} 周期", snap_cycle - born));
    }
    None
}

/// 组装滚动窗口的 prompt：本局事实 + 上一窗口的执行结果（反馈闭环）+ 硬约束。
fn build_window_prompt(brief: &str, prev_outcomes: &str) -> (String, String) {
    let system = "你是《缺氧》殖民地的滚动窗口规划器。每次只规划未来 3 步（不是全局计划）。世界是部分可观察的：挖开才知道后面是什么。所以**不要写长链**——只写从当前局面出发、下一步最该做的 3 件事，每步带前置条件（pre）与达成条件（post）。只输出 JSON，不要任何其它文字。".to_string();
    let kinds = crate::opening::CHECK_KINDS.iter().map(|k| format!("`{k}`")).collect::<Vec<_>>().join("/");
    let user = format!(
        "== 本局世界事实 ==\n{brief}\n\n== 上一窗口的执行结果（反馈闭环）==\n{prev_outcomes}\n\n== 硬约束 ==\n1. 恰好 {HORIZON} 个节点，id 为 W1/W2/W3；W1 必须是**现在就能执行**的（pre 已满足）；\n2. 每个节点：{{id, name, requires:[...], actions:[{{kind:\"auto\",goal:\"...\",why:\"...\"}}], success_check:{{...}}, success:\"人类可读\", pre:\"执行前必须成立的条件\", post:\"达成后应成立\"}}；\n3. 语义 goal 只能取：scout/material/surface/oxygen/expand/open_chamber/power/research/forage/mop_dirty/toilet/element:<元素名>；\n4. 判据 kind 只能取：{kinds}；字段格式：loose_min{{element,kg}} / building_min{{key,n}} / prefab_min{{prefab,n}} / oxygen_producing{{}} / power_operational{{}} / reachable_min{{n}} / food_cycles_min{{cycles}} / surface_open{{minY}}；\n5. 不写坐标；AutoBuild 的 region 一律 \"auto\"；\n6. **不要重复上一窗口里已失败的目标**（除非事实变了让它重新可行）——换一条路。\n7. 卫生/氧气/食物/居住/电力五条生命线：哪条最危急就排进 W1，其余按需排进 W2/W3。\n8. **建造依赖材料（物理约束，不可跳过）**：任何建造/研究/发电/产氧节点之前，必须先有 material 或 element:X 节点把材料挖出来。事实里地上散装建材为 0 时，W1 **必须**是采建材（material）——直接排建造会因缺材料全部失败。\n9. **执行结果里连续出现同一 fail_kind（no_material / no_target / 零进展）** → 本窗口必须换方向，不要重试同一目标。\n输出 JSON：{{\"objective\":\"一句话\",\"author\":\"ai\",\"locked\":true,\"nodes\":[...]}}",
    );
    (system, user)
}

/// 从 goal_outcomes.jsonl 读最近 N 条执行结果，作为反馈闭环注入 prompt。
fn recent_outcomes(sd: &std::path::Path, n: usize) -> String {
    let Ok(text) = std::fs::read_to_string(sd.join("goal_outcomes.jsonl")) else {
        return "（暂无执行记录）".into();
    };
    let mut lines: Vec<&str> = text.lines().collect();
    lines.reverse();
    let picked: Vec<String> = lines
        .iter()
        .take(n)
        .filter_map(|l| {
            serde_json::from_str::<Value>(l).ok().map(|o| {
                format!(
                    "{} → {}: {}",
                    o.get("goal").and_then(|v| v.as_str()).unwrap_or("?"),
                    if o.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) { "成功" } else { "失败" },
                    o.get("detail").and_then(|v| v.as_str()).unwrap_or("").chars().take(80).collect::<String>()
                )
            })
        })
        .collect();
    if picked.is_empty() { "（暂无执行记录）".into() } else { picked.join("\n") }
}


/// 把 live_snapshot 压成 LLM 能直接读懂的**传感器→大脑统一简报**（决策 prompt 与窗口 prompt 共用）。
///
/// 约束：不能直接丢整份快照——LLM 看到 `"inventory": []` 推不出
/// "零建材 → 任何建造前必须先挖"。
/// 摘要把**决策真正依赖的数字**放最前面，并显式标出"物理约束是否触发"。
pub fn facts_summary(snap: &serde_json::Value, cycle: i64) -> String {
    // ★ 取数一律走传感器层的类型化访问器（sense::Snapshot），这里**不手写 JSON 路径**：
    //   手写路径读不到会被 unwrap_or(0.0) 当成 0 → 大脑按假饥荒排窗口。
    //   传感器没报的数字一律标"未知"（AGENTS.md §9：读不到 ≠ 极值）。
    let s: crate::sense::Snapshot = match serde_json::from_value(snap.clone()) {
        Ok(s) => s,
        Err(e) => return format!(
            "周期 {cycle}；⚠ 快照无法解析为 Snapshot（{e}）→ 事实未知，不要假设任何存量为 0"),
    };
    // ★ 这是**唯一**的传感器→大脑简报：决策 prompt（选目标/解卡点）和窗口 prompt（排 3 步）都用它。
    //   覆盖面 = 五条生命线 + 在途工作 + 传感器健康。
    let opt = |v: Option<f64>| v.map(|k| format!("{k:.0}")).unwrap_or_else(|| "未知".into());
    let mat_tags = ["SandStone", "Dirt", "Algae", "Sand", "Clay", "Cuprite", "Carbon", "IronOre"];
    // 建材总量只用于"建材为零"物理约束；清单本身见下方"地上物品"（不设白名单）
    let mut loose_total = 0.0;
    for tag in mat_tags {
        let kg = s.loose_kg(tag);
        if kg > 0.0 && ["SandStone", "Dirt", "Algae", "Sand", "Clay"].contains(&tag) { loose_total += kg; }
    }
    // 只有传感器真的报了 inventory 才能断言"建材为零"；inventory 缺失 ≠ 没有
    let inventory_known = s.inventory.get("items").map(|v| v.is_array()).unwrap_or(false);
    let build_mat_zero = inventory_known && loose_total < 50.0;
    let dupes = s.dupes.len();
    let bladder = s.max_bladder();
    let min_breath = s.dupes.iter().filter_map(|d| d.breath).fold(f64::INFINITY, f64::min);
    let breath_txt = if min_breath.is_finite() { format!("{min_breath:.0}%") } else { "未知".to_string() };
    // 官方可达格为 0 而复制人还站着 → 顶层读数没采到，退到每人的可达数；都没有才是"未知"
    let reachable = if s.reachable_cells > 0 {
        s.reachable_cells.to_string()
    } else {
        s.dupes.iter().filter_map(|d| d.reachable).max()
            .map(|r| format!("{r}（按每人）")).unwrap_or_else(|| "未知".into())
    };
    let oxygen = s.oxygen_kg_opt().map(|k| format!("{k:.0} kg")).unwrap_or_else(|| "未知".into());
    let food = s.food_kcal_opt().map(|k| format!("{k:.0} kcal")).unwrap_or_else(|| "未知".into());
    let pending = s.pending_orders.map(|n| n.to_string()).unwrap_or_else(|| "未知".into());
    // ---- 续航（§8：用续航不用绝对值）----
    let food_days = s.food.get("daysRemaining").and_then(|v| v.as_f64());
    let food_status = s.food.get("status").and_then(|v| v.as_str()).unwrap_or("").to_string();
    // 读不到 ≠ 0：atmos 源缺失时这些数一律"未知"
    let o2_per_dupe = s.atmos.get("oxygenPerDupeKg").and_then(|v| v.as_f64());
    // 氧气续航没有官方值：按复制人呼吸 100 g/s ≈ 60 kg/人/周期估算，假设写在文案里让 LLM 可推翻
    let o2_cycles = o2_per_dupe.filter(|k| *k > 0.0).map(|k| format!("{:.1}", k / 60.0)).unwrap_or_else(|| "未知".into());
    let world = if s.paused || (s.game_speed >= 0.0 && s.game_speed <= 0.0) { "暂停/速度0" } else { "运行中" };
    let mut out = String::new();
    // ★ 传感器健康放第一行：数据可能过期时，下面所有数字都不该用来规划
    match s.health {
        crate::sense::SensorHealth::Stale => out.push_str(&format!(
            "⚠ 传感器数据可能过期（mod 心跳未推进）→ 不要据此规划或换目标；{}\n", s.health_notes.join("；"))),
        crate::sense::SensorHealth::Degraded => out.push_str(&format!(
            "⚠ 传感器降级（对应数字不可信，不要当成 0）：{}\n", s.health_notes.join("；"))),
        crate::sense::SensorHealth::Ok => {}
    }
    let day_pct = s.clock.map(|c| format!("{:.0}%", c.1)).unwrap_or_else(|| "未知".into());
    out.push_str(&format!("周期 {cycle}（进度 {day_pct}）；人口 {dupes}；世界 {world}；可达格 {reachable}；最低呼吸 {breath_txt}；最高膀胱 {bladder:.0}%\n"));
    // 人口 0 = 死局：第一行就喊停，大脑不得再规划（mod 的 daysRemaining 在无人时会报 999 之类的荒诞值）
    if dupes == 0 { out.push_str("☠️ 人口 0：殖民地已无复制人 → 不要规划、不要换目标，建议回滚存档\n"); }
    // ★ 日程事实（官方 workScheduled）：全员睡觉时重算窗口，LLM 必须知道"工单要等开工才执行"
    //   （否则醒前的零进展会被读成失败）。
    out.push_str(&format!("日程：{}\n", s.schedule_summary()));
    // ---- 每个人的体征 + 所处环境（站在污染水/CO₂ 里是选址与救援的关键事实）----
    if !s.dupes.is_empty() {
        out.push_str("复制人：\n");
        for d in &s.dupes {
            out.push_str(&format!("  {} @({},{}) {} 呼吸{} 压力{} 膀胱{} 体力{} 站在:{} 可达{}\n",
                d.name, d.x, d.y, d.chore,
                opt(d.breath), opt(d.stress), opt(d.bladder), opt(d.stamina), d.element,
                d.reachable.map(|r| r.to_string()).unwrap_or_else(|| "未知".into())));
        }
    }
    // ---- 大气 / 食物（续航）----
    // ★ 气体全量清单（不只 CO₂/污染氧：氯气等有毒气体也必须让大脑看见）
    let gas_label = |el: &str| match el {
        "Oxygen" => "O₂".to_string(), "CarbonDioxide" => "CO₂".to_string(),
        "ContaminatedOxygen" => "污染氧".to_string(), "Hydrogen" => "氢".to_string(),
        "ChlorineGas" => "⚠ 氯(有毒)".to_string(), "NaturalGas" => "天然气".to_string(),
        "Steam" => "蒸汽".to_string(), "Propane" => "丙烷".to_string(), other => other.to_string(),
    };
    let gases: Vec<String> = s.gas.get("dominantGases").and_then(|v| v.as_array()).map(|a| {
        a.iter().filter_map(|g| {
            let el = g.get("element")?.as_str()?;
            let kg = g.get("massKg")?.as_f64()?;
            let cells = g.get("cellCount").and_then(|c| c.as_i64()).unwrap_or(0);
            Some(format!("{} {kg:.0}kg/{cells}格", gas_label(el)))
        }).collect()
    }).unwrap_or_default();
    out.push_str(&format!(
        "氧气 {}（{} kg/人，≈{} 周期，按 60kg/人/周期估）；气体：{}；窒息风险 {}；可呼吸格 {}/{}\n",
        oxygen, opt(o2_per_dupe), o2_cycles,
        if gases.is_empty() { "未知".to_string() } else { gases.join("；") },
        if s.suffocation_risk().is_empty() { "未知".into() } else { s.suffocation_risk() },
        opt(s.atmos.get("breathableCells").and_then(|v| v.as_f64())),
        opt(s.atmos.get("totalVisibleCells").and_then(|v| v.as_f64()))));
    // ---- 液体：清水是电解/研究/厕所的资源；污染水是卫生隐患，人泡在里面要处理 ----
    match s.liquids() {
        Some(ls) if !ls.is_empty() => {
            let parts: Vec<String> = ls.iter().map(|(el, kg, cells)| {
                if el == "DirtyWater" || el == "SewageWater" { format!("⚠ 污染水({el}) {kg:.0} kg / {cells} 格") }
                else { format!("{el} {kg:.0} kg / {cells} 格") }
            }).collect();
            out.push_str(&format!("液体：{}\n", parts.join("；")));
            let in_dirty = s.dirty_liquid_dupe_count();
            let dirty_kg: f64 = ls.iter().filter(|(el, _, _)| el == "DirtyWater" || el == "SewageWater").map(|(_, kg, _)| kg).sum();
            if dirty_kg > 0.0 || in_dirty > 0 {
                out.push_str(&format!("⚠ 卫生：污染水 {dirty_kg:.0} kg{} → 小面积用 mop_dirty 拖掉；大面积/持续产生（厕所/随地排泄）要存储或处理，泡在里面会积 germs 与压力\n",
                    if in_dirty > 0 { format!("，{in_dirty} 人正泡在里面") } else { String::new() }));
            }
        }
        Some(_) => out.push_str("液体：**0（什么都没有）**\n"),
        None => out.push_str("液体：未知（传感器未报）\n"),
    }
    // 食物构成：营养棒士气 -1、不可持续；大脑要知道"吃什么"而不只是多少 kcal
    let foods: Vec<String> = s.food_detail.get("foods").and_then(|v| v.as_array()).map(|a| {
        a.iter().filter_map(|f| {
            let n = f.get("name")?.as_str()?;
            let morale = f.get("morale").and_then(|m| m.as_i64()).unwrap_or(0);
            Some(format!("{n}(士气{morale:+})"))
        }).collect()
    }).unwrap_or_default();
    out.push_str(&format!("食物 {food}（≈{} 周期，mod 口径{}{}）\n",
        food_days.filter(|d| dupes > 0 && *d < 100.0).map(|d| format!("{d:.1}")).unwrap_or_else(|| if dupes == 0 { "—".into() } else { "未知".into() }),
        if food_status.is_empty() { String::new() } else { format!("，状态 {food_status}") },
        if foods.is_empty() { String::new() } else { format!("；构成：{}", foods.join("、")) }));
    // ---- 建筑与电力（§8「能不能造」三关：材料 + 研究锁 + 运转条件）----
    // ★ 梯子数（导航能力）+ 其它已建 prefab 清单（大脑需要知道有没有梯子/口粮箱）
    let ladders = s.building_count("Ladder");
    let others: Vec<String> = s.buildings.as_object().map(|m| {
        m.iter().filter(|(k, _)| !matches!(k.as_str(), "Outhouse" | "Ladder"))
            .map(|(k, v)| format!("{k}×{}", v.as_i64().unwrap_or(0))).collect()
    }).unwrap_or_default();
    out.push_str(&format!("建筑：厕所 {} / 床 {} / 制氧 {} / 研究站 {} / 电池 {}；梯子 {} 格{}\n",
        s.building("toilets"), s.building("beds"), s.building("oxygenProducers"),
        s.building("researchStations"), s.building("batteries"), ladders,
        if others.is_empty() { String::new() } else { format!("；其它 {}", others.join("、")) }));
    // ★ 研究状态（§8 三关之一：研究锁）：大脑排"研究制氧"时得知道现在在研什么、研究站有没有
    let queue_n = s.research.get("queue").and_then(|q| q.as_array()).map(|q| q.len()).unwrap_or(0);
    let stations = s.building("researchStations");
    match s.research_brief() {
        Some((name, pct, unlocks)) => out.push_str(&format!(
            "研究：在研 {name}（{pct:.0}%{}）；队列 {queue_n}；研究站 {stations}{}\n",
            if unlocks.is_empty() { String::new() } else { format!("，解锁 {}", unlocks.join("/")) },
            if stations == 0 { " → 没有研究站，研究不会推进" } else { "" })),
        None => out.push_str(&format!("研究：无在研（队列 {queue_n}）；研究站 {stations}{}\n",
            if stations == 0 { " → 先建研究站（goal research）" } else { " → 由你定方向：research <TechId>" })),
    }
    {
        // 打印舱：可领取项是战略资源，领哪个由大脑决定（pod claim <index>）；新人只能在游戏 UI 里选
        let imm = s.immigrants_available == Some(true);
        let pod = s.printing_rewards.as_ref().filter(|p| p.available && p.reward_count > 0);
        if pod.is_some() || imm {
            let mut parts: Vec<String> = Vec::new();
            if let Some(p) = pod {
                parts.push(format!("{} 个物品包（{}）→ 大脑用 pod claim <index> 领", p.reward_count, p.rewards.join(" / ")));
            }
            if imm { parts.push("可打印新人（mod 不支持自动选人，需用户在游戏里选）".into()); }
            out.push_str(&format!("打印舱：✅ 可领取：{}\n", parts.join("；")));
        }
    }
    out.push_str(&format!("电力：发电机 {}（运行 {}，缺燃料 {}）/ 电池 {}（储电 {}%）/ 用电设备 {}（通电 {}）/ 净 {} W\n",
        s.power_int("generatorCount"), s.power_int("generatorOperational"), s.power_int("generatorEmpty"),
        s.power_int("batteryCount"), opt(Some(s.power_num("batteryStoredPercent"))),
        s.power_int("consumerCount"), s.power_int("consumerPowered"), opt(Some(s.power_num("netActiveWatts")))));
    // ---- 在途工作（有活排队 ≠ 空跑；施工中 ≠ 没人干活）----
    out.push_str(&format!("在途：待办工单 {pending}；施工中 {}\n", s.under_construction));
    // ---- 游戏自带告警（官方事实优先）----
    if let Some(alerts) = s.diagnostics.get("alerts").and_then(|v| v.as_array()) {
        let lines: Vec<String> = alerts.iter().take(6).filter_map(|a| {
            let msg = a.get("message").and_then(|m| m.as_str())?;
            let cat = a.get("category").and_then(|c| c.as_str()).unwrap_or("");
            let sev = a.get("severity").and_then(|c| c.as_str()).unwrap_or("");
            Some(format!("[{cat}/{sev}] {msg}"))
        }).collect();
        if !lines.is_empty() { out.push_str(&format!("游戏告警：{}\n", lines.join("；"))); }
    }
    // ---- 传感器健康：读失败必须可见，否则 LLM 把 0/空当成真值 ----
    if !s.errors.is_empty() {
        let urs: Vec<String> = s.errors.iter().take(4).map(|e| e.replace("oni://", "")).collect();
        out.push_str(&format!("⚠ 传感器：{} 个观察源读取失败（对应数字不可信，不要当成 0）：{}\n", s.errors.len(), urs.join("，")));
    }
    if !inventory_known {
        out.push_str("地上物品：未知（传感器未报 inventory）\n");
    } else {
        // ★ 物品清单**不设白名单**（白名单会漏掉磷矿/氧岩等）：列出全部有质量的非复制人物品。
        let mut items: Vec<(String, f64)> = s.inventory.get("items").and_then(|i| i.as_array()).map(|a| {
            a.iter().filter(|it| it.get("prefabId").and_then(|p| p.as_str()) != Some("Minion"))
                .filter_map(|it| Some((it.get("name").and_then(|n| n.as_str()).unwrap_or("?").to_string(),
                    it.get("totalMassKg").and_then(|m| m.as_f64()).unwrap_or(0.0))))
                .filter(|(_, kg)| *kg > 0.0).collect()
        }).unwrap_or_default();
        items.sort_by(|a, b| b.1.total_cmp(&a.1));
        let stored: f64 = s.storage.get("storages").and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|x| x.get("massStoredKg").and_then(|m| m.as_f64())).sum()).unwrap_or(0.0);
        if items.is_empty() {
            out.push_str("地上物品：**0（什么都没有）**\n");
        } else {
            let txt: Vec<String> = items.iter().map(|(n, kg)| format!("{n} {kg:.0} kg")).collect();
            out.push_str(&format!("地上物品：{}{}\n", txt.join("；"), if stored > 0.0 { format!("（已入箱 {stored:.0} kg）") } else { String::new() }));
        }
    }
    if build_mat_zero {
        out.push_str("⚠ 物理约束触发：**建材为零** → 任何建造（厕所/床/发电机/研究站/产氧）都不可能成功，第一步必须是采建材（material）或挖矿（element:X）。\n");
    }
    if bladder >= 80.0 { out.push_str("⚠ 卫生红线：有人膀胱 ≥80% → 厕所是最高优先级（但前提是先有建材）。\n"); }
    if min_breath < 50.0 { out.push_str("⚠ 窒息红线：有人呼吸 <50% → 救援/换气是最高优先级。\n"); }
    out
}

/// 校验前先**适配 LLM 的常见写法偏差**（契约层容错，而非拒绝）。
///
/// LLM 会把自然语言前置条件（"存在可安全移动的复制人"）写进 `requires` 数组，
/// 而规范是 `requires` 只放节点 ID、前置条件文本放 `pre` 字段。
/// 约束：拒绝整份窗口会让殖民地继续跑旧 DAG，所以自动迁移而不是拒绝。
pub fn adapt_window(plan: &mut Plan) -> Vec<String> {
    let ids: std::collections::HashSet<String> = plan.nodes.iter().map(|n| n.id.clone()).collect();
    let mut notes = Vec::new();
    for n in &mut plan.nodes {
        let (valid, prose): (Vec<String>, Vec<String>) = n
            .requires
            .iter()
            .cloned()
            .partition(|r| ids.contains(r));
        if !prose.is_empty() {
            notes.push(format!("{}: {} 条自然语言前置从 requires 迁移到 pre", n.id, prose.len()));
            n.requires = valid;
            let merged = prose.join("；");
            n.note = if n.note.is_empty() { merged } else { format!("{}；{}", n.note, merged) };
        }
    }
    notes
}

/// 校验滚动窗口（比 opening 的全局校验轻：不要求五条生命线全覆盖，那是安全壳的事）。
pub fn validate_window(plan: &Plan) -> Vec<String> {
    let mut errs = Vec::new();
    if plan.nodes.is_empty() {
        errs.push("窗口为空".into());
        return errs;
    }
    if plan.nodes.len() > HORIZON + 2 {
        errs.push(format!("窗口过长（{} 步），最多 {}", plan.nodes.len(), HORIZON + 2));
    }
    if let Some(w1) = plan.nodes.iter().find(|n| n.id == "W1") {
        if !w1.requires.is_empty() {
            errs.push("W1 不得有 requires（它必须现在就能执行）".into());
        }
    }
    for n in &plan.nodes {
        if n.actions.is_empty() {
            errs.push(format!("{} 无动作", n.id));
        }
        if n.success_check.is_none() {
            errs.push(format!("{} 缺 success_check", n.id));
        }
        for r in &n.requires {
            if !plan.nodes.iter().any(|m| &m.id == r) {
                errs.push(format!("{} requires 不存在的节点 {r}", n.id));
            }
        }
    }
    errs
}

/// 滚动窗口编排：LLM 生成 → 校验 → 落盘（覆盖 plan.json）。
pub fn refresh_window(cfg: &SlowConfig, brief: &str, plan_path: &str, fp: Option<&str>, cycle: i64, sd: &std::path::Path) -> Result<String, String> {
    let (system, user) = build_window_prompt(brief, &recent_outcomes(sd, 6));
    let raw = crate::slow::call_llm(cfg, &system, &user, 8000)?;
    let json_text = crate::opening::extract_json(&raw).ok_or("LLM 输出里没有 JSON")?;
    let mut v: Value = serde_json::from_str(json_text).map_err(|e| format!("窗口 JSON 解析失败: {e}"))?;
    if let Some(o) = v.as_object_mut() {
        o.insert("author".into(), Value::from("ai"));
        o.insert("locked".into(), Value::from(true));
        o.insert("window_cycle".into(), Value::from(cycle));
        if let Some(f) = fp { o.insert("save_fingerprint".into(), Value::from(f.to_string())); }
    }
    let mut plan: Plan = serde_json::from_value(v.clone()).map_err(|e| format!("窗口 schema 不合法: {e}"))?;
    let repairs = crate::opening::repair_plan_checks(&mut plan);
    let adapted = adapt_window(&mut plan);
    let errs = validate_window(&plan);
    if !errs.is_empty() {
        let names: Vec<String> = plan.nodes.iter().map(|n| format!("{}({})", n.id, n.name.chars().take(10).collect::<String>())).collect();
        return Err(format!("窗口校验未通过（{} 条）：{}；节点：{}", errs.len(), errs.join("；"), names.join("、")));
    }
    // 落盘**适配后**的 plan（而不是原始 v）：否则 requires 里的自然语言会原样写回，
    //   decide() 读到 requires 不存在的节点 → 整张窗口被当成死锁。
    let mut out_v = serde_json::to_value(&plan).map_err(|e| format!("窗口序列化失败: {e}"))?;
    if let (Some(o), Some(src_o)) = (out_v.as_object_mut(), v.as_object()) {
        for k in ["window_cycle", "save_fingerprint", "objective"] {
            if let Some(x) = src_o.get(k) { o.insert(k.to_string(), x.clone()); }
        }
    }
    std::fs::write(plan_path, serde_json::to_string_pretty(&out_v).unwrap_or_default())
        .map_err(|e| format!("写盘失败: {e}"))?;
    let mut msg = format!("滚动窗口已刷新（{} 步，周期 {}）", plan.nodes.len(), cycle);
    if !repairs.is_empty() { msg.push_str(&format!("；⚠ {} 条判据自愈（goal 兜底/manual）", repairs.len())); }
    if !adapted.is_empty() { msg.push_str(&format!("；适配 {} 处 requires→pre", adapted.len())); }
    Ok(msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decide::Node;

    /// 给大脑的事实摘要必须读传感器层的**真实路径**，不能手写路径读成 0。
    ///   锁住的不变量：快照里氧气 398kg / 食物 16000kcal / 床 1 / 研究站 2 / 发电机 1 / 砂岩 400kg，
    ///   摘要必须原样报出来。
    #[test]
    fn facts_summary_reads_truth_not_zero() {
        let snap = serde_json::json!({
            "cycle": 1, "paused": false, "game_speed": 1.0, "under_construction": 2,
            "diagnostics": {
                "oxygenKgVisible": 398.1, "pollutedOxygenKgVisible": 23.5,
                "buildings": {"toilets": 0, "beds": 1, "oxygenProducers": 0, "researchStations": 2, "batteries": 1},
                "alerts": [{"category": "oxygen", "message": "未检测到制氧设备。", "severity": "warning"}],
            },
            "atmos": {"oxygenPerDupeKg": 140.5, "carbonDioxideMassKg": 102.7, "suffocationRisk": "nominal",
                      "breathableCells": 262, "totalVisibleCells": 3450},
            "inventory": {"items": [
                {"elementId": "FieldRation", "prefabId": "FieldRation", "name": "营养棒", "totalCaloriesKcal": 16000.0, "totalMassKg": 20.0},
                {"elementId": "SandStone", "prefabId": "SandStone", "name": "砂岩", "totalMassKg": 400.0},
                {"elementId": "Phosphorite", "prefabId": "Phosphorite", "name": "磷矿", "totalMassKg": 10.0},
            ]},
            "power": {"summary": {"generatorCount": 2, "generatorOperational": 1, "generatorEmpty": 1,
                                  "batteryCount": 1, "batteryStoredPercent": 37.5,
                                  "consumerCount": 3, "consumerPowered": 2, "netActiveWatts": -120.0}},
            "food": {"totalKcal": 16000.0, "daysRemaining": 5.3, "status": "nominal"},
            "gas": {"dominantGases": [{"element": "Oxygen", "massKg": 398.1, "cellCount": 249}, {"element": "ChlorineGas", "massKg": 41.1, "cellCount": 45}]},
            "buildings": {"Headquarters": 1, "RationBox": 1, "Ladder": 3},
            "storage": {"storages": [{"prefabId": "RationBox", "massStoredKg": 13.2}]},
            "research": {"active": {"id": "FarmingTech", "name": "<link=\"FARMINGTECH\">基础耕种</link>", "progress": 0.4, "unlocks": [{"id": "AlgaeHabitat", "name": "<link=\"ALGAEHABITAT\">藻类箱</link>"}]}, "queue": [{"id": "FarmingTech"}]},
            "food_detail": {"foods": [{"name": "营养棒", "morale": -1, "totalCaloriesKcal": 16000.0}]},
            "immigrants_available": true, "clock": [1, 42.0],
            "pending_orders": 1, "errors": [],
            "reachable_cells": 12,
            "dupes": [{"name": "t", "x": 3, "y": 4, "chore": "空闲", "breath": 72.0, "stress": 8.0,
                       "bladder": 31.0, "stamina": 85.0, "element": "污染水", "schedule": "工作", "blocked": 0,
                       "reachable": 9, "work_scheduled": true}],
        });
        let s = facts_summary(&snap, 1);
        assert!(s.contains("氧气 398 kg"), "{s}");
        assert!(s.contains("食物 16000 kcal"), "{s}");
        assert!(s.contains("≈5.3 周期，mod 口径"), "{s}");
        assert!(s.contains("≈2.3 周期"), "氧气续航估算：{s}");
        assert!(s.contains("厕所 0 / 床 1 / 制氧 0 / 研究站 2 / 电池 1"), "{s}");
        assert!(s.contains("发电机 2（运行 1，缺燃料 1）/ 电池 1（储电 38%）/ 用电设备 3（通电 2）/ 净 -120 W"), "{s}");
        assert!(s.contains("t @(3,4) 空闲 呼吸72 压力8 膀胱31 体力85 站在:污染水 可达9"), "{s}");
        assert!(s.contains("待办工单 1；施工中 2"), "{s}");
        assert!(s.contains("游戏告警：[oxygen/warning] 未检测到制氧设备。"), "{s}");
        assert!(s.contains("地上物品：砂岩 400 kg；营养棒 20 kg；磷矿 10 kg（已入箱 13 kg）"), "物品不设白名单：{s}");
        assert!(s.contains("周期 1（进度 42%）"), "{s}");
        assert!(s.contains("O₂ 398kg/249格；⚠ 氯(有毒) 41kg/45格"), "气体全量含氯：{s}");
        assert!(s.contains("呼吸72 压力8 膀胱31"), "{s}");
        assert!(s.contains("梯子 3 格；其它 Headquarters×1、RationBox×1"), "{s}");
        assert!(s.contains("研究：在研 基础耕种（40%，解锁 藻类箱）；队列 1；研究站 2"), "{s}");
        assert!(s.contains("构成：营养棒(士气-1)"), "{s}");
        assert!(s.contains("打印舱：✅ 可领取：可打印新人（mod 不支持自动选人"), "{s}");
        assert!(s.contains("可达格 12"), "{s}");
        assert!(!s.contains("建材为零"), "有 400kg 砂岩不该报建材为零：{s}");
        assert!(!s.contains("传感器："), "没有读失败就不该报：{s}");
    }

    /// 液体清单必须进简报：污染水几吨、几个人泡在里面。
    #[test]
    fn facts_summary_reports_liquids_and_pollution() {
        let snap = serde_json::json!({
            "cycle": 4,
            "liquid": {"liquids": [
                {"element": "Water", "cellCount": 195, "massKg": 183890.8, "avgTempC": 22.4},
                {"element": "DirtyWater", "cellCount": 26, "massKg": 7226.0, "avgTempC": 26.8}]},
            "dupes": [{"name": "a", "x": 0, "y": 0, "chore": "睡觉", "breath": 100.0, "bladder": 10.0,
                       "element": "污染水", "schedule": "Sleep", "blocked": 0, "work_scheduled": false}],
        });
        let s = facts_summary(&snap, 4);
        assert!(s.contains("液体：Water 183891 kg / 195 格；⚠ 污染水(DirtyWater) 7226 kg / 26 格"), "{s}");
        assert!(s.contains("⚠ 卫生：污染水 7226 kg，1 人正泡在里面 → 小面积用 mop_dirty"), "{s}");
        let clean = facts_summary(&serde_json::json!({
            "cycle": 4, "liquid": {"liquids": [{"element": "Water", "cellCount": 5, "massKg": 5000.0}]},
        }), 4);
        assert!(clean.contains("液体：Water 5000 kg / 5 格") && !clean.contains("卫生："), "{clean}");
        assert!(facts_summary(&serde_json::json!({"cycle": 4}), 4).contains("液体：未知"), "源缺失 → 未知，不是 0");
    }

    /// 人口 0 = 死局：简报第二行必须喊停，且食物续航不能拿 mod 在无人时报的 999 之类的值。
    #[test]
    fn facts_summary_leads_with_dead_colony() {
        let s = facts_summary(&serde_json::json!({
            "cycle": 5, "food": {"daysRemaining": 999.0, "totalKcal": 10583.0}, "dupes": [],
            "inventory": {"items": [{"prefabId": "FieldRation", "name": "营养棒", "totalCaloriesKcal": 10583.0, "totalMassKg": 13.0}]},
        }), 5);
        assert!(s.contains("☠️ 人口 0：殖民地已无复制人"), "{s}");
        assert!(s.contains("食物 10583 kcal（≈— 周期") && !s.contains("999"), "{s}");
    }

    /// 传感器数据可能过期时，简报第一行必须喊停：下面所有数字都不该用来规划。
    #[test]
    fn facts_summary_flags_stale_sensor_first() {
        let s = facts_summary(&serde_json::json!({
            "cycle": 4, "health": "stale",
            "health_notes": ["mod 心跳未推进（realtimeSinceStartup 不变）→ 响应可能是缓存，数据不可信", "已重连 session，下一拍复核"],
        }), 4);
        assert!(s.starts_with("⚠ 传感器数据可能过期"), "{s}");
        assert!(s.contains("已重连 session"), "{s}");
        let d = facts_summary(&serde_json::json!({
            "cycle": 4, "health": "degraded", "health_notes": ["周期不一致：status=4 diagnostics=1"],
        }), 4);
        assert!(d.starts_with("⚠ 传感器降级") && d.contains("周期不一致"), "{d}");
        let ok = facts_summary(&serde_json::json!({"cycle": 4, "health": "ok"}), 4);
        assert!(ok.starts_with("周期 4"), "{ok}");
    }

    /// 传感器读失败必须可见：否则 LLM 把 0/空当成真值（AGENTS.md §9：读不到 ≠ 极值）。
    #[test]
    fn facts_summary_surfaces_sensor_errors() {
        let s = facts_summary(&serde_json::json!({
            "cycle": 1, "errors": ["oni://fluid/gas/summary: timeout", "oni://power/summary: 503"],
        }), 1);
        assert!(s.contains("⚠ 传感器：2 个观察源读取失败"), "{s}");
        assert!(s.contains("fluid/gas/summary: timeout"), "{s}");
        assert!(s.contains("power/summary: 503"), "{s}");
    }

    /// 窗口 prompt 必须带日程事实：全员就寝时重算，LLM 要知道工单要等开工才执行。
    #[test]
    fn facts_summary_reports_schedule() {
        let snap = serde_json::json!({
            "cycle": 1,
            "dupes": [{"name": "a", "x": 0, "y": 0, "chore": "睡觉", "breath": 100.0, "bladder": 10.0,
                       "element": "氧气", "schedule": "Sleep", "blocked": 0, "work_scheduled": false},
                      {"name": "b", "x": 1, "y": 0, "chore": "放松", "breath": 100.0, "bladder": 10.0,
                       "element": "氧气", "schedule": "Recreation", "blocked": 0, "work_scheduled": false}],
        });
        let s = facts_summary(&snap, 1);
        assert!(s.contains("日程：工作 0 / 非工作 2"), "{s}");
        assert!(s.contains("不是卡住"), "{s}");
        assert!(s.contains("氧气 未知"), "日程行不影响其它事实");
    }

    /// 传感器没报的数字必须标"未知"，不能编一个 0 让大脑按假饥荒决策（AGENTS.md §9）。
    ///   也不能把"没报 inventory"当成"建材为零"去触发物理约束。
    #[test]
    fn facts_summary_says_unknown_when_sensor_silent() {
        let s = facts_summary(&serde_json::json!({"cycle": 1, "dupes": []}), 1);
        assert!(s.contains("氧气 未知"), "{s}");
        assert!(s.contains("食物 未知"), "{s}");
        assert!(s.contains("最低呼吸 未知"), "{s}");
        assert!(s.contains("地上物品：未知"), "{s}");
        assert!(!s.contains("建材为零"), "没报 inventory 不能断言建材为零：{s}");
        // 快照整体不是 Snapshot → 明说事实未知，而不是返回一串 0
        let bad = facts_summary(&serde_json::Value::Null, 1);
        assert!(bad.contains("事实未知"), "{bad}");
    }

    /// inventory 报了但确实没有建材 → 物理约束必须触发（这条语义不能被上一条误伤）。
    #[test]
    fn facts_summary_keeps_zero_material_constraint_when_inventory_reported() {
        let s = facts_summary(&serde_json::json!({
            "cycle": 1, "inventory": {"items": [{"elementId": "FieldRation", "totalCaloriesKcal": 16000.0}]},
            "dupes": [],
        }), 1);
        assert!(s.contains("地上物品：**0（什么都没有）**"), "{s}");
        assert!(s.contains("建材为零"), "{s}");
        assert!(s.contains("食物 16000 kcal"), "{s}");
    }

    fn node(id: &str, status: &str, req: Vec<&str>) -> Node {
        Node {
            id: id.into(), name: id.into(), status: status.into(),
            requires: req.into_iter().map(|s| s.to_string()).collect(),
            unlocks: vec![], actions: vec![], success_check: None, success: String::new(),
            rescue: false, note: String::new(), priority: None, deferred: None, deferred_at: None, stall: None,
        }
    }
    fn plan(nodes: Vec<Node>, cycle: i64) -> Plan {
        Plan { nodes, objective: String::new(), survival_node: None, window_cycle: cycle, ..Default::default() }
    }

    #[test]
    fn stale_when_empty_or_all_done_or_all_stuck() {
        assert!(window_stale(&plan(vec![], 0), 5).is_some());
        let all_done = plan(vec![node("W1", "done", vec![]), node("W2", "done", vec!["W1"])], 5);
        assert!(window_stale(&all_done, 6).is_some(), "全部完成 → 过期");
        let mut stuck = plan(vec![node("W1", "blocked", vec![]), node("W2", "todo", vec!["W1"])], 5);
        stuck.nodes[1].deferred = Some("x".into());
        assert!(window_stale(&stuck, 6).is_some(), "全部 blocked/deferred → 枷锁效应 → 过期");
    }

    #[test]
    fn stale_after_two_cycles_even_if_not_done() {
        let p = plan(vec![node("W1", "todo", vec![])], 10);
        assert!(window_stale(&p, 11).is_none(), "跨 1 周期不过期");
        assert!(window_stale(&p, 12).is_some(), "跨 2 周期 → 世界大概率变了 → 过期");
    }

    #[test]
    fn fresh_window_is_not_stale() {
        let p = plan(vec![node("W1", "todo", vec![]), node("W2", "todo", vec!["W1"]), node("W3", "todo", vec!["W2"])], 10);
        assert!(window_stale(&p, 11).is_none());
    }


    /// MPC 本职：W1 的 goal 连续失败 ≥3 次 → 轨迹不可行 → 窗口重算。
    #[test]
    fn w1_repeated_failure_marks_window_stale() {
        let dir = std::env::temp_dir().join(format!("oni_w1_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let p = plan(vec![node("W1", "todo", vec![]), node("W2", "todo", vec!["W1"])], 10);
        let mut w1 = p.clone(); w1.nodes[0].actions = vec![crate::exec::Action::Auto { goal: "material".into(), why: String::new() }];
        // 无 outcomes → 不过期
        let _ = std::fs::remove_file(dir.join("goal_outcomes.jsonl"));
        assert!(super::w1_trajectory_infeasible(&w1, &dir).is_none());
        // 连续 2 次失败 → 还没到阈值
        let two = (0..2).map(|i| format!("{{\"goal\":\"material\",\"ok\":false,\"detail\":\"x\",\"ts\":{}}}", 100 + i)).collect::<Vec<_>>().join("\n");
        std::fs::write(dir.join("goal_outcomes.jsonl"), &two).unwrap();
        assert!(super::w1_trajectory_infeasible(&w1, &dir).is_none(), "2 次不应触发");
        // 连续 3 次失败 → 不可行
        let three = format!("{}\n{{\"goal\":\"material\",\"ok\":false,\"detail\":\"x\",\"ts\":103}}", two);
        std::fs::write(dir.join("goal_outcomes.jsonl"), &three).unwrap();
        let r = super::w1_trajectory_infeasible(&w1, &dir);
        assert!(r.is_some() && r.unwrap().contains("material"), "3 次应触发重算");
        // 中间有成功 → 计数中断，不触发
        let with_ok = format!("{}\n{{\"goal\":\"material\",\"ok\":true,\"detail\":\"ok\",\"ts\":104}}\n{{\"goal\":\"material\",\"ok\":false,\"detail\":\"x\",\"ts\":105}}", three);
        std::fs::write(dir.join("goal_outcomes.jsonl"), &with_ok).unwrap();
        assert!(super::w1_trajectory_infeasible(&w1, &dir).is_none(), "成功应中断计数");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn w1_must_be_executable_now() {
        let bad = plan(vec![node("W1", "todo", vec!["W2"]), node("W2", "todo", vec![])], 1);
        assert!(validate_window(&bad).iter().any(|e| e.contains("W1 不得有 requires")));
    }
}


/// W1（第一个可执行节点）的 goal 是否**连续失败 ≥3 次**。
/// 读 goal_outcomes.jsonl 从最新往回数，同一 goal 连续 ok=false 的条数（遇到成功即停）。
pub fn w1_trajectory_infeasible(plan: &Plan, sd: &std::path::Path) -> Option<String> {
    let done: std::collections::HashSet<&str> = plan.nodes.iter().filter(|n| n.is_done()).map(|n| n.id.as_str()).collect();
    let w1 = plan.nodes.iter().find(|n| {
        !n.is_done() && !n.is_blocked() && !n.is_deferred()
            && n.requires.iter().all(|r| done.contains(r.as_str()))
    })?;
    let goal = w1.actions.iter().find_map(|a| match a {
        crate::exec::Action::Auto { goal, .. } => Some(goal.as_str()),
        _ => None,
    })?;
    let text = std::fs::read_to_string(sd.join("goal_outcomes.jsonl")).ok()?;
    let mut fails = 0usize;
    for line in text.lines().rev() {
        let Ok(o) = serde_json::from_str::<Value>(line) else { continue };
        if o.get("goal").and_then(|v| v.as_str()) != Some(goal) { continue; }
        if o.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) { break; }
        fails += 1;
        if fails >= 3 {
            return Some(format!("W1「{}」goal={} 连续失败 {} 次 → 轨迹不可行，重算", w1.name, goal, fails));
        }
    }
    None
}
