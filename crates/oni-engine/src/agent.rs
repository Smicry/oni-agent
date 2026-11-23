//! AGENT —— 常驻循环守护进程。
//!
//! 为什么要有它：**主 agent 的思考要几分钟，游戏不会等你**。
//! 守护进程在后台自己跑完整循环，观察/判断/冻结/决策/执行/验证/恢复/记录，
//! 于是游戏在我思考期间也在推进，而不是被我拖成空跑。
//!
//! 暂停策略（与主 agent 的判断配对）：
//! * 紧急态 → 立刻暂停，直到条件解除才恢复（避免复制人在我思考期间憋死）
//! * 检测到**手动暂停**且无紧急态 → 自动恢复（手动暂停最容易白烧周期）
//! * 写操作**不暂停**（通配符补丁与复制人位置无关，不需要稳定快照）
//! * 紧急暂停有冷却，避免"暂停↔恢复"来回打架把计划饿死

use std::time::{Duration, Instant};

use oni_mcp::{Catalog, McpSession};
use serde_json::Value;

use crate::decide::{decide, load_plan, Decision};
use crate::exec::execute;
use crate::judge::{judge, Judgment, Verdict};
use crate::sense::{sense, Snapshot};
use crate::trend::{compute, load_samples, record};

pub struct AgentConfig {
    pub interval_secs: u64,
    pub plan_path: String,
    /// 紧急暂停后的冷却秒数（冷却期内只记录，不重复暂停）
    pub pause_cooldown: u64,
    /// 「施工窗口」秒数：只有在这个窗口里游戏才跑；窗口结束立刻冻回去
    pub work_window_secs: u64,
    /// 单轮最多执行几个目标
    pub max_actions_per_cycle: usize,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            interval_secs: 10,
            plan_path: "plan.json".into(),
            pause_cooldown: 600,
            work_window_secs: 90,
            max_actions_per_cycle: 1,
        }
    }
}

/// 控制文件路径：写入秒数即请求开一个施工窗口（由代理拥有，到点自动冻回）
pub fn window_request_path() -> std::path::PathBuf {
    oni_mcp::state_dir().join("open_window")
}

/// **hold 标记**：主 agent 正在改代码/深挖问题时写它。
/// 守护进程看到它就**只观察不自动恢复**，等主 agent 写 `open_window` 或删掉它才动。
///
/// 为什么需要：
///   主 agent 修 bug 时游戏若继续跑，局势会在看不见的地方变化 ——
///   修完代码一看，殖民地已经死了（a past failure）。hold 让"修复期"也保持观察权。
pub fn hold_path() -> std::path::PathBuf {
    oni_mcp::state_dir().join("hold")
}

/// 写 hold 标记（主 agent 开始改代码时调用）。
pub fn write_hold(reason: &str) -> std::io::Result<()> {
    let path = hold_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, reason)
}

/// 清除 hold 标记（主 agent 改完代码后调用）。
pub fn clear_hold() {
    let _ = std::fs::remove_file(hold_path());
}

/// 是否处于 hold 状态。
pub fn hold_active() -> bool {
    hold_path().exists()
}

/// ★ 动作指纹：从 exec 结果里提取"这次到底对哪个目标动了手"。
///
/// 为什么需要指纹而不是只按节点计：节点级计数看得见"这个节点失败"，
///   看不见"代理在同一格上反复下同样的工单"（实测：厕所每轮挖同一格，
///   那格早已挖开 → 无限循环）。指纹让循环可被识别、被上报、被换方案。
///
/// 覆盖的 detail 形态（exec.rs 的稳定文案）：
///   "已下挖掘工单 @(x,y)" / "已放置 Prefab @(x,y)" / "落点 (x,y)" / "先挖 (x,y)"
pub fn action_fingerprint(r: &crate::exec::ActionResult) -> Option<String> {
    let d = r.detail.as_str();
    // 优先取"@坐标"（工单实际落点），其次取"落点 (x,y)"，再取"(x,y)"首个坐标对。
    let pick = |s: &str| -> Option<(i32, i32)> {
        let mut it = s.match_indices('(');
        while let Some((i, _)) = it.next() {
            let rest = &s[i + 1..];
            if let Some(j) = rest.find(')') {
                let inner = &rest[..j];
                let mut it2 = inner.split(',');
                if let (Some(a), Some(b)) = (it2.next(), it2.next()) {
                    if let (Ok(x), Ok(y)) = (a.trim().parse::<i32>(), b.trim().parse::<i32>()) {
                        return Some((x, y));
                    }
                }
            }
        }
        None
    };
    // "@(x,y)" 优先：它是**真正下的工单位置**
    let coord = if let Some(at) = d.find("@(") {
        pick(&d[at + 1..])
    } else if let Some(lp) = d.find("落点 (") {
        pick(&d[lp + 3..])
    } else {
        pick(d)
    }?;
    Some(format!("{}@{},{}", r.kind, coord.0, coord.1))
}

/// ★ 结构化失败原因分类：把 exec 的自由文本 detail 归为稳定的类别。
/// 慢系统据此诊断「为什么失败」，而不是盲目换目标名字
/// （实测 oxygen 连续 8 次失败 = "语义目标找不到"，却一直被重试）。
pub fn classify_fail(detail: &str) -> String {
    let d = detail;
    if d.contains("找不到") && (d.contains("语义目标") || d.contains("地图")) { "no_target".into() }
    else if d.contains("研究锁") { "research_locked".into() }
    else if d.contains("缺建材") || d.contains("材料") && d.contains("不可用") { "no_material".into() }
    else if d.contains("预检未通过") { "preflight".into() }
    else if d.contains("落地失败") || d.contains("dry-run 未通过") { "rejected".into() }
    else if d.contains("落点") || d.contains("建造区域") { "no_placement".into() }
    else if d.contains("无法解析") { "bad_goal".into() }
    // ★ 零进展（挖不动/无可下单曲）——单独归类：它既不是"找不到目标"也不是预检失败，
    //   而是"目标存在但当前物理条件推不动"（前沿全需梯子、已有标记等）。
    //   旧版把它归进 unknown → 慢系统看不出"该换方向"，只会重试同一个目标。
    else if d.contains("无可下单曲") || d.contains("零进展") || d.contains("推不动") { "no_progress".into() }
    else { "unknown".into() }
}

/// 读取并消费一次施工窗口请求。
pub fn read_window_request() -> Option<u64> {
    let path = window_request_path();
    let text = std::fs::read_to_string(&path).ok()?;
    let secs: u64 = text.trim().parse().ok()?;
    let _ = std::fs::remove_file(&path);
    Some(secs)
}

pub struct Agent {
    pub session: McpSession,
    pub catalog: Catalog,
    pub cfg: AgentConfig,
    paused_by_us: bool,
    pause_until: Instant,
    last_danger: String,
    /// 当前施工窗口的截止时间（None = 不在窗口内，游戏应保持暂停）
    window_until: Option<Instant>,
    /// 当前这一段运行是代理自己开的施工窗口（到期由代理冻回）
    window_owned_by_us: bool,
    /// 连续全员空闲的轮数
    idle_streak: usize,
    /// Per-dupe consecutive truly-idle cycles (name → count). Enables AI to see
    /// "someone has nothing to do" before the whole colony stalls.
    per_dupe_idle: std::collections::HashMap<String, usize>,
    /// Consecutive cycles with pending orders but nobody working (dead-order detector).
    idle_streak_unworked: usize,
    /// 空跑停机标记：**置位后不得自动恢复**
    idle_stopped: bool,
    /// 窗口结束冻回标记：**同样不得自动恢复**（"结束即暂停"是硬要求）
    frozen: bool,
    /// **工作节律自持**：代理自己冻回后，按「冻 20s → 跑 120s」自动续窗。
    ///
    /// 为什么需要：窗口到期后代理按纪律冻回暂停、等待**新的窗口请求**，
    ///   而请求通常来自主 agent —— 于是无人值守时殖民地**全程冻着**（白烧一小时，
    ///   氧气 213kg、砂岩 5080kg 都在，人却闲着）。
    ///   只有"窗口正常到期"才置位；"动作失败且无待办"（真没活干）**不**置位，
    ///   否则会变成"停→恢复→再停"的空转烧周期。
    auto_rhythm: bool,
    /// 下次自动开窗的时刻（自持节律用）
    resume_at: Option<Instant>,
    /// 连续自持次数（上限保护：即使误判为可自持，也不会长期越权暂停用户的游戏）
    rhythm_streak: usize,
    /// 上一轮的人口（用来发现"有人没了"）
    last_pop: usize,
    /// 保命节点连续失败次数
    survival_failures: usize,
    /// ★ 动作级循环账本：动作指纹 → 连续出现次数。
    ///
    /// 为什么需要：节点级计数（action_failures / stuck_streak）只看得见"这个节点失败了"，
    ///   看不见"代理在同一格上反复下同样的工单"（实测厕所：每轮挖 (115,194)，
    ///   而那格早已挖开 → 无限循环，厕所永远建不成）。
    /// 命中阈值 → 不再重复该动作，改为上报慢系统换方案（契约：慢系统处理策略性死循环）。
    action_loops: std::collections::HashMap<String, usize>,
    /// **卡住去抖计数**：同一 kind 连续命中几轮（瞬时状态不升级）。
    ///
    /// 为什么需要：
    ///   「复制人在不可呼吸的格子里」是**瞬时**状态 —— 复制人可能正在
    ///   **前往**安全区（差事="前往"），下一秒就离开了。
    ///   误报 → 冻结世界 → 停手 → 白白中断施工。
    ///   正解：**连续 2 轮**命中同一 kind 才升级；中断即清零。
    stuck_streak: std::collections::HashMap<String, usize>,
    /// **空间不足**类失败次数（找不到建造落点）→ 布局决策属于慢系统。
    space_failures: usize,
    /// **事件驱动重规划**的冷却（秒）：避免每轮都重规划。
    ///
    /// S5：计划不该是一成不变的 ——
    ///   人口下降 / 氧气告急 / 节点连续失败时，要**按新的世界事实重新生成**。
    /// 上一轮的进展指纹（用来识别"卡住"）
    last_fingerprint: String,
    /// 上一轮结束时世界是否在跑（用来判断"这一轮世界有没有真的动过"）。
    /// 世界没跑 → 指纹必然不变 → 那不是"卡住"，不能计入零进展熔断（）。
    left_running: bool,
    /// 连续无进展的轮数
    no_progress: usize,
    /// 连续**动作明确失败**的次数
    ///
    /// 与 no_progress 的区别：no_progress 要求"世界跑过但没变化"；
    ///   这里是"动作自己报错"（找不到落点/材料不足/被研究锁）。
    ///   失败后代理会冻回暂停 → 世界不跑 → no_progress 永远不计数 → 必须单独记。
    action_failures: usize,
    /// 紧急态占空比：冻 20s → 跑 120s → 再冻 …… 让殖民地能自救，
    /// 又保证每个周期都有一个"世界冻结"的快照供主 agent 判断。
    danger_cycle: bool,
    pub cycles: u64,
    /// 上一轮的周期号（回滚检测：周期回退 = 读档/重开同一局 → 清局内状态）
    last_cycle: i64,
}

/// B: 从目标锁表里剔除最容易卡死的两类（surface / open_chamber）。
/// 抽成纯函数以便单元测试：必须只删这两类，其它锁（食材/元素）一律保留。
fn strip_stuck_locks(v: &mut Value) -> usize {
    let Some(obj) = v.as_object_mut() else { return 0; };
    let before = obj.len();
    obj.remove("surface");
    obj.remove("open_chamber");
    before - obj.len()
}

impl Agent {
    pub fn new(session: McpSession, catalog: Catalog, cfg: AgentConfig) -> Self {
        Self {
            session,
            catalog,
            cfg,
            paused_by_us: false,
            pause_until: Instant::now(),
            last_danger: String::new(),
            window_until: None,
            window_owned_by_us: false,
            idle_streak: 0,
            per_dupe_idle: std::collections::HashMap::new(),
            idle_streak_unworked: 0,
            idle_stopped: false,
            frozen: false,
            danger_cycle: false,
            auto_rhythm: false,
            resume_at: None,
            rhythm_streak: 0,
            last_pop: 0,
            survival_failures: 0,
            space_failures: 0,
            stuck_streak: std::collections::HashMap::new(),
            action_loops: std::collections::HashMap::new(),
            last_fingerprint: String::new(),
            left_running: false,
            no_progress: 0,
            action_failures: 0,
            cycles: 0,
            last_cycle: 0,
        }
    }

    /// 连续动作失败多少次 → 暂缓该节点（换方案）。
    ///
    /// 为什么是 2 而不是 3：建造失败往往是**结构性问题**（空间不够 / 被研究锁），
    ///   不会因为多试几次就好 → 快速暂缓比死磕 3 轮更省时间。
    ///   熔断阈值是 3（因为挖掘可能只是慢），动作失败阈值是 2（因为建造是确定性的）。
    const ACTION_FAIL_MAX: usize = 2;

    /// 同一动作指纹连续出现多少次 → 判定执行层循环（升级慢系统 + 暂缓节点）。
    ///
    /// 为什么是 3：挖掘/建造要复制人真的走过去动手，2 轮可能是正常的慢；
    ///   3 轮还在同一格 → 几乎必然是"选点/判据不成立"或"那格早已挖开"（实测厕所）。
    const ACTION_LOOP_MAX: usize = 3;

    /// 暂缓节点的冷却期（轮）：暂缓后至少隔这么多轮才允许自动恢复。
    ///
    /// 为什么需要：没有冷却 → 暂缓 → 下一轮前置仍满足 → 立刻恢复 → 又失败
    ///   又暂缓…… 代理在同一节点上空转。
    const DEFER_COOLDOWN: u64 = 8;

    /// 实时查游戏是否暂停（读 oni://colony/status，该 URI 走 1s TTL 的秒级新鲜度）。
    ///
    /// 用途：**兜底复核**快照的 paused 判定。
    ///   快照是批量采集的，采集与判定之间有秒级间隔；本函数在判定点重新读一次
    ///   （1s TTL → 判定时基本必读实时值），避免用陈旧状态决定"世界在不在跑"。
    fn game_paused(&self) -> bool {
        self.session
            .read_json("oni://colony/status")
            .ok()
            .and_then(|v| {
                let paused = v.get("isPaused").and_then(|b| b.as_bool()).unwrap_or(false);
                let speed = v.get("gameSpeed").and_then(|s| s.as_f64()).unwrap_or(-1.0);
                // 任一信号说停 → 判停（保守：宁可不下工单，也不可在停着的世界里下单）
                Some(paused || (speed >= 0.0 && speed <= 0.0))
            })
            .unwrap_or(false)
    }

