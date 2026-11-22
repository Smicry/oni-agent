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
    /// 手动暂停是否自动恢复（**默认 false：绝不越权恢复用户的暂停**）
    pub auto_resume_manual: bool,
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
            auto_resume_manual: false,
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
    /// 用户手动暂停（代理不得越权恢复）
    user_paused: bool,
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
    /// **因「等慢系统」而冻结世界**（有 pending escalation / 待处理用户指令）。
    ///
    /// 与其它暂停标志严格区分：
    ///   - user_paused   = 用户手动暂停 → 代理**绝不**恢复；
    ///   - frozen        = 施工窗口正常到期 → 可自持续窗；
    ///   - idle_stopped  = 空跑停机 → 不得自动恢复；
    ///   - escalation_hold = 等慢系统 → 只能由慢系统 ack 或看门狗解除。
    escalation_hold: bool,
    /// 因「等慢系统」冻结的起点（看门狗用：太久没人处理就降级继续，避免殖民地饿死）
    escalation_since: Option<Instant>,
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
    replan_cooldown: u64,
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
    /// **看门狗**：等慢系统超过这么多秒还没处理 → 降级解除冻结（避免殖民地饿死）。
    ///
    /// 慢系统可能离线/崩溃/被打断；没有看门狗 → 世界永远冻着 → 殖民地慢慢死。
    const ESCALATION_WATCHDOG_SECS: u64 = 300;
    pub fn new(session: McpSession, catalog: Catalog, cfg: AgentConfig) -> Self {
        Self {
            session,
            catalog,
            cfg,
            paused_by_us: false,
            pause_until: Instant::now(),
            last_danger: String::new(),
            window_until: None,
            user_paused: false,
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
            escalation_hold: false,
            escalation_since: None,
            stuck_streak: std::collections::HashMap::new(),
            replan_cooldown: 0,
            last_fingerprint: String::new(),
            left_running: false,
            no_progress: 0,
            action_failures: 0,
            cycles: 0,
        }
    }

    /// 连续动作失败多少次 → 暂缓该节点（换方案）。
    ///
    /// 为什么是 2 而不是 3：建造失败往往是**结构性问题**（空间不够 / 被研究锁），
    ///   不会因为多试几次就好 → 快速暂缓比死磕 3 轮更省时间。
    ///   熔断阈值是 3（因为挖掘可能只是慢），动作失败阈值是 2（因为建造是确定性的）。
    const ACTION_FAIL_MAX: usize = 2;

    /// 暂缓节点的冷却期（轮）：暂缓后至少隔这么多轮才允许自动恢复。
    ///
    /// 为什么需要：没有冷却 → 暂缓 → 下一轮前置仍满足 → 立刻恢复 → 又失败
    ///   又暂缓…… 代理在同一节点上空转。
    const DEFER_COOLDOWN: u64 = 8;

    fn game_paused(&self) -> bool {
        self.session
            .read_json("oni://colony/status")
            .ok()
            .and_then(|v| v.get("isPaused").and_then(|b| b.as_bool()))
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
        if backlog && everyone_unproductive && self.idle_streak_unworked >= 2
            && !self.escalation_hold
        {
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
            // 世界本就被 hook 冻住时不要越权改 owner；只在运行时主动冻结。
            if !snap.paused {
                self.pause("work backlog + all idle → freeze for AI judgment");
                self.window_until = None;
                self.window_owned_by_us = false;
            }
            self.escalation_hold = true;
            self.escalation_since = Some(Instant::now());
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
                    oxygen_kg: snap.oxygen_kg(), food_kcal: snap.food_kcal() },
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
        {
            // 趋势在这里自算一次（load_samples 是读文件、compute 是纯计算，都很便宜）
            let tr_local = compute(&load_samples(200), &snap);
            let o2_left = tr_local.oxygen_cycles_left;
            let reason = if pop_dropped {
                Some("人口下降")
            } else {
                crate::planner::should_replan(self.last_pop, snap.dupes.len(), 0, o2_left)
                    .filter(|r| *r == "氧气续航告急")
            };
            if let Some(reason) = reason {
                if self.replan_cooldown == 0 {
                    let facts = crate::planner::collect_facts(&self.session, &snap);
                    let (mut new_plan, strategy) = crate::planner::build_plan(&facts);
                    if let Ok(old) = load_plan(std::path::Path::new(&self.cfg.plan_path)) {
                        crate::planner::merge_done(&mut new_plan, &old);
                    }
                    // Slow system owns strategy; mechanical replan only suggests.
                    self.replan_cooldown = 60;
                    let plan_json = serde_json::to_string(&new_plan).unwrap_or_default();
                    crate::decisions::suggest(&reason, strategy.label(), &plan_json);
                        report.notes.push(format!(
                            "🔄 重规划建议（{}）→ {}（{} 节点）已写入 suggestions.jsonl，等 AI 审阅（不覆盖 plan.json）",
                            reason,
                            strategy.label(),
                            new_plan.nodes.len()
                        ));
                }
            } else if self.replan_cooldown > 0 {
                self.replan_cooldown -= 1;
            }
        }

        // =====================================================================
        // 2) 运行状态机：这一轮世界在不在跑
        //    铁律：**世界不跑 ⇒ 不下工单、不累计进展、不熔断。**
        // =====================================================================
        let observed_running = !snap.paused;
        // 暂停所有权恢复：
        //   守护进程换二进制重启后内存标志全清零，但 control.json 还在。
        //   若持久化的 owner=User（hook/用户暂停）→ 恢复 user_paused，绝不越权恢复；
        //   否则不臆造"用户暂停"。
        let ctl = crate::control::read();
        if ctl.owner == crate::control::Owner::User {
            self.user_paused = true;
        }
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
            self.window_until = None;
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
            self.user_paused = false;
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
            //   - 只有"代理自己冻的"(frozen) 才自持；用户手动暂停时 frozen=false → 不自持
            //   - "真没活干"（idle_stopped / 动作失败无待办）→ 不自持
            //   - 有 hold 标记（主 agent 改代码中）→ 上面已早退，不会走到这里
            //   - 连续自持上限 RHYTHM_MAX：即使误判也不会长期越权暂停用户的游戏
            const RHYTHM_MAX: usize = 4;
            let rhythm_due = self.auto_rhythm
                && self.frozen
                && !self.user_paused
                && !self.idle_stopped
                && self.rhythm_streak < RHYTHM_MAX
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
            } else if self.frozen || self.idle_stopped {
                report.notes.push("🔒 已冻回暂停（等新窗口请求）".into());
            } else if self.paused_by_us && !self.escalation_hold {
                // 上轮紧急暂停、条件已解除 → 开一个短窗口，而不是一直跑
                // **等慢系统期间不能在这里恢复**。
                //   旧逻辑：paused_by_us 就恢复 → 下一轮「等慢系统」又冻回
                //   两个分支每 10s 互相打架 → 世界一半在跑、一半冻住
                //   殖民地零进展。
                //   现在：等慢系统时保持冻结，由看门狗统一解冻。
                self.window_until = Some(Instant::now() + Duration::from_secs(self.cfg.work_window_secs));
                self.window_owned_by_us = true;
                self.resume("危急已解除 → 开施工窗口");
                self.last_danger.clear();
                can_execute = true;
                report.notes.push("▶ 危急已解除 → 开施工窗口".into());
            } else {
                self.user_paused = true;
                report.notes.push(
                    "⏸ 用户手动暂停中 → 代理只观察待命，绝不越权恢复".into(),
                );
            }
        }

        // ---- 显式施工窗口请求----
        if let Some(secs) = read_window_request() {
            // 用户显式 `oni window` = 授权代理恢复：先清掉 user/hook 的暂停所有权。
            //   不清的话，下一轮守护进程读到 owner=User 又会判"用户暂停"→ 死锁。
            crate::control::clear();
            if judgment.verdict != Verdict::Danger {
                self.window_until = Some(Instant::now() + Duration::from_secs(secs));
                self.window_owned_by_us = true;
                self.paused_by_us = false;
                self.user_paused = false;
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
                self.user_paused = false;
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

        // ---- 执行慢系统的语义目标（B方案：慢系统定 goal，快系统 autoact 翻译成工单）----
        //   慢系统把决策 goal 写成 source=ai_goal 的 command；这里消费并执行。
        //   必须在 wait_slow 停手逻辑**之前**完成，否则 pending_cmd 会因为这条而停手。
        {
            let ai_goals: Vec<_> = crate::commands::pending().into_iter()
                .filter(|c| c.source == "ai_goal")
                .collect();
            for c in ai_goals {
                let goal = c.text.trim();
                if crate::autoact::parse_goal(goal).is_none() {
                    crate::commands::complete(c.id, &format!("无效语义目标: {}", goal));
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
                        let rec = serde_json::json!({
                            "goal": goal, "ok": ok, "detail": detail,
                            "ts": std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0),
                        });
                        let _ = writeln!(f, "{}", rec);
                    }
                }
                crate::commands::complete(c.id, &format!("ai_goal[{}] {}：{}", goal,
                    if ok { "已执行" } else { "未成功" }, detail));
                report.notes.push(format!("🎯 慢系统目标[{}]：{} → {}", goal,
                    if ok { "已执行" } else { "未成功" }, detail));
            }
        }

        // ---- 快慢分工：慢系统有未处理的升级请求时，**快系统主动停手** ----
        //   例外：有人倒地需要立刻救人 → 救援优先（紧急态允许继续动作）。
        //   其它情况：等慢系统改完计划再继续，避免带病施工浪费周期。
        // ---- 快慢交接：等慢系统期间**冻结世界**（不只是"不施工"）--------
        //   为什么必须冻结：慢系统分析 + 翻译指令要花几分钟；这段时间若世界照跑，
        //   复制人可能在毒气区继续干活 → 局势恶化 → 等慢系统接手时已经晚了。
        //   ONI 暂停会冻结倒计时 → 暂停期间人不耗氧、不被毒气伤害 → 冻结是安全的。
        //   例外：**有人倒地**（downed）→ 暂停会阻断救援（倒地的只能靠环境改善恢复）
        //     → 这种情况下走紧急态占空比，由上面的 Danger 分支处理。
        let downed = judgment.danger_keys.iter().any(|k| k == "downed");
        let pending_esc = crate::escalate::has_pending();
        let pending_cmd = !crate::commands::pending().is_empty();
        let waiting_slow = pending_esc || pending_cmd;
        if waiting_slow && !downed {
            let since = *self.escalation_since.get_or_insert_with(Instant::now);
            let waited = since.elapsed();
            // 看门狗：慢系统可能离线/崩溃 → **不能让殖民地永远冻死**。
            //   超时未处理 → 降级解除冻结，继续自主运行（问题仍留在清单里等人看）。
            if waited >= Duration::from_secs(Self::ESCALATION_WATCHDOG_SECS) {
                self.escalation_hold = false;
                self.escalation_since = None;
                crate::escalate::note_unhandled("看门狗超时，慢系统未处理");
                report.notes.push(format!(
                    "⏱ 等慢系统已 {}s 无人处理 → 看门狗降级：解除冻结，继续自主运行（问题仍留在清单）",
                    waited.as_secs()
                ));
            } else {
                // **不要为了等慢系统把世界冻住**。
                //   冻结的代价是整段游戏时间白烧：复制人不干活、空气照旧消耗、
                //   膀胱照旧涨 → 反而制造新危机。
                //   正确做法：**不下新工单**（等慢系统改计划），但世界继续跑，
                //   让已有工单做完；窗口到期仍按「有活续窗 / 空闲冻回」处理。
                if observed_running {
                    report.notes.push(
                        "⏸ 等慢系统处理 → 本轮不下新工单（世界继续跑，已有工单照常执行）".into(),
                    );
                }
                self.escalation_hold = true;
                // 保留施工窗口（不要 window_until=None）：世界继续跑，
                //   到点仍按「有活续窗 / 空闲冻回」处理 → 不会白烧周期。
                self.window_owned_by_us = true;
                if self.window_until.is_none() {
                    self.window_until =
                        Some(Instant::now() + Duration::from_secs(self.cfg.work_window_secs));
                }
                report.notes.push(format!(
                    "⏸ 等慢系统处理 → 世界已冻结（已等 {}s；看门狗 {}s 后降级继续）",
                    waited.as_secs(),
                    Self::ESCALATION_WATCHDOG_SECS
                ));
                if pending_cmd {
                    report.notes.push(crate::commands::summary());
                }
                if pending_esc {
                    report.notes.push(crate::escalate::summary());
                }
                return report;
            }
        }
        // 无待处理 → 解除「等慢系统」的冻结（慢系统 ack 后快系统自动继续干活）。
        if self.escalation_hold {
            self.escalation_hold = false;
            self.escalation_since = None;
            report.notes.push("✅ 慢系统已处理 → 解除冻结，快系统继续自主运行".into());
            // 开一个施工窗口让快系统真正动起来（**不覆盖用户手动暂停**）
            if !self.user_paused {
                self.resume("慢系统已处理，恢复施工");
                self.window_until = Some(Instant::now() + Duration::from_secs(self.cfg.work_window_secs));
                self.window_owned_by_us = true;
            }
        }

        // ---- 决策 + 执行 ----
        let plan = match load_plan(std::path::Path::new(&self.cfg.plan_path)) {
            Ok(p) => p,
            Err(e) => {
                report.notes.push(format!("计划读取失败: {e}"));
                return report;
            }
        };
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
        // C: 先 raise 再决定是否冻结 —— raise 返回 0 表示处于放行冷静期（同一误报
        //    已被 ack），此时**不冻结、不占 escalation_hold**，否则一条误报会
        //    无限重生、把世界反复冻住并压制其它看门。
        if directive.needs_ai {
            let first_time = !self.escalation_hold;
            if first_time {
                let esc_id = crate::escalate::raise(
                    crate::escalate::Kind::StrategyMismatch,
                    "",
                    format!("AI decision required: {}", directive.reason),
                    "Slow system: review situation and decide; world is frozen".into(),
                    crate::escalate::Ctx { cycle: snap.cycle, dupes: snap.dupes.len(),
                        oxygen_kg: snap.oxygen_kg(), food_kcal: snap.food_kcal() },
                );
                if esc_id == 0 {
                    // 放行冷静期内：当 Watch 处理，记日志继续自主运行，不打扰 AI。
                    report.notes.push(
                        "🔇 semantic 决策处于放行冷静期 → 不冻结、不重报（继续自主运行）".into(),
                    );
                } else {
                    if observed_running {
                        self.pause("semantic decision needed → freeze for AI");
                    }
                    self.escalation_hold = true;
                    self.escalation_since = Some(Instant::now());
                    self.window_until = None;
                    self.window_owned_by_us = false;
                    report.notes.push("⏸ semantic decision escalated to AI; no local action".into());
                    return report;
                }
            } else {
                report.notes.push("⏸ semantic decision escalated to AI; no local action".into());
                return report;
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
                            let tag = format!(
                                "{} {:?}",
                                n.name,
                                n.actions.iter().map(|a| format!("{:?}", a)).collect::<Vec<_>>().join(" ")
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
                    oxygen_kg: snap.oxygen_kg(), food_kcal: snap.food_kcal() },
            );
        }
        // 修正：**Urgent 抢占必须尊重依赖图**。
        //   旧行为：膀胱告急 → 直接选中"建厕所"节点 → 但它的前置（挖空间/铺砖）
        //   还没做 → 预检必不过 → 每轮失败 → 每轮升级 → 每轮冻结 → 噪声不断。
        //   正解：抢占到的节点若前置未完成 → **改为执行它的前置**（逐级回溯）。
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
                };
                crate::escalate::raise(
                    crate::escalate::Kind::Deadlock,
                    "",
                    format!(
                        "依赖图里没有可执行目标（{} 个节点全部 done/blocked/deferred）。快系统无法自行解开。",
                        plan.nodes.len()
                    ),
                    "建议慢系统：检查 requires 是否指向不存在的节点、或按当前局面重排依赖（oni plan-auto --keep）".into(),
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
                    //   旧逻辑：stuck → pause 冻结世界 + escalation_hold=true → 下一轮
                    //   守护进程见暂停又判「用户手动暂停」→ 谁都不恢复 → 只能手动 resume,
                    //   然后节点又失败又冻结 —— 无限循环，膀胱涨到 100%。
                    //   正解：**卡住时世界继续跑**（已有工单做完、膀胱/呼吸照常演化），
                    //   只在本轮不下新工单 + 升级给慢系统（问题仍在清单里）。
                    //   冻结只留给：真红线紧急态占空比 / 主 agent 的 hold。
                    if !rep.needs_running_world() && !self.escalation_hold {
                        self.escalation_hold = true;
                        self.escalation_since = Some(Instant::now());
                        report.notes.push(
                            "⏸ 卡住待慢系统 → 本轮不下新工单（世界继续跑，避免膀胱/空气红线恶化）".into(),
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
            // 动作失败且没有待办 → 冻回（等主 agent 介入，不空烧周期）
            self.pause("动作失败且无待办 → 冻回暂停，等待人工/主 agent 介入");
            self.paused_by_us = false;
            self.frozen = true;
            self.window_until = None;
            self.window_owned_by_us = false;
            // 真没活干 → **不自持**
            self.auto_rhythm = false;
            self.resume_at = None;
            report.notes.push(
                "🔒 动作失败且没有待办工单 → 冻回暂停（等主 agent 修代码/决策，不空烧周期）".into(),
            );
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
                        report.notes.push(format!(
                            "[{}] 成功判据达成 → 标记 done，下一轮换下一个目标",
                            node.id
                        ));
                    } else {
                        report.notes.push(format!(
                            "[{}] 动作已执行但成功判据未达成 → 下一轮继续",
                            node.id
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
                report.notes.push(format!(
                    "🆘 [{}] 保命节点连续失败 → 已升级给慢系统，本轮停手（不再无限重试）",
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
                && !self.escalation_hold
            {
                let who: Vec<String> = sustained.iter().map(|(n, c)| format!("{n}({c}轮)")).collect();
                let detail = format!("Sustained individual idleness [{}] while goals remain; assign work or replan", who.join(", "));
                self.pause("individual idle → freeze for AI work assignment");
                self.window_until = None;
                self.window_owned_by_us = false;
                self.escalation_hold = true;
                self.escalation_since = Some(Instant::now());
                let _ = crate::escalate::raise(
                    crate::escalate::Kind::NoProgress, "", detail.clone(),
                    "AI: assign work to idle dupes or replan".into(),
                    crate::escalate::Ctx { cycle: snap.cycle, dupes: snap.dupes.len(),
                        oxygen_kg: snap.oxygen_kg(), food_kcal: snap.food_kcal() },
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
        if self.idle_streak >= 2 && !snap.paused && self.window_owned_by_us {
            // Idle may mean "plan done" or "goals exist but nobody executes".
            // The fast system cannot judge which - freeze and escalate to AI.
            self.pause("all dupes idle → freeze for AI judgment");
            self.window_until = None;
            self.window_owned_by_us = false;
            self.idle_streak = 0;
            self.escalation_hold = true;
            self.escalation_since = Some(Instant::now());
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
                    oxygen_kg: snap.oxygen_kg(), food_kcal: snap.food_kcal() },
            );
            report.notes.push(format!("⏸ 全员空闲 → 冻结并上报 AI：{detail}"));

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

pub fn check_success(snap: &Snapshot, v: &Value) -> Option<bool> {
    let kind = v.get("kind")?.as_str()?;
    match kind {
        "building_min" => {
            let key = v.get("key")?.as_str()?;
            Some(snap.building(key) >= v.get("n")?.as_i64().unwrap_or(1))
        }
        "rooms_min" => {
            let key = v.get("key")?.as_str()?;
            Some(snap.rooms.get(key).and_then(|x| x.as_i64()).unwrap_or(0) >= v.get("n")?.as_i64().unwrap_or(1))
        }
        "storage_mass_min" => {
            let kg = v.get("kg")?.as_f64().unwrap_or(0.0);
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
        "oxygen_min" => Some(snap.oxygen_kg() >= v.get("kg")?.as_f64().unwrap_or(0.0)),
        // 地上散装资源够不够（建造前先确认材料够）
        "loose_min" => {
            let el = v.get("element")?.as_str()?;
            Some(snap.loose_kg(el) >= v.get("kg")?.as_f64().unwrap_or(0.0))
        }
        // 发电机真的在发电（不是"建好了但没燃料"）
        "generator_fueled" => Some(
            snap.power_int("generatorOperational") > 0 && snap.power_int("generatorEmpty") == 0,
        ),
        "power_net" => Some(snap.power_num("netActiveWatts") >= v.get("watts")?.as_f64().unwrap_or(0.0)),
        "dupe_min" => Some(snap.dupes.len() as i64 >= v.get("n")?.as_i64().unwrap_or(1)),
        // 基地扩容判据：官方可达格数。
        //   「挖通开阔气室」的真实效果 = **可达区变大**，而不是「某块石头没了」。
        //   用格数判、不用坐标判 → 计划文件里不出现硬编码坐标。
        "reachable_min" => {
            let n = v.get("n")?.as_i64().unwrap_or(0);
            Some(snap.reachable_cells as i64 >= n)
        }
        // 任意一种建材达标
        "loose_any_min" => {
            let kg = v.get("kg")?.as_f64().unwrap_or(0.0);
            let els = v.get("elements")?.as_array()?;
            let total: f64 = els
                .iter()
                .filter_map(|e| e.as_str())
                .map(|e| snap.loose_kg(e))
                .sum();
            Some(total >= kg)
        }
        // 按 prefabId 判建筑数量（梯子/砖块/存储箱…）
        "prefab_min" => {
            let p = v.get("prefab")?.as_str()?;
            Some(snap.building_count(p) >= v.get("n")?.as_i64().unwrap_or(1))
        }
        "food_min" => Some(snap.food_kcal() >= v.get("kcal")?.as_f64().unwrap_or(0.0)),
        // 换气完成的真实判据：复制人**能走到地表格**（可达区上界达到地表 Y）。
        //   用"地上散装煤 ≥ N kg"当判据是错的 —— 挖到煤层不等于打通了地表，
        //   会让依赖图提前放行下游节点。
        // 多材料同时达标（资源循环必须成套）：
        //   `elements` 是 [[元素ID, 最小kg], ...] —— 全部满足才算达成。
        //   为什么不分开判：只判"煤"会让"发电机"节点提前放行，
        //   但铜矿还没挖到 → 建造必失败 → 循环卡死（）。
        "loose_multi_min" => {
            let pairs = v.get("elements").and_then(|e| e.as_array());
            let Some(pairs) = pairs else { return None; };
            let ok = pairs.iter().all(|p| {
                let el = p.get(0).and_then(|x| x.as_str()).unwrap_or("");
                let kg = p.get(1).and_then(|x| x.as_f64()).unwrap_or(0.0);
                !el.is_empty() && snap.loose_kg(el) >= kg
            });
            Some(ok)
        }
        // 救援判据：所有复制人的呼吸值都高于阈值（说明都脱离了窒息区）。
        "breath_min" => {
            let min_v = v.get("value").and_then(|x| x.as_f64()).unwrap_or(50.0);
            let lowest = snap.min_breath();
            match lowest {
                Some(b) => Some(b >= min_v),
                None => None,   // 没有复制人数据 → 无法判定
            }
        }
        "surface_open" => {
            let min_y = v.get("minY").and_then(|y| y.as_i64()).unwrap_or(216) as i32;
            Some(snap.reach_bounds.map(|(_, _, _, y1)| y1 >= min_y).unwrap_or(false))
        }
        // 开局侦察是否完成（报告落在 ~/.oni-agent/scout.json）
        "scout_done" => Some(
            std::path::Path::new(&oni_mcp::save::save_file("scout.json")).exists(),
        ),
        // 食物续航判据（周期）：开局必然不达标 → 代理会去采食物；吃饱了才 done。
        "food_cycles_min" => {
            let need = v.get("cycles")?.as_f64().unwrap_or(0.0);
            Some(food_runway_cycles(snap.food_kcal(), snap.dupes.len()) >= need)
        }
        // 空气质量判据：O₂/(O₂+CO₂) 的百分比。换气做没做成，看这个比例。
        "breath_ratio_min" => {
            let pct = v.get("pct")?.as_f64().unwrap_or(85.0);
            Some(snap.breath_ratio() * 100.0 >= pct)
        }
        "manual" => None,
        _ => None,
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