    /// A: 工单积压 + 全员闲看门 —— 在**执行闸门之前**运行。
    ///
    /// 为什么要提前：hook 在每次用户消息时暂冻世界，旧代码在闸门之后才观测，
    /// AI 思考期间世界被冻、账本被清 → 永远看不到"有活没人干"。
    /// 账本只在世界真跑时累加，暂冻期保留；达阈值先做确定性自救（B），
    /// 仍无解才冻结 + 上报 AI 慢系统。
    fn watch_backlog_idle(&mut self, snap: &Snapshot, report: &mut CycleReport) {
        let backlog = snap.pending_orders.unwrap_or(0) > 0;
        let everyone_idle = !snap.dupes.is_empty()
            && snap.truly_idle_count() == snap.dupes.len();
        // 伪忙碌卡死（效果判据，不看 chore 名字）：
        //   ONI 的"寻找安全区域/恢复呼吸"被官方 workIdleCount 算作"在工作"，
        //   stuck.rs 也把它列为 SELF_RESCUING —— 但当**全员聚集在同一格**且工单积压时，
        //   这不是自救，是集体卡死（寻路反复失败）。比真空闲更糟：chore 名骗过了两层判断。
        let all_clustered = snap.dupes.len() > 1
            && snap.dupes.iter().all(|d| d.x == snap.dupes[0].x && d.y == snap.dupes[0].y);
        let all_self_rescuing = !snap.dupes.is_empty() && snap.dupes.iter().all(|d| {
            let c = d.chore.as_str();
            c.contains("寻找安全") || c.contains("恢复呼吸") || c.contains("待命")
        });
        let pseudo_idle = all_clustered && all_self_rescuing;
        let everyone_unproductive = everyone_idle || pseudo_idle;
        if backlog && everyone_unproductive {
            // 暂停快照是**冻结的稳定状态**（世界没动），"全员闲+工单积压"
            // 就是真实死锁信号 → 不需要多轮去抖，直接拉到阈值。
            // 运行期间才需要 2 轮去抖，避免把"正走向工单"误判空闲。
            if snap.paused {
                self.idle_streak_unworked = self.idle_streak_unworked.max(2);
            } else {
                self.idle_streak_unworked += 1;
            }
        } else if !snap.paused {
            self.idle_streak_unworked = 0;
        }
        if backlog && everyone_unproductive && self.idle_streak_unworked >= 2 {
            // ★ 确定性自救 0：先撤掉**没人能执行**的挖掘工单。
            //   游戏的差事队列里，全员被拒的单子会一直占着位置，
            //   让"工单积压 + 全员闲"永远自愈不了 —— 这就是死工单自愈的第一环。
            //   判据是事实（模型 + 官方可达集），不需要慢系统决策。
            let dead = crate::exec::sweep_dead_dig_orders(&self.session, Some(&self.catalog));
            if !dead.is_empty() {
                self.idle_streak_unworked = 0;
                report.notes.push(format!(
                    "[自救] 撤销 {} 条没人能执行的挖掘工单 {:?}（复制人走不到 → 游戏会拒收）",
                    dead.len(), dead
                ));
                return;
            }
            // B: 确定性自救 —— 卡住的目标锁（surface/open_chamber）先清掉换列，
            //    只有换列也救不回来（第二次达阈值）才上报 AI。
            let cleared = self.clear_stuck_goal_locks();
            if cleared > 0 {
                self.idle_streak_unworked = 0;
                report.notes.push(format!(
                    "[自救] 工单积压+全员闲 → 清除 {} 个卡住的目标锁，下一轮换列重算（不打扰 AI）",
                    cleared
                ));
                return;
            }
            let detail = format!(
                "{} work orders pending but all {} dupes idle — dead orders or unreachable target?",
                snap.pending_orders.unwrap_or(0), snap.dupes.len());
            // ★ 契约：工单积压+全员闲只上报慢系统，不冻结世界（保命/执行不阻塞）。
            self.idle_streak_unworked = 0;
            // 结构化诊断：快系统提供卡点类型/目标/可达区/候选，慢系统**选**而非手算坐标。
            let target = self.locked_goal_target();
            let mut candidates = vec![
                crate::escalate::Candidate {
                    id: "dig_access".into(), action: "dig_access_path".into(),
                    target, note: "沿可达区朝目标铺梯子/挖通路".into(),
                },
                crate::escalate::Candidate {
                    id: "cancel_orders".into(), action: "cancel_dead_orders".into(),
                    target: None, note: "撤销够不到的工单，让引擎重规划".into(),
                },
                crate::escalate::Candidate {
                    id: "switch_goal".into(), action: "switch_goal".into(),
                    target: None, note: "改目标（如断粮时先采食），放弃当前卡点".into(),
                },
            ];
            // 断粮时把"改采食"标为首选（快系统算得出的确定性事实）。
            if snap.food_kcal() < 400.0 * snap.dupes.len() as f64 * 5.0 {
                candidates[2].note = "★首选：食物续航<5周期，先采食再换气".into();
            }
            let context = crate::escalate::EscContext {
                obstacle: "dead_orders_unreachable".into(),
                goal: "surface/open_chamber".into(),
                target,
                reach_bounds: snap.reach_bounds,
                candidates,
            };
            let _ = crate::escalate::raise_struct(
                crate::escalate::Kind::NoProgress, "", detail.clone(),
                "AI: pick a candidate — dig access / cancel dead orders / switch goal".into(),
                crate::escalate::Ctx { cycle: snap.cycle, dupes: snap.dupes.len(),
                    oxygen_kg: snap.oxygen_kg(), food_kcal: snap.food_kcal(), ..Default::default() },
                context,
            );
            report.notes.push(format!("[上报AI] 工单积压+全员闲(带结构化候选)：{detail}"));
        }
    }

    /// 读当前卡住的目标锁坐标（surface/open_chamber），作为结构化诊断的 target。
    fn locked_goal_target(&self) -> Option<(i32, i32)> {
        let path = oni_mcp::save::save_file("goal_lock.json");
        let text = std::fs::read_to_string(path).ok()?;
        let v: Value = serde_json::from_str(&text).ok()?;
        for key in ["surface", "open_chamber"] {
            if let Some(g) = v.get(key) {
                let x = g.get("x")?.as_i64()? as i32;
                let y = g.get("y")?.as_i64()? as i32;
                return Some((x, y));
            }
        }
        None
    }

    /// B: 清除卡住的目标锁，让 PLANNER 下一轮换列重算。
    /// 工单积压 + 全员闲本身就是"当前锁定列走不通"的确定性证据；
    /// 清掉后若恢复推进则无需上报，若仍卡住第二次达阈值才交 AI。
    fn clear_stuck_goal_locks(&self) -> usize {
        let path = oni_mcp::save::save_file("goal_lock.json");
        let Ok(text) = std::fs::read_to_string(&path) else { return 0; };
        let Ok(mut v) = serde_json::from_str::<Value>(&text) else { return 0; };
        let cleared = strip_stuck_locks(&mut v);
        if cleared > 0 {
            let _ = std::fs::write(&path, serde_json::to_string_pretty(&v).unwrap_or_default());
        }
        cleared
    }

    fn pause(&mut self, why: &str) {
        let _ = self.session.call_text(
            "game_control",
            serde_json::json!({ "domain": "speed", "action": "pause", "task": format!("代理: {why}") }),
        );
        self.paused_by_us = true;
        self.left_running = false;
        self.pause_until = Instant::now() + Duration::from_secs(self.cfg.pause_cooldown);
        // 暂停所有权：代理暂停 → 落盘 owner=Agent，跨重启保留
        crate::control::pause(crate::control::Owner::Agent, why);
        eprintln!("⛔ 已暂停游戏：{why}");
    }

    fn resume(&mut self, why: &str) {
        // 暂停所有权：只有 agent 自己暂停的世界才能由 agent 恢复。
        //   若 owner=User（hook/用户手动）却被 agent 恢复 = 越权 → 拒绝。
        if !crate::control::resume(crate::control::Owner::Agent) {
            eprintln!("⛔ 代理恢复被拒（owner 不是 agent）：{why}");
            return;
        }
        let _ = self.session.call_text(
            "game_control",
            serde_json::json!({ "domain": "speed", "action": "resume", "task": format!("代理: {why}") }),
        );
        self.paused_by_us = false;
        self.left_running = true;
        eprintln!("▶ 已恢复游戏：{why}");
    }

    /// 跑一轮完整循环，返回本轮摘要。
    ///
    /// 流程顺序：
    ///   1. 采快照（只读，永远允许）
    ///   2. **判运行状态**：世界在不在跑？（Running / Frozen / Held / UserPaused）
    ///   3. 世界不跑 → **只观察直接返回**（不下工单、不计进展、不熔断）
    ///   4. 决策（实时优先级调度可抢占静态计划）→ 执行
    ///   5. 执行**之后**再判空闲 → 没活才冻回暂停
    ///
    ///  的完整链条（旧逻辑为什么会自毁）：
    ///   旧逻辑在"执行**之前**"用**上一轮**的 pending_orders 判空闲
    ///   → 这一轮刚下的第一张工单还没进队列 → 被当成空闲 → 立刻冻回暂停
    ///   → 那张工单永远不会被执行（游戏是暂停的）
    ///   → 指纹永远不变 → 连续 3 轮"零进展" → **误熔断好节点**
    ///   → 依赖图死锁 → 殖民地彻底停摆。
    pub fn cycle(&mut self) -> CycleReport {
        self.cycles += 1;
        // ---- 换存档检测（每轮最先做）----------------------------------
        // 指纹变了 = 换了一局 → 旧计划归档 + 计划按模板重置 +
        // 局内状态（目标锁/侦察/趋势样本/实时快照）切到新存档的目录。
        // 不做这一步 → 新局继承旧局的 plan（全 done）→ 代理空转几十分钟。
        let mut save_switch: Option<String> = None;
        if let Some(sw) = crate::saveswitch::ensure(&self.session, &self.cfg.plan_path) {
            let old_fp = sw
                .old
                .as_ref()
                .map(|o| o.fingerprint.clone())
                .unwrap_or_else(|| "无记录".into());
            save_switch = Some(format!(
                "🔀 检测到换存档（{} → {}）→ 旧计划已归档到 saves/{}/plan.json，本局计划按模板重置",
                old_fp, sw.new.fingerprint, old_fp
            ));
        }
        let snap = sense(&self.session);
        // 落盘共享快照：守护进程当**唯一采集者**，主 agent 读它
        //   MCP 只有一条通道（不再两边各采一遍），两边看到的也是同一份真值。
        let _ = crate::live::publish(&snap);
        let _ = record(&snap);
        let samples = load_samples(200);
        let trend = compute(&samples, &snap);
        let mut judgment = judge(&snap, Some(&trend));
        // 人口下降 = 有人死了/倒下了 → 立刻升级成红线
        let pop_dropped = self.last_pop > 0 && snap.dupes.len() < self.last_pop;
        if pop_dropped {
            judgment
                .danger_keys
                .push(format!("population_dropped:{}", self.last_pop - snap.dupes.len()));
            judgment.dangers.push(format!(
                "人口从 {} 掉到 {} —— 有人没了",
                self.last_pop,
                snap.dupes.len()
            ));
        }
        self.last_pop = snap.dupes.len();

        let mut report = CycleReport {
            cycle: snap.cycle,
            verdict: judgment.verdict,
            snap: snap.clone(),
            judgment: judgment.clone(),
            acted: false,
            node: None,
            notes: Vec::new(),
        };
        if let Some(msg) = save_switch {
            report.notes.push(msg);
        }

        // ---- S5：事件驱动重规划 ------------------------------------
        //   计划不该一成不变：人口下降 / 氧气告急 / 节点连续失败时，
        //   要**按新的世界事实重新生成依赖图**（换策略族），而不是继续走老路。
        //   冷却 60s，避免每轮都重算（采集事实要打多次 MCP）。
        // =====================================================================
        // 2) 运行状态机：这一轮世界在不在跑
        //    铁律：**世界不跑 ⇒ 不下工单、不累计进展、不熔断。**
        // =====================================================================
        // ★ 世界在不在跑：**实时真值**，不是过期快照。
        //   判据三重信号（同一份 oni://colony/status，1s TTL 秒级新鲜度）：
        //     ① isPaused = true → 停（暂停按钮被按下）
        //     ② gameSpeed <= 0 → 停（速度 0 等于没跑；mod 出现过
        //        isPaused=false 但 gameSpeed=0.0 的组合，只看 ① 会漏判）
        //     ③ 任一说停 → 判停（保守：宁可少下一轮工单，也不可在世界停着时下单）
        //   为什么不能只信快照里的 paused：拿过期值判定 → 代理以为世界在跑 →
        //   继续下工单 → 游戏实际停着、工单永不执行 → 指纹不变 → 判零进展 →
        //   误熔断好节点（契约 §9 铁律：世界不跑 ⇒ 不下工单、不计进展、不熔断）。
        let speed_stopped = snap.game_speed >= 0.0 && snap.game_speed <= 0.0;
        let mut observed_running = !snap.paused && !speed_stopped;
        // ★ 实时复核：快照说"在跑"时，再查一次实时状态兜底。
        //   快照是批量采集的，采集完成到判定执行之间有秒级间隔；
        //   这段时间里游戏可能被暂停（agent 自己冻 / 用户按空格 / 窗口到期冻回）。
        //   用陈旧的 false 去判定 → 以为世界在跑 → 继续下单 → 工单永不执行。
        //   复核读的是同一条 URI，但走 1s TTL → 判定时基本必读实时值；
        //   且并发读仍由 singleflight 合并，不会打爆 mod 端点。
        if observed_running && self.game_paused() {
            observed_running = false;
            report.notes.push(
                "🕐 实时复核：游戏实际已暂停（快照判定滞后）→ 本轮按世界停着处理，不下工单".into(),
            );
        }
        // 暂停所有权：唯一的用户停止信号是 hold（AI 总开关关），
        //   在下方 hold_active() 早退处理。其它 owner 一律由代理自主恢复。
        //   （旧逻辑把 owner=User 判成"用户暂停→绝不恢复"，与 AI 总开关重复且致死锁。）
        let ctl = crate::control::read();
        // 上一轮结束时世界在跑吗？→ 这一轮世界有没有真的动过（决定能不能比指纹）
        let world_advanced = self.left_running;
        self.left_running = observed_running;
        let danger_key = judgment.danger_keys.join(";");
        let mut can_execute = false;

        if hold_active() {
            // hold：主 agent 正在改代码 → 冻世界，只观察
            if observed_running {
                self.pause("hold：主 agent 正在处理，冻结世界");
            }

        // ---- 回滚检测：周期回退 = 读档/重开同一局 → 清局内决策上下文 ----
        //   不清的话：上一轮（往往以全灭告终）的 decisions/goal_outcomes/ai_goals/
        //   circuit_breaks 全部残留 → LLM 决策窗口被污染、产氧目标开局即熔断
        //   （实测缺口：回滚后 oxygen fail=4 的熔断记录直接封锁新局的产氧目标）。
        //   阈值 2：读档通常回退多个周期；正常跨周期是 +1，不会误触。
        if self.last_cycle > 0 && (snap.cycle as i64) + 2 <= self.last_cycle {
            crate::saveswitch::clear_save_state();
            report.notes.push(format!("🔁 检测到回滚（周期 {} → {}）→ 已清局内决策上下文（LLM 从零重排）", self.last_cycle, snap.cycle));
        }
        self.last_cycle = snap.cycle as i64;            self.window_until = None;
            self.window_owned_by_us = false;
            report.notes.push(
                "⏸ hold 模式：主 agent 正在处理 → 代理只观察，不自动恢复（等 oni release）".into(),
            );
            return report;
        }

        if judgment.verdict == Verdict::Danger {
            // 紧急态用**占空比**，不是"一暂停到底"：
            //   暂停只是冻结倒计时；人闷在 CO₂ 坑里，暂停并不能救他们。
            //   正确节奏：冻 20s（给主 agent 一个世界静止的快照）→ 跑 120s（让人能行动）→ 再冻。
            //   历史上"一暂停到底"的结果：三个人全部失去行动能力、一人死亡。
            self.danger_cycle = true;

            // 人口为 0 = 殖民地已死，**紧急态占空比在这里帮不上忙**。
            //   没有活人能靠"跑 120s"自救，
            //   占空比只会白烧游戏周期 + 持续唤醒代理。
            //   正确处置：保持暂停、只观察，等人工回滚存档。
            if crate::judge::is_no_population(&judgment.danger_keys) {
                if observed_running {
                    self.pause("人口为 0：殖民地已死，冻结世界（不白烧周期）");
                }
                self.window_until = None;
                self.window_owned_by_us = false;
                self.danger_cycle = false;
                report.notes.push(
                    "☠️ 人口为 0：无人可自救 → 紧急态占空比停用，保持暂停（不白烧周期）；需要回滚存档"
                        .into(),
                );
                return report;
            }

            let now = Instant::now();
            let deadline = self.window_until.unwrap_or(now);
            if self.paused_by_us {
                // 冻结期：到点就恢复运行 120s，让人能爬出坑、完成施工
                if now >= deadline {
                    self.resume("紧急态：冻结期结束 → 运行 120s 让人行动");
                    self.window_until = Some(now + Duration::from_secs(120));
                    self.window_owned_by_us = true;
                    can_execute = true;
                    report.notes.push("▶ 紧急态：恢复运行 120s（让人能爬出 CO₂ 坑 / 完成施工）".into());
                } else {
                    report.notes.push(format!(
                        "🔒 紧急态冻结中（剩 {}s，随后自动跑 120s）",
                        (deadline - now).as_secs()
                    ));
                }
            } else if observed_running && now >= deadline {
                // 运行期：到点冻 20s，给主 agent 一个世界静止的快照
                self.pause(&danger_key);
                self.window_until = Some(now + Duration::from_secs(20));
                self.window_owned_by_us = true;
                report.notes.push("🔒 紧急态：冻 20s 取静止快照 → 随后自动跑 120s".into());
            } else if observed_running {
                can_execute = true;
                report.notes.push(format!("▶ 紧急态运行中（剩 {}s）", (deadline - now).as_secs()));
            } else {
                // 紧急态 + 游戏暂停：开一个**最短窗口**让人有机会恢复，然后立刻冻回。
                //   旧逻辑是"完全不跑，只报警" —— 但这在真紧急态下会**阻止救援**：
                //   downed 的复制人只能靠环境改善（CO₂ 稀释 / 污染水清理）恢复，
                //   而环境改善需要**游戏在跑**（工单要能被执行、人要能行动）。
                //   三人全倒时若游戏一直冻着 → 永远恢复不了 = 死局。
                //   边界：用户手动暂停依然被尊重 —— 只开最短窗口，到点立刻冻回。
                const RESCUE_TICK: u64 = 30;
                self.window_until = Some(Instant::now() + Duration::from_secs(RESCUE_TICK));
                self.window_owned_by_us = true;
                let _ = self.session.call_text(
                    "game_control",
                    serde_json::json!({ "domain": "speed", "action": "resume",
                                       "task": "代理: 紧急态救援窗口（30s 后立即冻回）" }),
                );
                self.left_running = true;
                can_execute = true;
                report.notes.push(format!(
                    "⛔→⏱ 紧急态 + 用户暂停：开 {RESCUE_TICK}s 最短救援窗口（让人能恢复/执行工单），到点立即冻回"
                ));
            }
            self.last_danger = danger_key;
        } else if observed_running {
            // ---- 世界在跑 ----
            self.danger_cycle = false;
            self.frozen = false;
            self.idle_stopped = false;
            self.paused_by_us = false;
            can_execute = true;
            if self.window_owned_by_us {
                match self.window_until {
                    Some(deadline) if Instant::now() >= deadline => {
                        // 窗口到期但**有活没干完** → 续窗，别把正在干的活冻死
                        let busy = snap.pending_orders.unwrap_or(0) > 0 || snap.under_construction > 0;
                        if busy {
                            self.window_until =
                                Some(Instant::now() + Duration::from_secs(self.cfg.work_window_secs));
                            report.notes.push(format!(
                                "▶ 窗口到期但仍有活 → 续窗 {}s（让复制人干完）",
                                self.cfg.work_window_secs
                            ));
                        } else {
                            self.pause("施工窗口结束 → 回到暂停");
                            self.paused_by_us = false;
                            self.frozen = true;
                            self.window_until = None;
                            self.window_owned_by_us = false;
                            can_execute = false;
                            // 工作节律自持：窗口正常到期 → 冻 20s 后自动续窗。
                            //   无人值守时不再需要外部窗口请求（此前会白烧一小时）。
                            self.auto_rhythm = true;
                            self.resume_at = Some(Instant::now() + Duration::from_secs(20));
                            report.notes.push(
                                "🔒 施工窗口结束 → 冻回暂停（20s 后自持续窗；用户手动暂停则不续）".into(),
                            );
                        }
                    }
                    Some(_) => {
                        report.notes.push(format!(
                            "▶ 施工窗口中（剩 {}s）",
                            (self.window_until.unwrap() - Instant::now()).as_secs()
                        ));
                    }
                    None => {
                        self.window_until =
                            Some(Instant::now() + Duration::from_secs(self.cfg.work_window_secs));
                    }
                }
            } else {
                report.notes.push(
                    "▶ 游戏在跑（用户恢复的）→ 代理只观察与执行，不代为暂停".into(),
                );
            }
        } else {
            // ---- 游戏是暂停的 ----
            self.danger_cycle = false;
            // 工作节律自持：代理自己冻回后，按「冻20s → 跑120s」自动续窗。
            //   边界（必须严守）：
            //   - 只有"代理自己冻的"(frozen) 才自持；hold 由用户/主 agent 掌控，不自持
            //   - "真没活干"（idle_stopped / 动作失败且慢系统无待办目标）→ 不自持
            //     （动作失败但 ai_goals_pending 非空 = 有活 → 走自持续窗重试）
            //   - 有 hold 标记（主 agent 改代码中）→ 上面已早退，不会走到这里
            //   - 连续自持上限 RHYTHM_MAX：即使误判也不会长期越权暂停用户的游戏
            const RHYTHM_MAX: usize = 4;
            // ★ 有未完成的长线目标/在建工单 → 视为"有活"，续窗不消耗自持额度，
            //   让竖井/研究这类跨多窗口任务能持续到完成（原来4次后必停 = 长线任务夭折）。
            let has_pending_goal = !crate::commands::ai_goals_pending().is_empty()
                || crate::escalate::has_pending();
            let work_pending = has_pending_goal
                || snap.pending_orders.unwrap_or(0) > 0
                || snap.under_construction > 0;
            if work_pending { self.rhythm_streak = 0; }
            let rhythm_due = self.auto_rhythm
                && self.frozen
                && !self.idle_stopped
                && (work_pending || self.rhythm_streak < RHYTHM_MAX)
                && self
                    .resume_at
                    .map(|t| Instant::now() >= t)
                    .unwrap_or(false);
            if rhythm_due {
                self.rhythm_streak += 1;
                self.window_until = Some(Instant::now() + Duration::from_secs(self.cfg.work_window_secs));
                self.window_owned_by_us = true;
                self.frozen = false;
                self.auto_rhythm = false;
                self.resume_at = None;
                self.resume("工作节律自持 → 续开施工窗口");
                can_execute = true;
                report.notes.push(format!(
                    "🔁 工作节律自持：自动续开 {}s 施工窗口（第 {}/{} 次，无需外部窗口请求）",
                    self.cfg.work_window_secs,
                    self.rhythm_streak,
                    RHYTHM_MAX
                ));
            } else if (self.frozen || self.idle_stopped) && work_pending {
                // ★ 冻结中慢系统送来了新目标（ai_goal / 升级请求 / 在建工单）→ 有活就该干，
                //   立即续窗（契约 §7：有活续窗 / 空闲冻回）。否则慢系统在冻结期间入队的
                //   目标会因「冻结只观察」永远不被消费 → 快慢互等死锁（2026-11-24 实测：
                //   目标消费完才冻结，LLM 80s 后入队新目标，冻结态无人开窗，第二层死锁）。
                self.rhythm_streak = 0;
                self.window_until =
                    Some(Instant::now() + Duration::from_secs(self.cfg.work_window_secs));
                self.window_owned_by_us = true;
                self.frozen = false;
                self.idle_stopped = false;
                self.auto_rhythm = false;
                self.resume_at = None;
                self.resume("慢系统有待办目标 → 冻结中续窗执行");
                can_execute = true;
                report.notes.push(
                    "▶ 冻结中慢系统入队新目标 → 续开施工窗口（有活续窗）".into(),
                );
            } else if self.frozen || self.idle_stopped {
                report.notes.push("🔒 已冻回暂停（等新窗口请求）".into());
            } else if self.paused_by_us {
                // 上轮紧急暂停、条件已解除 → 开一个短窗口，而不是一直跑。
                //   注：契约已废除"等慢系统→冻结"与看门狗，暂停只由 hold/紧急占空比/
                //   施工窗口三种情形产生；这里处理的是"危险期结束 → 恢复施工窗口"。
                self.window_until = Some(Instant::now() + Duration::from_secs(self.cfg.work_window_secs));
                self.window_owned_by_us = true;
                self.resume("危急已解除 → 开施工窗口");
                self.last_danger.clear();
                can_execute = true;
                report.notes.push("▶ 危急已解除 → 开施工窗口".into());
            } else {
                // ★ 统一恢复：AI 开启时，游戏的运行/暂停由代理自主决定。
                //   唯一的用户停止信号是 hold（已在上方早退）；
                //   其余 owner（agent/none/user/escalation）的暂停一律视为可恢复，
                //   直接清 owner + resume 游戏 + 开施工窗口，长线任务继续执行。
                crate::control::clear();
                self.frozen = false;
                self.auto_rhythm = false;
                self.idle_stopped = false;
                    self.rhythm_streak = 0;
                self.window_until = Some(Instant::now() + Duration::from_secs(self.cfg.work_window_secs));
                self.window_owned_by_us = true;
                let _ = self.session.call_text(
                    "game_control",
                    serde_json::json!({ "domain": "speed", "action": "resume",
                                       "task": format!("代理: AI自主恢复（原 owner={:?}）开施工窗口", ctl.owner) }),
                );
                self.left_running = true;
                can_execute = true;
                report.notes.push(
                    format!("⏸→▶ 代理自主恢复（原 owner={:?}）→ 开施工窗口，长线任务继续", ctl.owner),
                );
            }
        }

        // ---- 显式施工窗口请求----
        if let Some(secs) = read_window_request() {
            // 用户显式 `oni window` = 授权代理恢复：先清掉 user/hook 的暂停所有权。
            //   清 owner：暂停所有权必须与"谁真的在跑"一致，否则下一轮
            //   读到残留的 owner 会得出错误的运行状态判断。
            crate::control::clear();
            if judgment.verdict != Verdict::Danger {
                self.window_until = Some(Instant::now() + Duration::from_secs(secs));
                self.window_owned_by_us = true;
                self.paused_by_us = false;
                    self.idle_stopped = false;
                self.frozen = false;
                let _ = self.session.call_text(
                    "game_control",
                    serde_json::json!({ "domain": "speed", "action": "resume",
                                       "task": format!("代理: 开 {secs}s 施工窗口") }),
                );
                self.left_running = true;
                can_execute = true;
                report.notes.push(format!("▶ 已按请求开 {secs}s 施工窗口（到点自动冻回暂停）"));
            } else {
                // 紧急态也要能开**短**窗口：暂停只是冻结倒计时（窒息的人不会因为暂停而好转）。
                //   如果这里拒绝开窗，坑里的人就永远出不来 → 死锁。
                let rescue = secs.min(150);
                self.window_until = Some(Instant::now() + Duration::from_secs(rescue));
                self.window_owned_by_us = true;
                self.paused_by_us = false;
                    self.idle_stopped = false;
                self.frozen = false;
                let _ = self.session.call_text(
                    "game_control",
                    serde_json::json!({ "domain": "speed", "action": "resume",
                                       "task": format!("代理: 紧急态开 {rescue}s 短窗口（让人能行动）") }),
                );
                self.left_running = true;
                can_execute = true;
                report.notes.push(format!(
                    "▶ 紧急态开 {rescue}s 短窗口：暂停只冻结倒计时，救人/施工必须让游戏跑起来"
                ));
            }
        }

        // A: 观测前移 —— 空闲看门在闸门之前跑，hook 暂冻世界时也能发现"有活没人干"。
        self.watch_backlog_idle(&snap, &mut report);

        // ★ 契约：不再有"等慢系统→冻结/解冻/看门狗"逻辑。
        //   慢系统只是参谋，从不暂停世界。保留 control owner 的自主恢复（状态机处）。

        // =====================================================================
        // 3) 执行闸门：世界不跑 ⇒ 只观察
        //    （不下工单、不计进展、不熔断 ——  的直接止血点）
        // =====================================================================
        if !can_execute {
            report.notes.push(
                "⏸ 世界没在跑 → 本轮只观察（不下工单、不计进展、不熔断）".into(),
            );
            return report;
        }

        // 人口为 0：殖民地已无人 → 代理完全停手（别在死掉的地图上刷工单）
        //   ⚠ 但必须先确认数据可信：观察源读取失败时 dupes 也会空。
        //     误判成人口 0 → 代理全面停工。
        let dupes_failed = snap.errors.iter().any(|e| {
            e.contains("dupes") || e.contains("todos") || e.contains("status-check")
        });
        if snap.dupes.is_empty() && dupes_failed {
            report.notes.push("⚠ 复制人数据读取失败 → 无法确认人口，继续施工（不当作人口为 0）".into());
        } else if snap.dupes.is_empty() {
            report.notes.push(
                "☠️ 人口为 0 —— 殖民地已无人，代理停止一切操作（需要回滚存档）".into(),
            );
            return report;
        }

        // ---- 殖民地杂务：每轮都把打印舱的奖励领出来 ----
        // 为什么独立于依赖图：玩家绝不会让打印舱的产出烂在舱里，
        // 而它可能产出**唯一的新人**（三人只剩两个）。
        if let Ok(out) = self.session.call_text(
            "building_control",
            serde_json::json!({
                "domain": "side_surface", "surface": "facility", "kind": "printing_pod",
                "action": "claim", "rewardIndex": 0, "confirm": true,
                "task": "代理: 领取打印舱奖励"
            }),
        ) {
            let first = out.lines().find(|l| l.contains("claimable") || l.contains("selectedReward"));
            if let Some(l) = first {
                report.notes.push(format!("🎁 打印舱奖励: {}", l.trim().chars().take(60).collect::<String>()));
            }
        }

        // ---- 救援：有人倒地 → 立刻把他挪到安全的建筑旁（ 的正解）
        //   旧结论"downed 不可救"是错的：mod 有 dupes_control move_to。
        //   倒地的复制人待在污染区**必定死亡**；挪到干净的建筑旁边就能活。
        //   本步骤每轮都做（不依赖依赖图），优先级高于任何施工。
        // 门槛必须是"有人需要救援"，不能只是"有人倒地"。
        //   旧门槛 downed_count()>0 让"呼吸在掉但还站着"的人永远进不了救援逻辑
        //   （ 的修复只做了一半 —— 执行方改了、调用方没改）。
        // S3 执行后复核：回读上一轮移动是否**真的生效**。
        //   「已下令」≠「已生效」：倒地的复制人无法自行寻路 → 移动工单永远不执行。
        //   复核不通过时要**显式报告**，否则会误以为救到了而不去建床/清毒气。
        for line in crate::exec::verify_moves(&self.session) {
            report.notes.push(line);
        }
        if snap.dupes.iter().any(crate::exec::dupe_needs_rescue) {
            let moved = crate::exec::rescue_dupes(&self.session, &snap.dupes);
            if !moved.is_empty() {
                report.notes.push(format!(
                    "⛑ 救援：把倒地的复制人移到安全建筑旁（{}）",
                    moved.join("、")
                ));
            }
        }

        // ---- 执行慢系统的语义目标（慢系统定 goal，快系统 autoact 翻译成工单）----
        //   慢系统把决策 goal 写进独立 ai_goals.jsonl；这里消费并执行。
        //   必须在 wait_slow 停手逻辑**之前**完成，否则 pending_cmd 会因为这条而停手。
        // ★ 紧急闸门：生命红线告急时本轮不消费长线 goal，把工单额度让给紧急抢占
        //   （下方 1063 行的 directive + 1128 行的抢占），避免长线挤占意外处理。
        let urgent_guard = judgment.verdict == crate::judge::Verdict::Danger
            || judgment.danger_keys.iter().any(|k| k.starts_with("breath@") || k == "downed")
            || snap.min_breath().map(|b| b < crate::priority::URGENT_BREATH).unwrap_or(false)
            || (snap.max_bladder() > crate::priority::BLADDER_CRITICAL
                && snap.building("toilets") < snap.dupes.len() as i64);
        // ★ 只跳"扩张类" goal（会和保命抢工单额度）；
        //   保命类（material/build toilet/oxygen/element:X）恰恰是解急症的手段，必须照常执行。
        let is_expansion_goal = |g: &str| {
            matches!(g, "expand" | "scout" | "surface" | "open_chamber")
        };
        {
            // ★ 从独立 ai_goals.jsonl 读（不再从 commands.jsonl 过滤 source）
            let mut ai_goals: Vec<_> = crate::commands::ai_goals_pending();
            if urgent_guard {
                let before = ai_goals.len();
                ai_goals.retain(|c| !is_expansion_goal(c.text.trim()));
                if ai_goals.len() < before {
                    report.notes.push(
                        "🚨 生命红线告急 → 跳过扩张类 goal，保命类 goal 照常执行".into(),
                    );
                }
            }
            for c in ai_goals {
                let goal = c.text.trim();
                if crate::autoact::parse_goal(goal).is_none() {
                    crate::commands::complete_ai_goal(c.id, &format!("无效语义目标: {}", goal));
                    continue;
                }
                if !can_execute {
                    break; // 世界没跑 → 保留，等下一轮
                }
                let action = crate::exec::Action::Auto {
                    goal: goal.to_string(),
                    why: format!("慢系统语义目标: {}", if c.result.is_empty() { "P级决策" } else { &c.result }),
                };
                let out = crate::exec::execute(&self.session, &self.catalog, &[action]);
                let ok = out.ok;
                let detail = out.results.iter()
                    .map(|r| r.detail.as_str()).collect::<Vec<_>>().join("; ");
                // 执行结果写入单一权威来源，供慢系统 history 读取（避免 command.result 被决策理由污染）
                {
                    use std::io::Write;
                    let fp = oni_mcp::save::last_seen()
                        .map(|s| s.fingerprint).unwrap_or_else(|| "unknown".into());
                    let sd = oni_mcp::save::save_dir(&fp);
                    let _ = std::fs::create_dir_all(&sd);
                    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true)
                        .open(sd.join("goal_outcomes.jsonl")) {
                        // ★ 结构化失败原因：从 detail 归类，供慢系统诊断「为什么失败」而非盲目换名字。
                        let fail_kind = if ok { "none".to_string() } else { classify_fail(&detail) };
                        let rec = serde_json::json!({
                            "goal": goal, "ok": ok, "detail": detail, "fail_kind": fail_kind,
                            "ts": std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0),
                        });
                        let _ = writeln!(f, "{}", rec);
                    }
                }
                crate::commands::complete_ai_goal(c.id, &format!("ai_goal[{}] {}：{}", goal,
                    if ok { "已执行" } else { "未成功" }, detail));
                report.notes.push(format!("🎯 慢系统目标[{}]：{} → {}", goal,
                    if ok { "已执行" } else { "未成功" }, detail));
            }
        }

        // ---- 快慢交接（契约 docs/FAST_SLOW_CONTRACT.md）----
        // ★ "等慢系统"不再是暂停/停手理由：escalation 只是上报给慢系统做战略参考，
        //   快系统的保命与执行**永不因此阻塞**。慢系统照常 resolve（清 pending/写静默），
        //   但快系统每轮照常干活——这是"慢系统不审批急症"的核心。
        //   旧的 60 行 waiting_slow 停手逻辑已删除（它曾导致膀胱爆表却没人建厕所）。
        let pending_esc = crate::escalate::has_pending();
        let pending_cmd = !crate::commands::pending().is_empty();
        if pending_esc || pending_cmd {
            if pending_cmd { report.notes.push(crate::commands::summary()); }
            if pending_esc { report.notes.push(crate::escalate::summary()); }
        }

        // ---- 决策 + 执行 ----
        let plan = match load_plan(std::path::Path::new(&self.cfg.plan_path)) {
            Ok(p) => p,
            Err(e) => {
                report.notes.push(format!("计划读取失败: {e}"));
                return report;
            }
        };
        // ★ **复核 done 节点**：done 必须有证据。
        //   计划里的 status=done 可能来自 AI 直接写的计划（未经判据校验）。
        //   实测本局：E1「地表换气」done 但可达上界只有 194（判据要 216）、
        //   B1「床位」done 但 beds=0 → 依赖图以为"换气与床位都完成了"
        //   → 再没有任何节点要求产氧 → 基地慢慢耗尽。
        //   每轮用同一套判据复核，不成立就打回 todo。
        {
            let bad = recheck_done_nodes(&self.cfg.plan_path, &snap);
            if !bad.is_empty() {
                report.notes.push(format!(
                    "🔎 复核 done 节点：{} 的成功判据当前**不成立** → 打回 todo（done 必须有证据）",
                    bad.join(", ")
                ));
            }
        }

        // ★ **开局规划素材**（快慢契约 §4：快系统采事实，慢系统做战略）：
        //   计划不是 AI 生的（模板/机械兜底）→ 采集本局世界事实
        //   （侦察 + 结构化 facts）写盘，供慢系统 LLM 生成开局战略。
        //   旧实现：每局计划来自 plan.template.json（硬编码 playbook）
        //   → 换一张地图（水世界/金属星）节点图就不适用（用户指出的设计缺陷）。
        //   只采**事实**，节点结构由慢系统决定 —— 分工不变。
        if plan.author.as_deref() != Some("ai") {
            let brief_path = oni_mcp::save::save_file("opening_brief.json");
            if !std::path::Path::new(&brief_path).exists() {
                let facts = crate::opening::collect_facts(&self.session, &snap);
                let scout_report = crate::autoact::scout_resources(&self.session, Some(&self.catalog));
                let rec = serde_json::json!({
                    "facts": serde_json::to_value(&facts).unwrap_or_default(),
                    "scout": scout_report,
                    "cycle": snap.cycle,
                    "population": snap.dupes.len(),
                });
                match std::fs::write(&brief_path, serde_json::to_string_pretty(&rec).unwrap_or_default()) {
                    Ok(_) => report.notes.push(
                        "📋 开局规划素材已采集（opening_brief.json）→ 慢系统将依据本局事实生成开局战略".into()),
                    Err(e) => report.notes.push(format!("⚠ 开局规划素材采集失败: {e}")),
                }
            }
        }
        let d: Decision = decide(&plan);
        // 死锁解锁：所有节点被熔断、图停摆时，decide() 复活了一个 blocked 节点。
        //   立刻把它写回 todo 并重新载入 plan，避免"依赖图里没有可执行目标"刷屏。
        if !d.revived.is_empty() {
            for id in &d.revived {
                mark_node_todo(&self.cfg.plan_path, id);
            }
            report.notes.push(format!(
                "🔓 依赖图死锁 → 复活熔断节点 {:?}（熔断是为了换方案，不是让图停摆）",
                d.revived
            ));
        }
        // 保命优先：空气告急 / 有人倒下 → 强制执行 survival_node（产氧/换气），
        //   其它目标一律让位。这条是上一局"三个人"的直接教训。
        // 救援模式：有人倒地时，**普通施工必须让位**。
        let rescue_mode = judgment.danger_keys.iter().any(|k| k == "downed");
        let survival_mode = judgment.danger_keys.iter().any(|k| {
            k.starts_with("air")
                || k.starts_with("no_population")
                || k.starts_with("breath@")
                || k.starts_with("population_dropped")
        });
        // ---- 实时优先级调度----
        // 三次的共同根因：代理看不见"人"。调度器每轮按**当前真实状态**
        // 算"最该做什么"，能**抢占**当前节点。
        let trend_now = compute(&load_samples(200), &snap);
        let directive = crate::priority::direct(
            &snap,
            &judgment,
            trend_now.oxygen_per_cycle,
            trend_now.stress_rising,
            trend_now.oxygen_frozen,
            trend_now.oxygen_cycles_left,
            trend_now.food_cycles_left,
        );
        report.notes.push(format!("📊 调度[{:?}]: {}", directive.level, directive.reason));

        // Semantic decision → freeze + escalate, never act on it locally.
        // The fast system only watches indicators; judgment belongs to the AI slow system.
        // raise 返回 0 = 同类上报在静默期内（不重复打扰 AI）；无论如何快系统继续自主处理。
        if directive.needs_ai {
            // ★ 契约：语义决策只上报慢系统做战略参考，**绝不停手/冻结**。
            //   escalation 自带 category 静默去重（同类急症窗内不重复打扰 AI）；
            //   无论上报结果如何，快系统继续往下走 Urgent 抢占 + 保命执行。
            let category = format!("{:?}", directive.action);
            let esc_id = crate::escalate::raise_categorized(
                crate::escalate::Kind::StrategyMismatch,
                "",
                format!("AI decision required: {}", directive.reason),
                "Slow system FYI: fast system is handling it autonomously".into(),
                crate::escalate::Ctx { cycle: snap.cycle, dupes: snap.dupes.len(),
                    oxygen_kg: snap.oxygen_kg(), food_kcal: snap.food_kcal(),
                    breath: snap.min_breath().unwrap_or(0.0),
                    bladder: snap.max_bladder(), },
                &category,
                crate::escalate::EscContext::default(),
            );
            if esc_id == 0 {
                report.notes.push("🔇 semantic 上报静默期内（不重复打扰 AI）".into());
            } else {
                report.notes.push("📡 semantic 已上报 AI（FYI）；快系统自主处理，不停手".into());
            }
        }

        // 救援模式优先于保命模式：有人倒地时，先救倒下的，再谈产氧/换气。
        // 实时调度优先：Level>=2（紧急/救命）时，**抢占**静态计划。
        let picked_by_scheduler = if directive.level >= crate::priority::Level::Urgent {
            let want = directive.action;
            // ：氧气续航告急时禁止扩张性挖掘 —— 跳过"换气/地表/挖矿"节点，
            //   否则代理越缺氧越挖，暴露地下 CO₂ 让氧气掉得更快（死亡螺旋）。
            //   产氧节点不受此限（它产值不扩张）。
            if directive.forbid_expansion {
                report.notes.push(
                    "🚫 氧气续航告急 → 禁止扩张性挖掘（跳过换气/地表/挖矿节点，只做产氧/救援/食物）".into(),
                );
            }
            plan.nodes
                .iter()
                .find(|n| n.rescue && !n.is_done() && !n.is_blocked())
                .cloned()
                .or_else(|| {
                    plan.nodes
                        .iter()
                        .find(|n| {
                            // 二次修正：
                            //   旧逻辑把 done 节点一律跳过 → T1 建了 1 座厕所就 done，
                            //   但 3 人膀胱告急时**还需要第 2/3 座** → Urgent 喊「建厕所」
                            //   却匹配不到任何节点 → 回落常规计划 → 膀胱涨到 100% → 随地排泄
                            //   → 污染水
                            //   正解：**膀胱告急时允许复活 done 的卫生节点**（条件已重新告急）。
                            let is_toilet_urgent = matches!(
                                want,
                                crate::priority::Action::BuildToilet
                            );
                            if n.is_done() && !is_toilet_urgent {
                                return false;
                            }
                            if n.is_blocked() {
                                return false;
                            }
                            // ★ tag 只取节点的**语义身份**（名称 + 动作的 goal/prefab），
                            //   **不含动作的 why 文案**。
                            //   为什么：why 是给人和 LLM 看的解释文本，里面的词是**偶然**的 ——
                            //   实测 T1「浅层建材自举」的 why 里写了"厕所、梯子、砖块都需要建材"，
                            //   于是 Urgent「建厕所」匹配到了 T1（采建材）而不是 T2（卫生前置：三人厕所）→
                            //   每轮把 T1 复活成 done → T2 永远轮不上 → 厕所永远建不成。
                            let tag = format!(
                                "{} {}",
                                n.name,
                                n.actions
                                    .iter()
                                    .map(action_identity)
                                    .collect::<Vec<_>>()
                                    .join(" ")
                            );
                            // ：禁止扩张时跳过换气/地表/挖矿类节点
                            if directive.forbid_expansion
                                && (tag.contains("换气") || tag.contains("地表") || tag.contains("挖矿"))
                                && !tag.contains("产氧")
                            {
                                return false;
                            }
                            match want {
                                crate::priority::Action::Rescue => {
                                    // 人在 CO₂/污染格里时正解是"换气/产氧把人带出来"，
                                    //   只匹配"救援擦排"会在救援节点 done 后匹配失败
                                    //   静默回落到常规计划 → 紧急态被稀释。
                                    tag.contains("救援")
                                        || tag.contains("擦")
                                        || tag.contains("排")
                                        || tag.contains("换气")
                                        || tag.contains("地表")
                                        || tag.contains("空气")
                                        || tag.contains("产氧")
                                        || tag.contains("氧石")
                                }
                                crate::priority::Action::Ventilate => {
                                    tag.contains("换气") || tag.contains("地表") || tag.contains("空气")
                                }
                                crate::priority::Action::ProduceOxygen => {
                                    tag.contains("产氧") || tag.contains("氧石")
                                }
                                crate::priority::Action::ForageFood => {
                                    tag.contains("食物") || tag.contains("采集")
                                }
                                crate::priority::Action::Power => {
                                    tag.contains("供电") || tag.contains("发电")
                                }
                                crate::priority::Action::BuildToilet => {
                                    // 膀胱告急 → 必须匹配到"卫生/厕所/旱厕"节点。
                                    //   没有厕所 → 随地排泄 → 污染水
                                    // T1 名字写「旱厕」而非「厕所」，旧匹配只认
                                    //   「厕所/卫生/如厕」→ Urgent 喊建厕所却匹配不到 → 回落 → 膀胱 100%。
                                    //   同时认 Outhouse prefab（无论节点名怎么写）。
                                    tag.contains("厕所")
                                        || tag.contains("卫生")
                                        || tag.contains("如厕")
                                        || tag.contains("旱厕")
                                        || tag.contains("Outhouse")
                                }
                                crate::priority::Action::FollowPlan => false,
                            }
                        })
                        .cloned()
                })
        } else {
            None
        };

        // 调度器抢了紧急态但匹配不到节点 → 必须显式报告，
        //   不能静默回落让人以为"紧急态已被处理"。
        if directive.level >= crate::priority::Level::Urgent && picked_by_scheduler.is_none() {
            report.notes.push(format!(
                "⚠ 调度[{:?}] 找不到匹配节点（动作 {:?}）→ 回落常规计划；请检查 plan 是否覆盖该场景",
                directive.level, directive.action
            ));
            // 效果回执：
            //   Urgent 喊出"建厕所"却匹配不到节点 = 紧急态形同虚设；
            //   旧逻辑只写一行日志 → 看着像在抢救，实际膀胱涨到 100%。
            //   现在升级为 escalation + bus，慢系统一定能看见并处理。
            let _ = crate::escalate::raise(
                crate::escalate::Kind::StrategyMismatch,
                "",
                format!("Urgent 动作 {:?} 找不到匹配节点：{}（紧急态被静默降级）",
                    directive.action, directive.reason),
                "请检查 plan 是否覆盖该紧急场景（节点名/动作标签）".into(),
                crate::escalate::Ctx { cycle: snap.cycle, dupes: snap.dupes.len(),
                    oxygen_kg: snap.oxygen_kg(), food_kcal: snap.food_kcal(), ..Default::default() },
            );
        }
        // 修正：**Urgent 抢占必须尊重依赖图**。
        //   旧行为：膀胱告急 → 直接选中"建厕所"节点 → 但它的前置（挖空间/铺砖）
        //   还没做 → 预检必不过 → 每轮失败 → 每轮升级 → 每轮冻结 → 噪声不断。
        //   正解：抢占到的节点若前置未完成 → **改为执行它的前置**（逐级回溯）。
        // ★ 紧急抢占必须尊重依赖图 —— **但有例外**。
        //   若被回溯到的前置**已被熔断（blocked）或暂缓（deferred）**，依赖链走不通：
        //   继续执行它只会重复一次注定失败的施工（实测 T1 熔断 → Urgent 建厕所
        //   变成"执行 T1" → 厕所永远建不成 → 膀胱爆表）。
        //   此时保命**不等它** → 标记 chain_blocked，改走内置保命反射弧。
        let mut urgent_chain_blocked = false;
        let picked_by_scheduler = picked_by_scheduler.map(|n| {
            let mut cur = n;
            let mut guard = 0;
            loop {
                guard += 1;
                if guard > 8 {
                    break;
                }
                let Some(pre_id) = cur.requires.iter().find(|r| {
                    !plan.nodes.iter().any(|m| &&m.id == r && m.is_done())
                }) else {
                    break;
                };
                let Some(pre) = plan.nodes.iter().find(|m| &m.id == pre_id) else {
                    break;
                };
                if pre.is_done() {
                    break;
                }
                if pre.is_blocked() || pre.is_deferred() {
                    // 等价于 has_unexecutable_predecessor 的单点判定（保持回溯语义）
                    urgent_chain_blocked = true;
                    report.notes.push(format!(
                        "⚠ 紧急抢占 [{}] 的前置 [{}] 已不可执行（熔断/暂缓）→ 保命不等它，改走内置反射弧",
                        cur.id, pre.id
                    ));
                    break;
                }
                report.notes.push(format!(
                    "⚠ 紧急抢占 [{}] 的前置 [{}] 未完成 → 本轮先做前置",
                    cur.id, pre.id
                ));
                cur = pre.clone();
            }
            cur
        });
        // Urgent 抢占到的是 done 节点（如 T1 建了 1 座厕所后 done，
        //   但膀胱告急需要第 2/3 座）→ 必须把它写回 todo，否则 decide() 永远不会选它。
        if let Some(n) = &picked_by_scheduler {
            if n.is_done() {
                mark_node_todo(&self.cfg.plan_path, &n.id);
                report.notes.push(format!(
                    "🔄 紧急需求重新告急 → 复活已完成节点 [{}]（判据已上调到 ≥人口数）",
                    n.id
                ));
            }
        }
        // ★ 契约不变量2：保命动作不可阻塞。
        //   Urgent（膀胱满/窒息/缺食）已判定，但**plan 走不通**时不能只报错回落：
        //     ① 依赖链断（前置被熔断/暂缓）→ "尊重依赖图"会把保命改写成执行那个
        //        注定失败的前置 → 保命目标永远不达成；
        //     ② plan 里根本没有匹配节点（plan-reset 后 LLM 还没出图，0 节点）。
        //   两种情况都改为：直接执行**内置保命反射弧**（不依赖 plan 的语义动作）。
        //   命中了可执行的 plan 节点时仍走 plan（它可能有更完整的施工流程）。
        if directive.level >= crate::priority::Level::Urgent
            && (urgent_chain_blocked || picked_by_scheduler.is_none())
        {
            let vital = vital_reflex_actions(directive.action);
            if !vital.is_empty() {
                let out = execute(&self.session, &self.catalog, &vital);
                // ★ 打印完整 detail：只打 "build:ok" 会掩盖"工单被接受但没落地"
                //   （实测：旱厕 2×3 没空位，mod 接受工单却建不成 → 看着成功实际没进展）。
                let detail = out.results.iter()
                    .map(|x| format!("{} → {}", x.kind, x.detail))
                    .collect::<Vec<_>>().join(" | ");
                report.acted = true;
                report.notes.push(format!(
                    "🛡 Urgent[{}] plan 走不通（{}）→ 直执行内置保命反射弧（{}）",
                    directive.action.label(),
                    if urgent_chain_blocked { "依赖链断" } else { "无匹配节点" },
                    detail
                ));
                // ★ 反射弧**失败**时不独占本轮 → 让 plan 继续执行。
                //   反射弧只会"下工单"，而工单能不能被复制人执行是另一回事
                //   （实测：旱厕 2×3 要先腾空间，反射弧挖的那格复制人够不到 → 死单）。
                //   真正的解法往往在 plan 里（挖空间/换方向）——
                //   若这里 return，plan 节点每轮都被抢占 → 永远轮不上 → 死锁。
                if out.ok {
                    return report;
                }
                report.notes.push(
                    "↩ 反射弧未达成 → 本轮继续执行 plan（真正的解法在依赖图里，如挖空间/换方向）".into(),
                );
            }
        }
        let chosen = if let Some(n) = picked_by_scheduler {
            Some(n)
        } else if rescue_mode {
            plan.nodes
                .iter()
                .find(|n| n.rescue && !n.is_done() && !n.is_deferred())
                .cloned()
                .or_else(|| {
                    plan.survival_node
                        .as_ref()
                        .and_then(|id| plan.nodes.iter().find(|n| &n.id == id))
                        .filter(|n| !n.is_done() && !n.is_deferred())
                        .cloned()
                })
                .or_else(|| d.chosen.clone())
        } else if survival_mode {
            plan.survival_node
                .as_ref()
                .and_then(|id| plan.nodes.iter().find(|n| &n.id == id))
                // 保命节点**已经完成/已暂缓**时不能再选它：它的目标格早被挖/建过了，
                //   再执行一遍只会反复被预检拒绝 → 死循环刷屏。
                .filter(|n| !n.is_done() && !n.is_deferred())
                .cloned()
                .or_else(|| d.chosen.clone())
        } else {
            d.chosen.clone()
        };
        let Some(node) = chosen else {
            // 保命反射弧已在上面处理（Urgent 且 plan 走不通时直执行）；
            //   走到这里说明不是 Urgent，或保命动作不在反射弧覆盖范围内。
            report.notes.push("依赖图里没有可执行目标".into());
            // 快慢分工：依赖图死锁 = **规则层解不开** → 喊慢系统。
            //   慢系统要么改 plan（换顺序/拆依赖），要么改代码（补规则）。
            //   快系统自己解只会反复空转。
            {
                let ctx = crate::escalate::Ctx {
                    cycle: snap.cycle,
                    dupes: snap.dupes.len(),
                    oxygen_kg: snap.oxygen_kg(),
                    food_kcal: snap.food_kcal(),
                    ..Default::default()
                };
                crate::escalate::raise(
                    crate::escalate::Kind::Deadlock,
                    "",
                    format!(
                        "依赖图里没有可执行目标（{} 个节点全部 done/blocked/deferred）。快系统无法自行解开。",
                        plan.nodes.len()
                    ),
                    "建议慢系统：检查 requires 是否指向不存在的节点、或按当前局面重排依赖（可改 plan.json 后用 oni plan-check 复核）".into(),
                    ctx,
                );
                report.notes.push(crate::escalate::summary());
            }
            return report;
        };
        if rescue_mode {
            report.notes.push(format!(
                "⛑ 救援模式：执行 {}（{}）—— 有复制人倒地，普通施工全部让位",
                node.id, node.name
            ));
        }
        if survival_mode {
            report.notes.push(format!(
                "🚨 保命模式：执行 {}（{}）—— 空气/人口红线优先于一切其它目标",
                node.id, node.name
            ));
        }
        report.node = Some(node.id.clone());

        // 判据**先于动作**：目标已经达成就别再动手。
        //   旧顺序（先执行、后判达标）有两个真实代价：
        //   1) 判据在开局就已满足的节点会白执行一次动作；
        //   2) 那个动作往往"没有对象可做" → 被判为失败 → 触发「动作失败→冻回暂停」
        //      → 殖民地刚开局就停摆。
        // 救援节点是**常驻能力**，不是可完成的里程碑 → 不能因判据达成就标 done。
        //   R2 的判据 breath_min 98 在人健康时达成 → R2 被标 done
        //   → 之后 Urgent 报「找不到匹配节点（动作 Rescue）」→ 紧急态被静默降级。
        if !node.rescue {
            if let Some(v) = node.success_obj() {
                if let Some(true) = check_success(&snap, &v) {
                    mark_node_done(&self.cfg.plan_path, &node.id);
                    report.notes.push(format!(
                        "[{}] 成功判据**已满足** → 直接标记 done（不执行动作）",
                        node.id
                    ));
                    report.acted = false;
                    return report;
                }
            }
        }

        if node.actions.is_empty() {
            report.notes.push(format!("[{}] 没有动作定义（需要先规划该节点）", node.id));
            return report;
        }
        // （）：**有人倒地时，本轮不许再施工**。
        //   旧逻辑在救援模式下仍会执行保命节点（常常是「挖地表」）
        //   施工继续把人送进毒气区，倒地的人没救成还多倒几个。
        //   正解：救援模式下只做救援动作（mop / 搬人 / 建床），挖掘与建造一律让位。
        //   但**保命节点必须放行**：2026-11-22 ，
        //     上面那条「空气/产氧主线」恰恰就是救命的施工（挖上去就有新风），
        //     一旦被「有人倒地就停工」拦住，殖民地只能等死。
        let node_is_survival = plan.survival_node.as_deref() == Some(node.id.as_str());
        if rescue_mode
            && !node_is_survival
            && !node.actions.is_empty()
            && node.actions.iter().all(|a| {
                matches!(
                    a,
                    crate::exec::Action::Dig { .. }
                        | crate::exec::Action::Build { .. }
                        | crate::exec::Action::AutoBuild { .. }
                )
            })
        {
            report.notes.push(format!(
                "[{}] 有人失去行动能力 → 本轮不施工（挖掘/建造让位），先救人",
                node.id
            ));
            return report;
        }
        let out = execute(&self.session, &self.catalog, &node.actions);
        report.acted = true;

        // ---- 统一卡住检测（单一真相来源）--------------------------
        //   之前"卡住"的判定散落在七八个 if 里 → 漏一个就是致命缺口。
        //   现在每轮跑一次 stuck::detect()，产出统一清单 → 全部升级给慢系统。
        //   死工单计数取自挖掘结果回报的"已在位 N"（mod 的 dig 标记不进 pendingOrders）。
        {
            let dig_markers = out
                .results
                .iter()
                .find_map(|r| {
                    let i = r.detail.find("已在位 ")?;
                    r.detail[i + "已在位 ".len()..]
                        .split(|c: char| !c.is_ascii_digit())
                        .next()
                        .and_then(|n| n.parse::<usize>().ok())
                })
                .unwrap_or(0);
            let rep = crate::stuck::detect(
                &snap,
                Some(&out),
                &node.id,
                false,
                self.action_failures,
                self.no_progress,
                dig_markers,
                self.last_pop,
            );
            if !rep.is_empty() {
                // 去抖：**连续 2 轮**命中同一 kind 才升级。
                //   瞬时状态（复制人正在"前往"安全区、刚睡醒、刚挖完一格）
                //   只会命中一轮 → 不升级、不冻结、不中断施工。
                let mut confirmed: Vec<&crate::stuck::Stuck> = Vec::new();
                for s in &rep.items {
                    let k = s.kind.code().to_string();
                    let n = self.stuck_streak.entry(k.clone()).or_insert(0);
                    *n += 1;
                    if *n >= 2 {
                        confirmed.push(s);
                    }
                }
                // 没命中的 kind → 清零（问题已消失）
                let hit: std::collections::HashSet<String> =
                    rep.items.iter().map(|s| s.kind.code().to_string()).collect();
                self.stuck_streak.retain(|k, _| hit.contains(k));
                if !confirmed.is_empty() {
                    let ctx = crate::escalate::Ctx {
                        cycle: snap.cycle,
                        dupes: snap.dupes.len(),
                        oxygen_kg: snap.oxygen_kg(),
                        food_kcal: snap.food_kcal(),
                        ..Default::default()
                    };
                    for s in &confirmed {
                    let kind = match s.kind {
                        crate::stuck::StuckKind::PreflightRejected => crate::escalate::Kind::StrategyMismatch,
                        crate::stuck::StuckKind::NoFeasibleTarget => crate::escalate::Kind::NoFeasibleTarget,
                        crate::stuck::StuckKind::NoMaterial => crate::escalate::Kind::NoFeasibleTarget,
                        crate::stuck::StuckKind::OrderNotExecuted => crate::escalate::Kind::NoProgress,
                        crate::stuck::StuckKind::NodeFailing => crate::escalate::Kind::NodeFailing,
                        crate::stuck::StuckKind::Deadlock => crate::escalate::Kind::Deadlock,
                        crate::stuck::StuckKind::Downed => crate::escalate::Kind::UnknownHazard,
                        crate::stuck::StuckKind::Unbreathable => crate::escalate::Kind::UnknownHazard,
                        crate::stuck::StuckKind::PopulationDrop => crate::escalate::Kind::UnknownHazard,
                        crate::stuck::StuckKind::NoProgress => crate::escalate::Kind::NoProgress,
                    };
                    crate::escalate::raise(
                        kind,
                        &s.node,
                        s.detail.clone(),
                        "请慢系统决策：换目标 / 改计划 / 改代码 / 直接下精确工单".into(),
                        ctx,
                    );
                    // P0-61 核心修复：策略类卡住的**节点必须暂缓**，
                    //   否则下一轮 decide 又选它 → 又失败 → 又 escalate → 冻结空转（活锁根因）。
                    //   危险类（倒地/不可呼吸/人口下降/死锁）不 defer —— 那是全局态，defer 节点没用。
                    //   保命节点/救援节点永不 defer。
                    let strategic = matches!(
                        s.kind,
                        crate::stuck::StuckKind::PreflightRejected
                            | crate::stuck::StuckKind::NoFeasibleTarget
                            | crate::stuck::StuckKind::NoMaterial
                            | crate::stuck::StuckKind::NodeFailing
                            | crate::stuck::StuckKind::NoProgress
                    );
                    let is_survival = plan.survival_node.as_deref() == Some(s.node.as_str());
                    let is_rescue = plan.nodes.iter()
                        .find(|n| n.id == s.node).map(|n| n.rescue).unwrap_or(false);
                    if strategic && !s.node.is_empty() && !is_survival && !is_rescue {
                        mark_node_deferred(&self.cfg.plan_path, &s.node, "卡住检测确认：换方案前暂缓", snap.cycle as u64);
                        report.notes.push(format!("🔄 卡住节点 {} 已暂缓（换方案，避免反复选中）", s.node));
                    }
                }
                    let confirmed_summary: String = confirmed
                        .iter()
                        .map(|s| format!("[{}] {}", s.kind.code(), s.detail))
                        .collect::<Vec<_>>()
                        .join("；");
                    report.notes.push(format!("🆘 卡住检测：{}", confirmed_summary));
                    // 二次修正：
                    //   旧逻辑：stuck → 冻结世界 + 等慢系统 → 谁都不恢复 → 无限循环（已废除）。
                    //   正解：**卡住时世界继续跑**（已有工单做完、膀胱/呼吸照常演化），
                    //   只在本轮不下新工单 + 升级给慢系统（问题仍在清单里）。
                    //   冻结只留给：真红线紧急态占空比 / 主 agent 的 hold。
                    // ★ 契约：卡住只上报慢系统，不停手——世界继续跑，保命动作照常。
                    if !rep.needs_running_world() {
                        report.notes.push(
                            "⏸ 卡住已上报慢系统 → 世界继续跑，保命动作照常执行".into(),
                        );
                    }
                }
            }
        }

        // ---- 空间/布局类失败 → 喊慢系统（快系统解不了）----------------
        //   「找不到能放 X 的落点」= **空间不够**。
        //   这不是「再试一次」能解决的，是**布局决策** → 属于慢系统的活：
        //   要么先扩建出 2 格高空间，要么换布局/换建筑。
        {
            let space_blocked = out.results.iter().any(|r| {
                r.detail.contains("找不到能放") || r.detail.contains("auto 内找不到")
            });
            if space_blocked {
                self.space_failures += 1;
            } else {
                self.space_failures = 0;
            }
            if self.space_failures >= 2 {
                // 这条路径**不再自己升级/停手** ——
                //   统一交给 stuck::detect，
                //   否则"前置未完成"这种正常状态会每轮都升级+冻结 → 噪声不断。
                //   这里只保留计数，供 stuck::detect 参考。
                self.space_failures = 0;
            }
        }

        // =====================================================================
        // 5) 执行**之后**再判空闲 → 没活才冻回暂停
        //    顺序是硬要求：旧逻辑在执行前用"上一轮的 pending_orders"判空闲，
        //      把"这一轮刚下的第一张工单"当成空闲 → 立刻冻回 → 工单永不执行。
        // =====================================================================
        let work_now = issued_work(&out)
            || snap.pending_orders.unwrap_or(0) > 0
            || snap.under_construction > 0;
        // 只有**动作真的失败**才冻回。动作成功但没产生工单（如只读的侦察、
        //   或节点已达标）不等于"卡住" → 继续下一轮换目标（否则开局第一轮就冻死，
        // 新档：侦察成功 → 冻回 → 殖民地停摆直到人工再开窗）。
        // 被研究锁的失败**不算**冻回理由：
        //   烤箱/电解器这类建筑造多少次都不会成功 → 冻回 → 殖民地停摆，
        //   而别的节点（产氧/换气）其实完全能做。
        let research_locked = out.results.iter().any(|r| {
            !r.ok && r.detail.contains("被研究锁")
        });
        if !work_now && !out.ok && !research_locked && self.window_owned_by_us {
            // ★ 慢系统还有待办语义目标吗（ai_goal / 升级请求）？
            //   有 → 这不是"真没活干"，是"本轮方案失败但战略还有别的路"：
            //     走自持节律（冻 20s → 续窗重试），失败记 goal_outcomes +
            //     熔断计数驱动慢系统换目标。绝不能进"等人工介入"死锁——
            //     否则慢系统入队的 ai_goal 永远等不到窗口：快系统暂停只观察、
            //     慢系统空转入队，两边互等（2026-11-24 实测死锁，世界冻死）。
            //   无 → 真没活干 → 冻回等介入（不空烧周期）。
            let slow_pending = !crate::commands::ai_goals_pending().is_empty()
                || crate::escalate::has_pending();
            let freeze_reason = if slow_pending {
                "动作失败但慢系统有待办目标 → 冻回（20s 后自持重试）"
            } else {
                "动作失败且无待办 → 冻回暂停，等待人工/主 agent 介入"
            };
            self.pause(freeze_reason);
            self.paused_by_us = false;
            self.frozen = true;
            self.window_until = None;
            self.window_owned_by_us = false;
            if slow_pending {
                self.auto_rhythm = true;
                self.resume_at = Some(Instant::now() + Duration::from_secs(20));
                report.notes.push(
                    "🔒 动作失败但慢系统有待办目标 → 冻 20s 后自持重试（熔断驱动换目标，不进互等死锁）".into(),
                );
            } else {
                // 真没活干 → **不自持**
                self.auto_rhythm = false;
                self.resume_at = None;
                report.notes.push(
                    "🔒 动作失败且没有待办工单 → 冻回暂停（等主 agent 修代码/决策，不空烧周期）".into(),
                );
            }
        }

        for r in &out.results {
            report.notes.push(format!(
                "{} {} → {}",
                if r.ok { "✅" } else { "❌" },
                r.kind,
                r.detail
            ));
        }

        // 被研究锁 → 把节点标记为**暂缓**（deferred），本轮不再选它。
        //   为什么不是"失败→冻回"：
        //     被研究锁的建筑造多少次都不会成功（烤箱/电解器/藻类箱）
        //     每轮失败 → 每轮冻回 → 殖民地反复停摆，而别的节点其实能做。
        //   为什么不是"熔断 blocked"：熔断 = 代理的方案走不通（要换方案）；
        //     研究锁 = 外部条件未满足（等研究就好），不该消耗熔断额度。
        if research_locked {
            mark_node_deferred(&self.cfg.plan_path, &node.id, "被研究锁：等研究完成后再造", self.cycles);
            report.notes.push(format!(
                "⏸ [{}] 被研究锁 → 标记暂缓（本轮跳过，不算熔断）；代理去做别的能做的事",
                node.id
            ));
            report.acted = false;
        }
        // 成功判据达成 → 节点完成，写回账本，下一轮换下一个目标
        if out.ok {
            if let Some(v) = node.success_obj() {
                if let Some(ok) = check_success(&snap, &v) {
                    if ok {
                        mark_node_done(&self.cfg.plan_path, &node.id);
                        clear_node_stall(&self.cfg.plan_path, &node.id);
                        report.notes.push(format!(
                            "[{}] 成功判据达成 → 标记 done，下一轮换下一个目标",
                            node.id
                        ));
                    } else {
                        // ★ 连续没达成 → 累加 stall：
                        //   达到 decide::STALL_LIMIT 就把这个节点让给别人。
                        //   旧逻辑只写 note 就继续选它 → 一个"每轮都能做点什么"
                        //   的节点会**永久霸占**执行位 → 其它节点（供电）0 次被选中。
                        let stall = node.stall.unwrap_or(0) + 1;
                        mark_node_stall(&self.cfg.plan_path, &node.id, stall);
                        report.notes.push(format!(
                            "[{}] 动作已执行但成功判据未达成 → 下一轮继续（连续 {} 轮未达成，达 {} 轮让位）",
                            node.id, stall, crate::decide::STALL_LIMIT
                        ));
                    }
                }
            }
        }

        // =====================================================================
        // 6) 进展指纹 / 无进展熔断
        //    只统计**世界真的跑过**的轮次（）：
        //      冻结期的指纹必然不变 —— 那不是"卡住"，是"世界没动"。
        //    有活排队时也不算卡住（复制人可能在睡觉，工单是好的）。
        // =====================================================================
        let fp = snap.progress_fingerprint();
        if world_advanced {
            // busy 也要算上「本轮自己刚下的工单」：
            //   mod 的工单在 step_facts 里看不见
            //   只看它 → 复制人正在挖的那 3~6 轮里指纹不变 → 连续 3 轮 → **误熔断好节点**
            //   。
            //   有活排队 = 不是卡住 = 不计入零进展。
            let busy = snap.pending_orders.unwrap_or(0) > 0
                || snap.under_construction > 0
                || issued_work(&out);
            if busy {
                self.no_progress = 0;
            } else if fp == self.last_fingerprint {
                self.no_progress += 1;
            } else {
                self.no_progress = 0;
            }
            self.last_fingerprint = fp.clone();
        }
        // 暂缓节点的**自动恢复**：条件变了就该重试（否则暂缓 = 永久放弃）。
        //   恢复判据：节点的 requires 已全部完成（前置做完了）+ **冷却期已过**。
        //   为什么要有冷却期：
        //     没有冷却 → 暂缓 → 下一轮前置仍满足 → 立刻恢复 → 又失败 → 又暂缓……
        //     形成"暂缓-恢复"抖动，代理在同一节点上空转。
        //     冷却期让代理先去做别的，条件真的变了再回来。
        for n in &plan.nodes {
            if !n.is_deferred() {
                continue;
            }
            // 冷却：从暂缓那一刻起，至少隔 N **代理轮次**才允许恢复。
            // 必须用代理自己的轮次（self.cycles），不能用游戏 cycle：
            //   游戏周期很长，用 cycle 会让冷却永远不结束
            //   → 所有节点都被暂缓 → "依赖图里没有可执行目标"（）。
            if let Some(at) = n.deferred_at {
                if self.cycles.saturating_sub(at) < Self::DEFER_COOLDOWN {
                    continue;
                }
            }
            let unmet: Vec<&str> = n
                .requires
                .iter()
                .filter(|rid| {
                    plan.nodes
                        .iter()
                        .find(|m| m.id == **rid)
                        .map(|m| !m.is_done())
                        .unwrap_or(false)
                })
                .map(|s| s.as_str())
                .collect();
            if unmet.is_empty() {
                // 前置都完成了 → 解除暂缓，重试
                clear_node_deferred(&self.cfg.plan_path, &n.id);
                report.notes.push(format!(
                    "🔄 [{}] 暂缓冷却期已过且前置完成 → 解除暂缓，重试",
                    n.id
                ));
            }
        }

        // 动作**明确失败**的计数：
        //   建造类失败（找不到落点 / 材料不足 / 被研究锁）会让代理**无限重试**同一个节点：
        //   第二个厕所连撞 8 次 → 每轮冻回 → 殖民地反复停摆，而别的节点其实能做。
        //   为什么不能靠 no_progress 熔断兜底：失败后代理会冻回暂停
        //     世界没跑 → 指纹没变 → 但"没跑"被排除在熔断计数外 → 永不熔断。
        //   正解：动作**明确失败**单独计数，连续 N 次 → 暂缓该节点（deferred），
        //     让代理换方案；等条件变化（空间挖好/研究完成）再自动恢复。
        let action_failed = !out.ok && out.results.iter().all(|r| !r.ok);
        if action_failed {
            self.action_failures += 1;
        } else {
            self.action_failures = 0;
        }
        // ★ 自动放宽选址判据：连续失败 → **扩大搜索范围**（多找几块地方）。
        //   这是"策略在代码里可调"的最小闭环：慢系统不必每次都出手，
        //   快系统先按策略自己放宽一档；仍不行再上报慢系统。
        //   ★ 绝不自动放宽 **CO₂ 阈值**：那是保命判据（人要待在那儿）。
        //     实测自动把 CO₂ 上限从 2.5 放宽到 8kg/格 → 厕所建在 CO₂ 里 →
        //     复制人如厕时呼吸掉到 66 → 触发一串假紧急态 → 慢系统反复暂停游戏。
        //     "放宽"应该是扩大候选集，不是放松安全线；安全线只能由慢系统显式调。
        {
            let mut pol = oni_world::preflight::site_policy();
            if pol.relax_after_fail > 0 && self.action_failures >= pol.relax_after_fail {
                let old = pol.site_radius_boost;
                pol.site_radius_boost = (old + 4).min(16);
                if pol.site_radius_boost > old {
                    oni_world::preflight::set_site_policy(pol.clone());
                    self.action_failures = 0;
                    report.notes.push(format!(
                        "🔧 选址范围自动放宽：搜索半径 +{} → +{} 格（连续失败 {} 次；CO₂ 上限 {}kg 不动）",
                        old, pol.site_radius_boost, pol.relax_after_fail, pol.co2_max_kg
                    ));
                }
            }
        }

        // ---- ★ 动作级循环检测（"循环即升级"）----
        //   节点级计数看不见"代理在同一格上反复下同样的工单"（实测厕所：每轮挖
        //   (115,194)，而那格早已挖开 → 无限循环，厕所永远建不成）。
        //   这里按**动作指纹**（kind@坐标）计账：同一指纹连续出现 N 次 →
        //   判定执行层循环 → ① 上报慢系统（带指纹/次数/失败类别/现场事实）
        //   ② 暂缓该节点（下一轮不再选它）→ 慢系统换方案，而不是继续硬试。
        let mut loop_hit: Option<(String, usize)> = None;
        // 本轮出现过的动作指纹（连续性判定：本轮没出现 = 中断 → 计数归零）
        let fps: Vec<String> = out.results.iter().filter_map(action_fingerprint).collect();
        for fp in &fps {
            // 保留本轮出现过的 + 上一轮累计的（上一轮没出现的已在下面清零）
            let c = self.action_loops.entry(fp.clone()).or_insert(0);
            *c += 1;
            if *c >= Self::ACTION_LOOP_MAX {
                loop_hit = Some((fp.clone(), *c));
                break;
            }
        }
        // 连续性中断的指纹（这轮没出现）→ 计数归零
        self.action_loops.retain(|k, _| fps.contains(k));
        if let Some((fp, times)) = loop_hit {
            let detail = format!(
                "执行层循环：动作 {} 连续 {} 轮重复执行仍未产生效果。{}",
                fp, times,
                out.results.iter().map(|r| r.detail.clone())
                    .collect::<Vec<_>>().join(" | ")
            );
            let ctx = crate::escalate::Ctx {
                cycle: snap.cycle, dupes: snap.dupes.len(),
                oxygen_kg: snap.oxygen_kg(), food_kcal: snap.food_kcal(),
                breath: snap.min_breath().unwrap_or(0.0),
                bladder: snap.max_bladder(), ..Default::default()
            };
            crate::escalate::raise(
                crate::escalate::Kind::NoProgress, &node.id, detail,
                "慢系统换方案：该目标落点/判据可能不成立（选址条件过严？目标已被挖开？换个目标或放宽判据）".into(),
                ctx,
            );
            // 暂缓该节点：下一轮不再选它 → 打断循环，等慢系统给新方向。
            let _ = mark_node_deferred(&self.cfg.plan_path, &node.id, "执行层循环", snap.cycle as u64);
            self.action_loops.clear();
            report.notes.push(format!(
                "🔁 执行层循环（{} 连续{}次）→ 已升级慢系统并暂缓 [{}]，不再硬试",
                fp, times, node.id
            ));
            return report;
        }
        // 未命中循环：账本已按"本轮未见则归零"维护，无需额外清理。
        if self.action_failures >= Self::ACTION_FAIL_MAX && !research_locked {
            let is_survival = plan.survival_node.as_deref() == Some(node.id.as_str());
            if is_survival {
                // 快慢分工修正：保命节点失败**不再无限重试**。
                //   旧行为：重置失败计数 → 下一轮再试 → 无限循环、白烧周期。
                //   新行为：**喊慢系统**（escalation）+ 停手等它改计划。
                //   慢系统拿到信息后要么改目标、要么改代码、要么换策略族 ——
                //   这才是「AI 参与规划」的落点，而不是让快系统自己猜。
                self.survival_failures += 1;
                let ctx = crate::escalate::Ctx {
                    cycle: snap.cycle,
                    dupes: snap.dupes.len(),
                    oxygen_kg: snap.oxygen_kg(),
                    food_kcal: snap.food_kcal(),
                    ..Default::default()
                };
                crate::escalate::raise(
                    crate::escalate::Kind::NodeFailing,
                    &node.id,
                    format!(
                        "保命节点「{}」连续 {} 次动作失败：{}。快系统无法自行解决（空气/产氧是红线）。",
                        node.id,
                        self.survival_failures,
                        out.results
                            .iter()
                            .map(|r| r.detail.clone())
                            .collect::<Vec<_>>()
                            .join(" | ")
                    ),
                    "建议慢系统检查：该目标的材料是否可达（上下分层？）、或换一条主线（例如直接打通地表换气而非产氧）".into(),
                    ctx,
                );
                self.action_failures = 0;
                // ★ 保命节点连续失败 → **暂缓**（deferred），不只"本轮停手"。
                //   旧行为：只 return 本轮 → 下一轮 decide() 又选中同一个保命节点
                //   → 又失败 → 又冻回 → **无限乒乓**（实测 T5/oxygen 物理上不可成功：
                //     零氧石、零电、零藻类 → 每轮必然失败 → 游戏永远停着）。
                //   正解：**暂缓**让快系统去执行别的目标、给慢系统换方向的空间；
                //   deferred 有冷却期，条件变化后可自动恢复（ unlike 熔断=永久自杀）。
                //   ★ 保命节点**禁熔断**的规则不变（不写 circuit_breaks 永久记录）。
                mark_node_deferred(
                    &self.cfg.plan_path,
                    &node.id,
                    &format!("保命节点连续 {} 次动作失败 → 暂缓换方向（禁熔断，条件变化后自动恢复）", self.survival_failures),
                    self.cycles,
                );
                report.notes.push(format!(
                    "🆘 [{}] 保命节点连续失败 → 已升级给慢系统并**暂缓**（本轮停手 + 后续几轮不再重试，等慢系统换方向；条件变化后自动恢复）",
                    node.id
                ));
                report.notes.push(crate::escalate::summary());
                return report;
            } else {
                mark_node_deferred(
                    &self.cfg.plan_path,
                    &node.id,
                    &format!("连续 {} 次动作失败 → 暂缓，等条件变化后自动恢复", self.action_failures),
                    self.cycles,
                );
                report.notes.push(format!(
                    "⏸ [{}] 连续 {} 次动作失败 → 标记暂缓（换方案）；条件变化后可自动恢复",
                    node.id, self.action_failures
                ));
                self.action_failures = 0;
            }
        }

        if self.no_progress >= 3 {
            // 保命节点（产氧/换气）**永不熔断**：它没有替代方案，
            //   熔断它 = 整张依赖图停摆 + colony 窒息（真事故）。
            let is_survival = plan.survival_node.as_deref() == Some(node.id.as_str());
            if is_survival {
                mark_node_todo(&self.cfg.plan_path, &node.id);
                report.notes.push(format!(
                    "🛡 [{}] 是保命节点，禁止熔断（熔断=自杀）→ 撤销死工单后重试",
                    node.id
                ));
            } else {
                mark_node_blocked(&self.cfg.plan_path, &node.id);
                report.notes.push(format!(
                    "⛔ [{}] 连续 {} 轮零进展 → 熔断该节点、撤销死工单，下一轮换目标",
                    node.id, self.no_progress
                ));
            }
            // 取消该节点留下的死工单（否则它们会一直挂着假装有活干）
            for a in &node.actions {
                if let crate::exec::Action::Dig { rect, .. } = a {
                    let nums: Vec<i32> = rect
                        .split(',')
                        .filter_map(|t| t.trim().parse().ok())
                        .collect();
                    if nums.len() == 4 {
                        let cmd = format!(
                            "消 x1={} y1={} x2={} y2={} priority=7 confirm=true",
                            nums[0], nums[1], nums[2], nums[3]
                        );
                        let _ = crate::exec::run_order_raw(&self.session, &cmd, false);
                    }
                }
            }
            self.no_progress = 0;
        }

        // ---- Per-dupe idle watch: report sustained individual idleness to AI ----
        // A single idle dupe is normal (walking, waiting for materials);
        // 6+ consecutive cycles (60s) of true idleness while goals remain is not.
        if !snap.paused {
            let idle_names: std::collections::HashSet<&str> = snap.dupes.iter()
                .filter(|d| {
                    let c = d.chore.as_str();
                    (c == "空闲" || c == "IdleChore")
                        && !c.contains("休息") && !c.contains("睡觉")
                })
                .map(|d| d.name.as_str()).collect();
            let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
            for name in &idle_names {
                seen.insert((*name).to_string());
                *self.per_dupe_idle.entry((*name).to_string()).or_insert(0) += 1;
            }
            self.per_dupe_idle.retain(|k, _| seen.contains(k));
            // Report only when NOT all-idle (all-idle has its own handler below),
            // goals remain, and no work is pending.
            let sustained: Vec<(String, usize)> = self.per_dupe_idle.iter()
                .filter(|(_, n)| **n >= 6)
                .map(|(k, v)| (k.clone(), *v))
                .collect();
            let todo_exists = plan.nodes.iter().any(|n| n.status == "todo");
            if !sustained.is_empty() && todo_exists
                && snap.pending_orders.unwrap_or(0) == 0
                && snap.under_construction == 0
                && snap.truly_idle_count() < snap.dupes.len()
            {
                let who: Vec<String> = sustained.iter().map(|(n, c)| format!("{n}({c}轮)")).collect();
                let detail = format!("Sustained individual idleness [{}] while goals remain; assign work or replan", who.join(", "));
                // ★ 契约：空闲只上报慢系统，不停手——世界继续跑。
                let _ = crate::escalate::raise(
                    crate::escalate::Kind::NoProgress, "", detail.clone(),
                    "AI: assign work to idle dupes or replan".into(),
                    crate::escalate::Ctx { cycle: snap.cycle, dupes: snap.dupes.len(),
                        oxygen_kg: snap.oxygen_kg(), food_kcal: snap.food_kcal(), ..Default::default() },
                );
                self.per_dupe_idle.clear();
                report.notes.push(format!("⏸ 个体持续空闲 → 冻结并上报 AI：{detail}"));
            }
        }

        // 工单积压+全员闲看门已前移到执行闸门之前（watch_backlog_idle）：
        // hook 暂冻世界期间也能观测，此处不再重复。

        // ---- 空跑即停：没有人干活就立刻冻回，不烧周期 ----
        // 空跑 = 没人干活 **且** 没有待办工单。
        // 用 truly_idle_count（排除休息/睡觉的人）：
        //   全员睡觉 ≠ 无人干活（醒来就会做）→ 用旧判据会在他们睡觉时误停机。
        let all_idle = !snap.dupes.is_empty()
            && snap.truly_idle_count() == snap.dupes.len()
            && snap.pending_orders.unwrap_or(0) == 0
            // 建筑在施工时复制人确实闲着，但那是**有意义的等待**（梯子列要一格格铺）。
            && snap.under_construction == 0
            // 本轮**自己**是否刚制造了活（工单/蓝图/梯子）。
            //   mod 的工单在 step_facts 里看不见 → 只看 pending_orders 会误判空跑
            //   → 代理冻回暂停 → 那些工单永远不会被执行（ 的第五个坑）。
            && !issued_work(&out);
        // 只有**游戏在跑**的时候才累计空跑轮数。
        if all_idle && !snap.paused {
            self.idle_streak += 1;
        } else if !snap.paused {
            // A: 暂停期保留账本（hook 暂冻世界不清零，避免观测窗口错配）
            self.idle_streak = 0;
        }
        // ★ 去掉 window_owned_by_us 门槛：用户恢复的世界里全员空跑同样要上报，
        //   否则"用户按空格开局后代理一直闲置"永远不触发 AI 规划（实测卡住感）。
        if self.idle_streak >= 2 && !snap.paused {
            // Idle may mean "plan done" or "goals exist but nobody executes".
            // The fast system cannot judge which - freeze and escalate to AI.
            // ★ 不冻结世界：全员空闲只上报慢系统（它会主动规划补 goal），世界继续跑。
            self.idle_streak = 0;
            let todo: Vec<&str> = plan.nodes.iter().filter(|n| n.status == "todo")
                .map(|n| n.id.as_str()).collect();
            let detail = if todo.is_empty() {
                "All dupes idle, plan has no todo nodes: run complete or plan needs new goals?".to_string()
            } else {
                format!("All dupes idle but goals [{}] remain: unstuck or replan?", todo.join(", "))
            };
            let _ = crate::escalate::raise(
                crate::escalate::Kind::NoProgress, "", detail.clone(),
                "AI: add goals, unblock a node, or let the colony rest".into(),
                crate::escalate::Ctx { cycle: snap.cycle, dupes: snap.dupes.len(),
                    oxygen_kg: snap.oxygen_kg(), food_kcal: snap.food_kcal(), ..Default::default() },
            );
            // 文案纠正：这里**没有冻结世界**（只是上报慢系统）——
            //   旧文案写"冻结并上报 AI"，与实际行为不符，
            //   读日志的人会以为世界被冻住（本次排查就被它误导过）。
            report.notes.push(format!("⏸ 全员空闲 → 上报 AI（世界继续跑）：{detail}"));

        }

        report
    }
}

/// 这次执行有没有**真的制造出活**（工单 / 蓝图 / 梯子 / 擦除单）。
///
/// 为什么需要：
///   mod 下发的挖掘工单**不出现在 step_facts 的 remainingTrackedOrders 里**
///   。
///   只看 pending_orders → 把"有活排队"误判成"全员空跑"→ 代理冻回暂停
///   → 那 5 张工单永远不会被执行 → 殖民地看起来"在跑"其实零进展。
pub fn issued_work(out: &crate::exec::ExecOutcome) -> bool {
    out.results.iter().any(|r| {
        r.ok && (r.detail.contains("新标记")
            || r.detail.contains("待完成")
            || r.detail.contains("已放置")
            || r.detail.contains("铺梯子")
            || r.detail.contains("擦 @"))
    })
}

pub struct CycleReport {
    pub cycle: i64,
    pub verdict: Verdict,
    pub snap: Snapshot,
    pub judgment: Judgment,
    pub acted: bool,
    pub node: Option<String>,
    pub notes: Vec<String>,
}

/// 检查成功判据（机器可判的那几类）。返回 None = 需要人工判断。
/// 食物还能撑几个周期。
///
/// 为什么要有这个：绝对 kcal 判据在**开局就满足**
///   → 节点在 t=0 直接 done → 代理永远不会去采食物（）。
///   续航判据反映的是"还能撑多久"，开局必然不达标。
pub fn food_runway_cycles(food_kcal: f64, dupes: usize) -> f64 {
    food_kcal / (dupes.max(1) as f64 * crate::judge::KCAL_PER_DUPE_CYCLE)
}

/// ★ 从判据 JSON 里取阈值，**兼容 LLM 常用的多种字段名**。
///
/// 为什么需要：开局规划由 LLM 生成判据，而 prompt 不可能穷举每种 kind 的字段格式，
///   LLM 只能"照相邻例子猜"——实测它把 `loose_min` 写成 `{key:"SandStone", n:50}`，
///   而代码只认 `{element, kg}` → `v.get("element")?` 返回 None → 判据**永远无法判定**
///   → 节点永远不算 done → 反复执行 → 零进展 → 被熔断 → 下游节点前置断链
///   （实测 T1「采建材」砂岩 7507kg 已满足 n=50，却被熔断，连带锁死 T2「建厕所」）。
///   这里做**别名容错**：猜错也能工作，把 LLM 的自由发挥变成可容忍的噪声。
/// 动作的**语义身份**（用于节点匹配）：只含目标词，不含解释文案。
///
/// ★ 为什么不能直接用 `{:?}`（Action 的 Debug）：Debug 会把 `why` 一起打出来，
///   而 `why` 是给人和 LLM 看的**解释文本**，里面的词是偶然的 ——
///   实测节点 T1「浅层建材自举」的 why 写了"厕所、梯子、砖块都需要建材"，
///   于是 Urgent「建厕所」把 T1（采建材）当成了卫生节点 → 真正该跑的
///   T2「卫生前置：三人厕所」永远轮不上。
pub fn action_identity(a: &crate::exec::Action) -> String {
    use crate::exec::Action;
    match a {
        Action::Auto { goal, .. } => format!("goal:{goal}"),
        Action::AutoBuild { prefab, .. } => format!("prefab:{prefab}"),
        Action::Build { prefab, .. } => format!("prefab:{prefab}"),
        Action::Dig { .. } => "dig".to_string(),
        Action::Order { .. } => "order".to_string(),
        Action::AutoConnect { from, to, .. } => format!("connect:{from}:{to}"),
        Action::Scout { .. } => "scout".to_string(),
        Action::Call { tool, .. } => format!("call:{tool}"),
    }
}

fn crit_num(v: &Value, names: &[&str], default: f64) -> f64 {
    for n in names {
        match v.get(*n) {
            Some(x) if x.is_number() => return x.as_f64().unwrap_or(default),
            _ => {}
        }
    }
    default
}

/// 从判据 JSON 里取标识字段（element / key / name / prefab / id 等别名）。
fn crit_str<'a>(v: &'a Value, names: &[&str]) -> Option<&'a str> {
    names.iter().find_map(|n| v.get(*n).and_then(|x| x.as_str()))
}

pub fn check_success(snap: &Snapshot, v: &Value) -> Option<bool> {
    let kind = v.get("kind")?.as_str()?;
    match kind {
        "building_min" => {
            let key = crit_str(v, &["key", "building", "name"])?;
            Some(snap.building(key) as i64 >= crit_num(v, &["n", "count", "min", "target"], 1.0) as i64)
        }
        "rooms_min" => {
            let key = v.get("key")?.as_str()?;
            Some(snap.rooms.get(key).and_then(|x| x.as_i64()).unwrap_or(0) >= v.get("n")?.as_i64().unwrap_or(1))
        }
        "storage_mass_min" => {
            let kg = crit_num(v, &["kg", "n", "min", "target"], 0.0);
            let total: f64 = snap
                .storage
                .as_array()
                .map(|a| a.iter().filter_map(|s| s.get("massStoredKg").and_then(|m| m.as_f64())).sum())
                .unwrap_or(0.0);
            Some(total >= kg)
        }
        // 必须读 **summary 子对象**：电力字段全在 summary 里，
        //   读顶层永远拿不到 → 这个判据一直是假的。
        "power_operational" => Some(snap.power_int("generatorOperational") > 0),
        "oxygen_min" => Some(snap.oxygen_kg() >= crit_num(v, &["kg", "n", "min", "target"], 0.0)),
        // ★ **产氧设备在运转**（不是"造出来了"）。
        //   实测本局铁证：氧气扩散器建成了（制氧1），但
        //   power/summary 显示 generatorCount=0、consumerOperational=0、
        //   netActiveWatts=-120 → **设备通电量为 0、根本没在产氧**，
        //   而基地氧气照常被消耗 → 慢慢耗尽。
        //   判据只看建筑数量（prefab_min）会把"造出来但不运转"当成完成。
        "oxygen_producing" => {
            let producers = snap.building("oxygenProducers");
            let powered = snap.power_int("consumerOperational") > 0;
            let has_generator = snap.power_int("generatorOperational") > 0;
            Some(producers > 0 && powered && has_generator)
        }
        // 地上散装资源够不够（建造前先确认材料够）
        "loose_min" => {
            // 字段别名容错：LLM 常写 {key, n}，规范是 {element, kg}
            let el = crit_str(v, &["element", "key", "name", "tag"])?;
            Some(snap.loose_kg(el) >= crit_num(v, &["kg", "n", "min", "minKg", "target"], 0.0))
        }
        // 发电机真的在发电（不是"建好了但没燃料"）
        "generator_fueled" => Some(
            snap.power_int("generatorOperational") > 0 && snap.power_int("generatorEmpty") == 0,
        ),
        "power_net" => Some(snap.power_num("netActiveWatts") >= crit_num(v, &["watts", "w", "n", "min", "target"], 0.0)),
        "dupe_min" => Some(snap.dupes.len() as i64 >= crit_num(v, &["n", "count", "min", "target"], 1.0) as i64),
        // 基地扩容判据：官方可达格数。
        //   「挖通开阔气室」的真实效果 = **可达区变大**，而不是「某块石头没了」。
        //   用格数判、不用坐标判 → 计划文件里不出现硬编码坐标。
        "reachable_min" => {
            let n = crit_num(v, &["n", "cells", "min", "target"], 0.0) as i64;
            Some(snap.reachable_cells as i64 >= n)
        }
        // 任意一种建材达标
        "loose_any_min" => {
            let kg = crit_num(v, &["kg", "n", "min", "target"], 0.0);
            let els = v.get("elements").or_else(|| v.get("keys")).or_else(|| v.get("names"))?.as_array()?;
            // 兼容两种写法：["SandStone","Dirt"] 与 [{"element":"SandStone","kg":50}]
            let total: f64 = els
                .iter()
                .filter_map(|e| match e {
                    Value::String(s) => Some(snap.loose_kg(s)),
                    Value::Object(_) => {
                        let el = crit_str(e, &["element", "key", "name", "tag"])?;
                        let kg = crit_num(e, &["kg", "n", "min"], 0.0);
                        Some(snap.loose_kg(el).min(kg))
                    }
                    _ => None,
                })
                .sum();
            Some(total >= kg)
        }
        // 按 prefabId 判建筑数量（梯子/砖块/存储箱…）
        "prefab_min" => {
            let p = crit_str(v, &["prefab", "key", "id", "name"])?;
            Some(snap.building_count(p) as i64 >= crit_num(v, &["n", "count", "min", "target"], 1.0) as i64)
        }
        "food_min" => Some(snap.food_kcal() >= crit_num(v, &["kcal", "n", "min", "target"], 0.0)),
        // 换气完成的真实判据：复制人**能走到地表格**（可达区上界达到地表 Y）。
        //   用"地上散装煤 ≥ N kg"当判据是错的 —— 挖到煤层不等于打通了地表，
        //   会让依赖图提前放行下游节点。
        // 多材料同时达标（资源循环必须成套）：
        //   `elements` 是 [[元素ID, 最小kg], ...] —— 全部满足才算达成。
        //   为什么不分开判：只判"煤"会让"发电机"节点提前放行，
        //   但铜矿还没挖到 → 建造必失败 → 循环卡死（）。
        "loose_multi_min" => {
            let pairs = v.get("elements").or_else(|| v.get("pairs"))?.as_array();
            let Some(pairs) = pairs else { return None; };
            // 兼容两种写法：[["SandStone",50],...] 与 [{"element":"SandStone","kg":50}]
            let ok = pairs.iter().all(|p| match p {
                Value::Array(a) => {
                    let el = a.first().and_then(|x| x.as_str()).unwrap_or("");
                    let kg = a.get(1).and_then(|x| x.as_f64()).unwrap_or(0.0);
                    !el.is_empty() && snap.loose_kg(el) >= kg
                }
                Value::Object(_) => {
                    let el = crit_str(p, &["element", "key", "name", "tag"]).unwrap_or("");
                    let kg = crit_num(p, &["kg", "n", "min"], 0.0);
                    !el.is_empty() && snap.loose_kg(el) >= kg
                }
                _ => false,
            });
            Some(ok)
        }
        // 救援判据：所有复制人的呼吸值都高于阈值（说明都脱离了窒息区）。
        "breath_min" => {
            let min_v = crit_num(v, &["value", "n", "min", "target"], 50.0);
            let lowest = snap.min_breath();
            match lowest {
                Some(b) => Some(b >= min_v),
                None => None,   // 没有复制人数据 → 无法判定
            }
        }
        "surface_open" => {
            let min_y = crit_num(v, &["minY", "y", "n", "min", "target"], 216.0) as i32;
            Some(snap.reach_bounds.map(|(_, _, _, y1)| y1 >= min_y).unwrap_or(false))
        }
        // 开局侦察是否完成（报告落在 ~/.oni-agent/scout.json）
        "scout_done" => Some(
            std::path::Path::new(&oni_mcp::save::save_file("scout.json")).exists(),
        ),
        // 食物续航判据（周期）：开局必然不达标 → 代理会去采食物；吃饱了才 done。
        "food_cycles_min" => {
            let need = crit_num(v, &["cycles", "n", "min", "target"], 0.0);
            Some(food_runway_cycles(snap.food_kcal(), snap.dupes.len()) >= need)
        }
        // 空气质量判据：O₂/(O₂+CO₂) 的百分比。换气做没做成，看这个比例。
        "breath_ratio_min" => {
            let pct = crit_num(v, &["pct", "percent", "n", "min", "target"], 85.0);
            Some(snap.breath_ratio() * 100.0 >= pct)
        }
        "manual" => None,
        _ => None,
    }
}

/// 该节点的某条 `requires` 前置是否**已不可执行**（熔断 blocked / 暂缓 deferred）。
///
/// ★ 这是"保命被改成执行注定失败的前置"的判据。
///   紧急抢占会沿 requires 回溯到未完成的前置；但若该前置已被熔断或暂缓，
///   依赖链走不通 —— 此时保命**不能等它**，必须改走内置反射弧。
///   实测：T2「卫生前置：三人厕所」requires T1「采建材」；T1 熔断后，
///   Urgent 建厕所变成"执行 T1" → 厕所永远建不成 → 膀胱爆表。
pub fn has_unexecutable_predecessor(node: &crate::decide::Node, plan: &[crate::decide::Node]) -> bool {
    node.requires.iter().any(|r| {
        plan.iter()
            .find(|m| &m.id == r)
            .map(|pre| pre.is_blocked() || pre.is_deferred())
            .unwrap_or(false)
    })
}

/// ★ **保命反射弧**：把 Urgent 动作翻译成**不依赖 plan 依赖图**的具体动作。
///
/// 为什么需要它独立于 plan：
///   保命（膀胱爆/窒息/缺食）**不能等前置**。plan 的 requires 是战略顺序，
///   但当 Urgent 目标的某个前置已被熔断（blocked）或暂缓（deferred）时，
///   "尊重依赖图"会把保命动作改写成执行那个注定失败的前置 ——
///   实测：T2「卫生前置：三人厕所」requires T1「采建材」，T1 熔断后 →
///   Urgent 建厕所变成"执行 T1" → 厕所永远建不成 → 膀胱爆表。
///
/// 范围严格限定为保命最小集；供电/扩张类属战略层，不在此列。
/// 动作本身用语义目标（如 `toilet`），由运行时探测 prefab 与落点，不写死坐标。
pub fn vital_reflex_actions(action: crate::priority::Action) -> Vec<crate::exec::Action> {
    match action {
        crate::priority::Action::BuildToilet => vec![crate::exec::Action::Auto {
            goal: "toilet".into(),
            why: "膀胱告急 → 内置保命反射弧（语义目标 toilet，运行时选 prefab/落点）".into(),
        }],
        crate::priority::Action::Rescue => vec![crate::exec::Action::Auto {
            goal: "mop_dirty".into(),
            why: "有人受污染威胁 → 内置保命反射弧清污".into(),
        }],
        crate::priority::Action::ProduceOxygen | crate::priority::Action::Ventilate => {
            vec![crate::exec::Action::Auto {
                goal: "oxygen".into(),
                why: "氧气红线 → 内置保命反射弧产氧".into(),
            }]
        }
        crate::priority::Action::ForageFood => vec![crate::exec::Action::Auto {
            goal: "forage".into(),
            why: "食物告急 → 内置保命反射弧采集".into(),
        }],
        crate::priority::Action::Power | crate::priority::Action::FollowPlan => Vec::new(),
    }
}

/// 把节点写回 todo（解除熔断 / 保命节点重试）。
pub fn mark_node_todo(plan_path: &str, id: &str) {
    let Ok(text) = std::fs::read_to_string(plan_path) else { return; };
    let Ok(mut v) = serde_json::from_str::<Value>(&text) else { return };
    if let Some(nodes) = v.get_mut("nodes").and_then(|n| n.as_array_mut()) {
        for n in nodes.iter_mut() {
            if n.get("id").and_then(|i| i.as_str()) == Some(id) {
                if let Some(o) = n.as_object_mut() {
                    o.insert("status".into(), Value::from("todo"));
                    o.remove("blocked_reason");
                }
            }
        }
    }
    let _ = std::fs::write(plan_path, serde_json::to_string_pretty(&v).unwrap_or_default());
}

/// 把节点标记为 blocked（熔断）：不再被选中，等人工或换方案。
pub fn mark_node_blocked(plan_path: &str, id: &str) {
    let Ok(text) = std::fs::read_to_string(plan_path) else { return; };
    let Ok(mut v) = serde_json::from_str::<serde_json::Value>(&text) else { return; };
    if let Some(nodes) = v.get_mut("nodes").and_then(|n| n.as_array_mut()) {
        for n in nodes.iter_mut() {
            if n.get("id").and_then(|i| i.as_str()) == Some(id) {
                if let Some(o) = n.as_object_mut() {
                    o.insert("status".into(), serde_json::Value::from("blocked"));
                    o.insert("blocked_reason".into(), serde_json::Value::from("连续无进展，代理自动熔断"));
                }
            }
        }
    }
    if let Ok(s) = serde_json::to_string_pretty(&v) {
        let _ = std::fs::write(plan_path, s + "\n");
    }
}

/// 把节点标记为**暂缓**（外部条件未满足，如被研究锁）。
///
/// 与 mark_node_blocked（熔断）的区别：暂缓**不是**代理的方案失败，
///   只是"现在造不了" → 不消耗熔断额度，也不该让代理反复尝试。
pub fn mark_node_deferred(plan_path: &str, id: &str, reason: &str, cycle: u64) {
    let Ok(text) = std::fs::read_to_string(plan_path) else { return; };
    let Ok(mut v) = serde_json::from_str::<Value>(&text) else { return; };
    if let Some(nodes) = v.get_mut("nodes").and_then(|n| n.as_array_mut()) {
        for n in nodes.iter_mut() {
            if n.get("id").and_then(|i| i.as_str()) == Some(id) {
                if let Some(o) = n.as_object_mut() {
                    o.insert("deferred".into(), Value::from(reason));
                    o.insert("deferred_at".into(), Value::from(cycle));
                }
            }
        }
    }
    let _ = std::fs::write(plan_path, serde_json::to_string_pretty(&v).unwrap_or_default());
}

/// 记录节点「连续未达成判据」的轮数（stall 计数）。
///
/// ★ 为什么需要（实测事故）：
///   依赖图按 (priority, unlocks 数, 文件顺序) 选节点，
///   一个"解锁下游多"的节点（AIR unlocks=5）只要每轮都能"做点什么"
///   （下了一格挖掘工单）就**永远压住**解锁数少的节点（P1 供电 unlocks=3）。
///   实测本局 P1 被选中 **0 次**（AIR 166 次 / T1 139 次）→
///   供电节点从没跑 → 氧气扩散器建成了但**没有电**（净 -120W、consumerOperational=0）
///   → 造出来 ≠ 在产氧 → 基地耗尽。
///   正解：连续没达成的节点计数累加，超限就让位（见 decide::STALL_LIMIT）。
pub fn mark_node_stall(plan_path: &str, id: &str, stall: u64) {
    let Ok(text) = std::fs::read_to_string(plan_path) else { return; };
    let Ok(mut v) = serde_json::from_str::<Value>(&text) else { return; };
    if let Some(nodes) = v.get_mut("nodes").and_then(|n| n.as_array_mut()) {
        for n in nodes.iter_mut() {
            if n.get("id").and_then(|i| i.as_str()) == Some(id) {
                if let Some(o) = n.as_object_mut() {
                    o.insert("stall".into(), Value::from(stall));
                }
            }
        }
    }
    let _ = std::fs::write(plan_path, serde_json::to_string_pretty(&v).unwrap_or_default());
}

/// 清零节点的 stall 计数（达成判据时）。
pub fn clear_node_stall(plan_path: &str, id: &str) {
    mark_node_stall(plan_path, id, 0);
}

/// **复核已 done 的节点**：判据现在不成立 → 打回 todo。
///
/// ★ 为什么必须有（实测本局两个铁证）：
///   1) E1「地表换气（无限空气，替代被锁的电解器）」status=done，
///      判据 surface_open(minY=216)，而实际可达上界只有 194 → **从来没通过过**；
///   2) B1「床位」status=done，判据 building_min(beds>=2)，
///      而 diagnostics.buildings.beds = 0 → **也是假的**。
///   也就是说：plan 里的 status=done 可以**未经任何校验**就存在（AI 写的计划自带）。
///   快系统信任它 → 依赖图认为"换气已完成、氧气有着落"
///   → **再没有任何节点要求产氧** → 基地慢慢耗尽。
///   正解：done 必须有**证据**。每轮用同一套判据复核，不成立就打回。
pub fn recheck_done_nodes(plan_path: &str, snap: &crate::sense::Snapshot) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(plan_path) else { return Vec::new(); };
    let Ok(v) = serde_json::from_str::<Value>(&text) else { return Vec::new(); };
    let Some(nodes) = v.get("nodes").and_then(|n| n.as_array()) else { return Vec::new(); };
    let mut bad = Vec::new();
    for n in nodes {
        if n.get("status").and_then(|s| s.as_str()) != Some("done") {
            continue;
        }
        let Some(id) = n.get("id").and_then(|i| i.as_str()) else { continue; };
        let Some(check) = n.get("success_check") else { continue; };
        // 只复核**机器可判**的判据；人工判据（kind=manual）跳过。
        match check_success(snap, check) {
            Some(false) => bad.push(id.to_string()),
            _ => {}
        }
    }
    for id in &bad {
        mark_node_todo(plan_path, id);
        clear_node_stall(plan_path, id);
    }
    bad
}

/// 解除节点的暂缓状态。
pub fn clear_node_deferred(plan_path: &str, id: &str) {
    let Ok(text) = std::fs::read_to_string(plan_path) else { return; };
    let Ok(mut v) = serde_json::from_str::<Value>(&text) else { return; };
    if let Some(nodes) = v.get_mut("nodes").and_then(|n| n.as_array_mut()) {
        for n in nodes.iter_mut() {
            if n.get("id").and_then(|i| i.as_str()) == Some(id) {
                if let Some(o) = n.as_object_mut() {
                    o.remove("deferred");
                }
            }
        }
    }
    let _ = std::fs::write(plan_path, serde_json::to_string_pretty(&v).unwrap_or_default());
}

pub fn mark_node_done(plan_path: &str, id: &str) {
    let Ok(text) = std::fs::read_to_string(plan_path) else { return };
    let Ok(mut v) = serde_json::from_str::<Value>(&text) else { return };
    if let Some(nodes) = v.get_mut("nodes").and_then(|n| n.as_array_mut()) {
        for n in nodes.iter_mut() {
            if n.get("id").and_then(|i| i.as_str()) == Some(id) {
                // 救援节点是**常驻能力**，不是可完成的里程碑 → 永不标 done。
                //   R2 的判据 breath_min 98 在人健康时达成
                //   被标 done → 之后 Urgent 报「找不到匹配节点（动作 Rescue）」
                //   → 紧急态被静默降级。在这一处拦截即可覆盖所有调用点。
                if n.get("rescue").and_then(|r| r.as_bool()) == Some(true) {
                    continue;
                }
                n["status"] = Value::String("done".into());
            }
        }
    }
    let _ = std::fs::write(plan_path, serde_json::to_string_pretty(&v).unwrap_or_default());
}

#[cfg(test)]
mod tests {
/// ★ **done 必须有证据**：判据现在不成立的 done 节点要被打回 todo。
    ///
    /// 实测本局铁证：E1「地表换气」done 但可达上界只有 194（判据要 216）、
    /// B1「床位」done 但 beds=0 → 依赖图以为"换气与床位都完成"
    /// → 再没有节点要求产氧 → 基地慢慢耗尽。
    #[test]
    fn done_node_with_failing_criterion_is_reopened() {
        let tmp = std::env::temp_dir().join(format!("oni_recheck_{}.json", std::process::id()));
        let plan = serde_json::json!({
            "nodes": [
                {"id": "E1", "status": "done", "success_check": {"kind": "oxygen_min", "kg": 999999.0}},
                {"id": "T1", "status": "done", "success_check": {"kind": "dupe_min", "n": 0}}
            ]
        });
        std::fs::write(&tmp, serde_json::to_string_pretty(&plan).unwrap()).unwrap();
        let snap = crate::sense::Snapshot::default();
        let bad = super::recheck_done_nodes(tmp.to_str().unwrap(), &snap);
        assert_eq!(bad, vec!["E1".to_string()], "判据不成立的 done 节点应被打回");
        let after: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&tmp).unwrap()).unwrap();
        let st = after["nodes"][0]["status"].as_str().unwrap();
        assert_eq!(st, "todo", "E1 应被打回 todo");
        let st2 = after["nodes"][1]["status"].as_str().unwrap();
        assert_eq!(st2, "done", "判据成立的 done 节点应保持");
        let _ = std::fs::remove_file(&tmp);
    }

    /// ★ 回归：保命动作**不能被改成执行注定失败的前置**。
    ///   场景：T2「卫生前置：三人厕所」requires T1「采建材」，T1 被熔断（blocked）。
    ///   旧行为：紧急抢占沿 requires 回溯 → 执行 T1 → 零进展 → 厕所永远建不成 → 膀胱爆表。
    ///   正解：识别出前置不可执行 → 改走内置保命反射弧（不依赖 plan 依赖图）。
    #[test]
    fn unexecutable_predecessor_blocks_urgent_from_waiting_on_it() {
        use crate::decide::Node;
        let mk = |id: &str, status: &str, requires: Vec<&str>| Node {
            id: id.into(),
            name: id.into(),
            status: status.into(),
            requires: requires.into_iter().map(|s| s.to_string()).collect(),
            unlocks: vec![],
            actions: vec![],
            success_check: None,
            success: String::new(),
            rescue: false,
            note: String::new(),
            priority: None,
            deferred: None,
            deferred_at: None,
            stall: None,
        };
        let t1_blocked = mk("T1", "blocked", vec![]);
        let t2_toilet = mk("T2", "todo", vec!["T1"]);
        let plan = vec![t1_blocked.clone(), t2_toilet.clone()];
        // T2 的前置 T1 已熔断 → 依赖链走不通 → 必须识别出来
        assert!(
            super::has_unexecutable_predecessor(&t2_toilet, &plan),
            "前置被熔断时必须识别出依赖链走不通，否则保命会被改成执行失败的前置"
        );
        // T1 自己没有前置 → 不算走不通
        assert!(!super::has_unexecutable_predecessor(&t1_blocked, &plan));
        // 前置正常（todo）→ 不算走不通，保命仍可按依赖图推进
        let t1_ok = mk("T1", "todo", vec![]);
        let t2b = mk("T2", "todo", vec!["T1"]);
        assert!(!super::has_unexecutable_predecessor(&t2b, &[t1_ok, t2b.clone()]));
    }

    /// ★ 回归：保命反射弧覆盖所有 Urgent 保命动作，且用语义目标（不写死 prefab/坐标）。
    #[test]
    fn vital_reflex_covers_urgent_actions_with_semantic_goals() {
        use crate::priority::Action;
        for (act, expect_goal) in [
            (Action::BuildToilet, "toilet"),
            (Action::Rescue, "mop_dirty"),
            (Action::ProduceOxygen, "oxygen"),
            (Action::Ventilate, "oxygen"),
            (Action::ForageFood, "forage"),
        ] {
            let acts = super::vital_reflex_actions(act);
            assert_eq!(acts.len(), 1, "每个保命动作都应有且只有一个反射弧动作");
            match &acts[0] {
                crate::exec::Action::Auto { goal, .. } => assert_eq!(goal, expect_goal),
                other => panic!("反射弧必须用语义目标（不写死 prefab/坐标），实际: {other:?}"),
            }
            // 语义目标必须能被 parse_goal 解析，否则运行时直接失败
            assert!(
                crate::autoact::parse_goal(expect_goal).is_some(),
                "反射弧的语义目标 {expect_goal} 必须可解析"
            );
        }
        // 供电/扩张类不属于保命最小集 → 不做反射弧
        assert!(super::vital_reflex_actions(Action::Power).is_empty());
        assert!(super::vital_reflex_actions(Action::FollowPlan).is_empty());
    }

    /// ★ 回归：判据字段别名容错 —— LLM 猜错字段名也不能让判据失效。
    ///   实测事故：LLM 把 `loose_min` 写成 `{key:"SandStone", n:50}`（照 building_min 的例子猜），
    ///   而代码只认 `{element, kg}` → 判据永远返回 None → 节点永远不算 done →
    ///   零进展 → 被熔断 → 下游「建厕所」节点前置断链 → 膀胱爆表。
    #[test]
    fn success_check_accepts_alias_field_names() {
        use crate::sense::Snapshot;
        let snap = Snapshot::default();
        // 规范写法（element + kg）
        assert_eq!(
            super::check_success(&snap, &serde_json::json!({"kind":"loose_min","element":"SandStone","kg":50})),
            Some(false), // 默认快照没有散装量 → 不满足，但不是 None（判据可判定）
            "规范写法的判据必须返回 Some（可判定），不能是 None"
        );
        // ★ LLM 实际写法（key + n）——必须同样可判定，不能是 None
        let alias = super::check_success(
            &snap,
            &serde_json::json!({"kind":"loose_min","key":"SandStone","n":50}),
        );
        assert!(
            alias.is_some(),
            "LLM 用 key/n 写的 loose_min 判据必须可判定；返回 None 会让节点永远无法 done"
        );
        // 其他常见别名同样要可判定
        for v in [
            serde_json::json!({"kind":"loose_min","name":"SandStone","min":50}),
            serde_json::json!({"kind":"building_min","key":"toilets","n":3}),
            serde_json::json!({"kind":"building_min","key":"toilets","count":3}),
            serde_json::json!({"kind":"reachable_min","n":200}),
            serde_json::json!({"kind":"reachable_min","cells":200}),
            serde_json::json!({"kind":"oxygen_min","kg":100}),
            serde_json::json!({"kind":"oxygen_min","n":100}),
            serde_json::json!({"kind":"food_cycles_min","cycles":5}),
            serde_json::json!({"kind":"food_cycles_min","n":5}),
        ] {
            assert!(
                super::check_success(&snap, &v).is_some(),
                "判据 {:?} 必须可判定（不能返回 None）",
                v
            );
        }
    }

    /// ★ 回归：节点匹配的 tag **不能包含动作的 why 文案**。
    ///   实测事故：T1「浅层建材自举」的 why 写了"厕所、梯子、砖块都需要建材"，
    ///   Urgent「建厕所」因此把 T1（采建材）当成卫生节点 → 每轮复活它、
    ///   真正该跑的 T2「卫生前置：三人厕所」永远轮不上。
    #[test]
    fn action_identity_excludes_why_text() {
        use crate::exec::Action;
        let a = Action::Auto {
            goal: "material".into(),
            why: "厕所、梯子、砖块都需要建材，先采浅层砂岩".into(),
        };
        let id = super::action_identity(&a);
        assert!(id.contains("material"), "应保留动作语义目标：{id}");
        assert!(!id.contains("厕所"), "why 里的词绝不能进入匹配 tag：{id}");
        assert!(!id.contains("梯子"), "why 里的词绝不能进入匹配 tag：{id}");
    }

    /// 动作指纹提取：从 detail 里认出"对哪个目标动了手"。
    #[test]
    fn action_fingerprint_extracts_target_from_detail() {
        let mk = |kind: &str, detail: &str| crate::exec::ActionResult {
            kind: kind.into(), why: String::new(), ok: true, detail: detail.into(),
        };
        // 工单落点（@ 坐标）优先
        assert_eq!(
            super::action_fingerprint(&mk("dig", "为 户外厕所 腾出空间：落点 (116,194) 还差实心岩，先挖 (116,195) → 已下挖掘工单 @(116,195)（ok: true）")),
            Some("dig@116,195".to_string())
        );
        // 建造落点
        assert_eq!(
            super::action_fingerprint(&mk("build", "已放置 Outhouse @(119,194) ok")),
            Some("build@119,194".to_string())
        );
        // 没有坐标 → 无指纹（不参与循环判定）
        assert_eq!(super::action_fingerprint(&mk("scout", "侦察完成")), None);
    }

    /// 结构化失败分类：把自由文本 detail 归为稳定类别，供慢系统诊断根因。
    #[test]
    fn classify_fail_maps_common_failure_modes() {
        assert_eq!(super::classify_fail("语义目标在当前地图上找不到（oxygen）→ 换方案"), "no_target");
        assert_eq!(super::classify_fail("✖ 造 Electrolyzer 缺建材（mod: missing）→ 先采建材"), "no_material");
        assert_eq!(super::classify_fail("预检未通过，未执行: 材料不足"), "preflight");
        assert_eq!(super::classify_fail("落地失败: ok: false"), "rejected");
        assert_eq!(super::classify_fail("build 被研究锁（需要先研究）"), "research_locked");
        assert_eq!(super::classify_fail("区域内找不到能放 的落点"), "no_placement");
        assert_eq!(super::classify_fail("无法解析的 goal: xxx"), "bad_goal");
        assert_eq!(super::classify_fail("某种全新错误"), "unknown");
    }

    /// B 自救：清锁必须只删 surface/open_chamber，保留其它锁（防误伤）。
    #[test]
    fn strip_stuck_locks_keeps_other_goals() {
        let mut v = serde_json::json!({
            "surface": {"x": 137, "y": 197},
            "open_chamber": {"x": 140, "y": 192},
            "element:Algae": {"x": 120, "y": 200},
            "food": {"x": 118, "y": 202}
        });
        assert_eq!(super::strip_stuck_locks(&mut v), 2);
        assert!(v.get("surface").is_none());
        assert!(v.get("open_chamber").is_none());
        assert!(v.get("element:Algae").is_some(), "元素锁不能被误删");
        assert!(v.get("food").is_some(), "食物锁不能被误删");
        // 没有卡锁时返回 0
        assert_eq!(super::strip_stuck_locks(&mut v), 0);
    }
    use super::*;

    /// 空闲即冻结的判据：没有待办工单 **且** 没有施工中建筑 → 应该冻结。
    /// 有待办工单（挖掘标记/建造蓝图）→ 复制人还在干活 → 让世界继续跑。
    #[test]
    fn idle_freeze_decision() {
        let work = |pending: Option<i64>, building: i64| pending.unwrap_or(0) > 0 || building > 0;
        // 没有任何待办 → 空闲 → 冻结
        assert!(!work(Some(0), 0));
        assert!(!work(None, 0));
        // 有挖掘标记 → 有活 → 继续跑
        assert!(work(Some(3), 0));
        // 有建筑在施工 → 有活 → 继续跑
        assert!(work(Some(0), 2));
        assert!(work(None, 1));
    }

    /// hold 标记：存在时守护进程只观察，不自动恢复。
    #[test]
    fn hold_roundtrip() {
        let _ = clear_hold();
        assert!(!hold_active());
        write_hold("test").expect("write hold");
        assert!(hold_active());
        clear_hold();
        assert!(!hold_active());
    }

    /// 食物判据必须是"续航"而不是绝对值。
    /// 绝对 kcal 在新档开局就满足 → 节点秒 done → 代理永远不采食物（）。
    #[test]
    fn food_runway_not_satisfied_at_start() {
        let food = 16000.0;
        let dupes = 3;
        let cycles = food_runway_cycles(food, dupes);
        // 修正：ONI 官方 400 kcal/人/周期。
        //   16000 ÷ 3 ÷ 400 ≈ 13.3 周期 —— 开局粮食充裕，断粮不是红线。
        assert!((cycles - 13.33).abs() < 0.1, "开局续航应约 13.3 周期，实际 {cycles}");
        assert!(food_runway_cycles(30000.0, dupes) > cycles);
        assert!(food_runway_cycles(16000.0, 6) < cycles);
    }

    /// 空气质量判据：开局 70% 不达标；换气（CO₂ 被稀释）后达标。
    #[test]
    fn breath_ratio_min_reflects_ventilation() {
        let v = serde_json::json!({"kind": "breath_ratio_min", "pct": 85.0});
        let before: Snapshot = serde_json::from_value(serde_json::json!({
            "atmos": {"oxygenMassKg": 401.0, "carbonDioxideMassKg": 172.0}
        }))
        .unwrap();
        assert_eq!(check_success(&before, &v), Some(false));
        let after: Snapshot = serde_json::from_value(serde_json::json!({
            "atmos": {"oxygenMassKg": 900.0, "carbonDioxideMassKg": 100.0}
        }))
        .unwrap();
        assert_eq!(check_success(&after, &v), Some(true));
    }
}