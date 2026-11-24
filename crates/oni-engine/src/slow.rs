//! **慢系统服务**（slow system）—— 常驻决策者，通过文件总线与快系统协同。
//!
//! 快系统（oni agent）每10s采快照、发现卡点，写结构化 escalation + live_snapshot。
//! 慢系统（本模块）常驻轮询：escalation → 调 LLM 从候选里选 → 执行 → 留痕。
//! 用户指令（oni command add 入队）：LLM 决策 → 执行 → 回执 → 恢复自主。
//! LLM 可插拔：ONI_LLM_ENDPOINT / ONI_LLM_KEY / ONI_LLM_MODEL，无配置用 mock。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct SlowConfig {
    pub endpoint: Option<String>,
    pub api_key: Option<String>,
    pub model: String,
    /// "anthropic" (Messages API) or "openai" (chat/completions)
    pub api_format: String,
    /// 自定义 headers（如 User-Agent），格式 "K:V,K2:V2"
    pub extra_headers: String,
    pub interval_secs: u64,
    pub web_port: u16,
}
impl Default for SlowConfig {
    fn default() -> Self {
        let endpoint = std::env::var("ONI_LLM_ENDPOINT").ok();
        let api_key = std::env::var("ONI_LLM_KEY").ok();
        let model = std::env::var("ONI_LLM_MODEL").ok();
        let api_format = std::env::var("ONI_LLM_FORMAT").ok();
        // 环境变量不全 → 尝试从 MCode 的 config.yaml 读 custom_provider（用户在设置界面填的）
        let (endpoint, api_key, model, api_format, extra_headers) =
            if endpoint.is_some() && api_key.is_some() && model.is_some() {
                (endpoint, api_key, model.unwrap(),
                 api_format.unwrap_or_else(|| "openai".into()), String::new())
            } else if let Some(m) = load_mcode_provider() {
                (Some(m.endpoint), Some(m.api_key), m.model, m.format, m.headers)
            } else {
                (endpoint, api_key, model.unwrap_or_else(|| "gpt-4o-mini".into()),
                 api_format.unwrap_or_else(|| "openai".into()), String::new())
            };
        Self {
            endpoint, api_key, model, api_format, extra_headers,
            interval_secs: 5, web_port: 8789,
        }
    }
}
impl SlowConfig {
    pub fn is_mock(&self) -> bool { self.endpoint.is_none() || self.api_key.is_none() }
    pub fn is_anthropic(&self) -> bool { self.api_format.eq_ignore_ascii_case("anthropic") }
}

/// 从 MCode config.yaml 的 custom_provider 里读一个 LLM provider 配置（零 yaml 依赖的轻量解析）。
struct McodeProvider {
    endpoint: String, api_key: String, model: String, format: String, headers: String,
}

fn load_mcode_provider() -> Option<McodeProvider> {
    let path = std::env::var("__MAVIS_PARENT_DATA_DIR")
        .or_else(|_| std::env::var("MINIMAX_DATA_DIR"))
        .unwrap_or_else(|_| format!("{}/.minimax", std::env::var("HOME").unwrap_or_default()));
    let text = std::fs::read_to_string(format!("{}/config.yaml", path)).ok()?;
    // 优先找 enabled + 含 baseURL 的 anthropic provider，否则第一个 custom provider
    let lines: Vec<&str> = text.lines().collect();
    // 定位 custom_provider 段内的 provider 名（缩进2，形如 "  magpie:"）
    let mut providers: Vec<(String, usize)> = Vec::new();
    let mut in_custom = false;
    for (i, line) in lines.iter().enumerate() {
        if line.trim_end() == "custom_provider:" { in_custom = true; continue; }
        if in_custom {
            let indent = line.len() - line.trim_start().len();
            if indent == 0 && !line.trim().is_empty() { break; }
            if indent == 2 && line.trim_end().ends_with(':') {
                let name = line.trim().trim_end_matches(':').to_string();
                providers.push((name, i));
            }
        }
    }
    // 优先 anthropic 格式的 provider（用户本地接入通常是它），其次其余。
    let mut ordered: Vec<&(String, usize)> = providers.iter().collect();
    ordered.sort_by_key(|(n, s)| {
        let end = providers.iter().map(|(_, p)| *p).filter(|p| p > s).min().unwrap_or(lines.len());
        let seg = &lines[*s..end];
        let is_anthropic = seg.iter().any(|l| l.trim().starts_with("api:") && l.contains("anthropic"));
        if is_anthropic { 0 } else { 1 }
    });
    for (name, start) in ordered.iter().map(|x| *x) {
        // 段范围：从 start 到下一个同级 provider
        let end = providers.iter().map(|(_, p)| *p).filter(|p| p > start).min().unwrap_or(lines.len());
        let seg = &lines[*start..end];
        let mut base = String::new(); let mut key = String::new();
        let mut api = String::new(); let mut ua = String::new();
        let mut model = String::new(); let mut in_models = false;
        for l in seg {
            let t = l.trim();
            if t.starts_with("models:") { in_models = true; continue; }
            if let Some(v) = t.strip_prefix("baseURL:") { base = clean_yaml(v); }
            if let Some(v) = t.strip_prefix("apiKey:") { key = clean_yaml(v); }
            if let Some(v) = t.strip_prefix("api:") { api = clean_yaml(v); }
            if let Some(v) = t.strip_prefix("User-Agent:") { ua = clean_yaml(v); }
            if in_models && t.ends_with(':') && !t.starts_with('#') {
                let ind = l.len() - l.trim_start().len();
                if ind == 6 { model = t.trim_end_matches(':').to_string(); break; }
            }
        }
        if !base.is_empty() && !key.is_empty() && !model.is_empty() {
            let format = if api.contains("anthropic") { "anthropic" } else { "openai" };
            let headers = if ua.is_empty() { String::new() } else { format!("User-Agent:{}", ua) };
            eprintln!("[slow] 使用 MCode provider '{}' ({} / {})", name, format, base);
            return Some(McodeProvider { endpoint: base, api_key: key, model,
                format: format.into(), headers });
        }
    }
    None
}

fn clean_yaml(v: &str) -> String {
    v.trim().trim_matches('"').trim_matches(|c| c == '\'').to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmDecision {
    pub choice: String,
    pub action: String,
    pub reason: String,
    /// 本次决策针对的目标层级 P0-P4（P0保命/P1续航/P2自给/P3扩张/P4目标）
    #[serde(default)]
    pub priority: String,
    /// 语义目标（交给快系统 autoact 翻译成工单）：surface/mop_dirty/material/scout/
    /// oxygen/expand/open_chamber/power/research/forage/element:X。留空=不下新目标。
    #[serde(default)]
    pub goal: String,
    #[serde(default)]
    pub commands: Vec<String>,
    /// ★ 可选的**策略调整**（判据参数化）：慢系统直接改选址判据。
    ///   例：{"co2_max_kg":3.0} / {"strict_prefabs":["Bed"]} / {"relax_after_fail":1}
    #[serde(default)]
    pub policy: serde_json::Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct SlowView {
    pub mode: String,
    pub cycle: i64,
    pub paused: bool,
    pub food_kcal: f64,
    pub oxygen_kg: f64,
    pub breathable_cells: usize,
    pub dupes: Vec<Value>,
    pub escalations: Vec<Value>,
    pub commands: Vec<Value>,
    pub recent_decisions: Vec<Value>,
    /// 对话流：用户指令 + 慢系统决策 + 执行结果（按时间）
    pub conversation: Vec<Value>,
    pub last_decision: String,
    /// AI 助手是否启用（false = hold 停止状态）
    pub ai_enabled: bool,
    /// 当前存档指纹 + 局内会话纪元：前端据此判断"换局/读档"→ 清空对话流（否则上一周期的对话会一直挂着）。
    pub save_fingerprint: String,
    pub session_epoch: i64,
    /// 方向盘模式：指令模式（有未完成用户指令）/ 自主模式。前端显示。
    pub control_mode: String,
    pub updated_at: i64,
}
impl Default for SlowView {
    fn default() -> Self {
        Self {
            mode: "mock".into(), cycle: 0, paused: true,
            food_kcal: 0.0, oxygen_kg: 0.0, breathable_cells: 0,
            dupes: vec![], escalations: vec![], commands: vec![],
            recent_decisions: vec![], conversation: vec![], last_decision: String::new(),
            ai_enabled: true,
            save_fingerprint: String::new(), session_epoch: 0, control_mode: "自主模式".into(), updated_at: 0,
        }
    }
}

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

pub(crate) fn call_llm(cfg: &SlowConfig, system: &str, prompt: &str, max_tokens: u32) -> Result<String, String> {
    let endpoint = cfg.endpoint.as_ref().ok_or("no endpoint")?;
    let key = cfg.api_key.as_ref().ok_or("no api key")?;

    let (url, body) = if cfg.is_anthropic() {
        // Anthropic Messages API: {baseURL}/messages
        let url = format!("{}/messages", endpoint.trim_end_matches('/'));
        let body = serde_json::json!({
            "model": cfg.model,
            "max_tokens": max_tokens,
            "system": system,
            "messages": [{"role": "user", "content": prompt}],
        });
        (url, body)
    } else {
        // OpenAI chat/completions
        let body = serde_json::json!({
            "model": cfg.model,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": prompt}
            ],
            "temperature": 0.2,
            "max_tokens": max_tokens,
        });
        (endpoint.clone(), body)
    };

    let mut req = ureq::post(&url);
    if cfg.is_anthropic() {
        req = req.set("x-api-key", key)
                 .set("anthropic-version", "2023-06-01");
    } else {
        req = req.set("Authorization", &format!("Bearer {}", key));
    }
    req = req.set("Content-Type", "application/json");
    // 自定义 headers（如 User-Agent）
    for pair in cfg.extra_headers.split(',') {
        if let Some((k, v)) = pair.split_once(':') {
            let (k, v) = (k.trim(), v.trim());
            if !k.is_empty() { req = req.set(k, v); }
        }
    }
    let resp = req.send_json(body)
        .map_err(|e| format!("LLM HTTP error: {}", e))?;
    let mut text = String::new();
    resp.into_reader().read_to_string(&mut text)
        .map_err(|e| format!("LLM read error: {}", e))?;
    let v: Value = serde_json::from_str(&text)
        .map_err(|e| format!("LLM JSON error: {}", e))?;
    if cfg.is_anthropic() {
        // Anthropic: {"content":[{"type":"text"|"thinking","text"|"thinking":"..."}]}
        let arr = v.get("content").and_then(|c| c.as_array());
        // 优先找 text 块
        let text_out = arr.and_then(|a| a.iter()
            .find(|x| x.get("type").and_then(|t| t.as_str()) == Some("text"))
            .and_then(|x| x.get("text").and_then(|t| t.as_str())));
        if let Some(t) = text_out { return Ok(t.to_string()); }
        // 推理模型可能只输出 thinking 块：从中提取最后的 JSON 对象
        let thinking = arr.map(|a| a.iter()
            .filter(|x| x.get("type").and_then(|t| t.as_str()) == Some("thinking"))
            .filter_map(|x| x.get("thinking").and_then(|t| t.as_str()))
            .collect::<Vec<&str>>().join("\n")).unwrap_or_default();
        if let (Some(s), Some(e)) = (thinking.find('{'), thinking.rfind('}')) {
            return Ok(thinking[s..=e].to_string());
        }
        Err(format!("Anthropic response has no text/json: stop={} thinking_len={}",
            v.get("stop_reason").and_then(|s| s.as_str()).unwrap_or("?"), thinking.len()))
    } else {
        v.pointer("/choices/0/message/content")
            .and_then(|c| c.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| "LLM response missing content".into())
    }
}

fn parse_decision(text: &str) -> Result<LlmDecision, String> {
    let t = text.trim();
    let js = if t.starts_with('{') { t }
        else if let (Some(a), Some(b)) = (t.find('{'), t.rfind('}')) { &t[a..=b] }
        else { t };
    serde_json::from_str(js).map_err(|e| format!("parse failed: {} | {}", e, js))
}

fn mock_decision(prompt: &str, has_escalation: bool, has_command: bool, proactive: bool) -> LlmDecision {
    if has_command {
        LlmDecision { choice: "custom".into(), action: "command".into(),
            reason: "mock: 用户指令优先，已记录并恢复自主".into(),
            priority: "P3".into(), goal: String::new(), commands: vec!["window 90".into()], policy: Value::Null }
    } else if has_escalation {
        let c = if prompt.contains("食物") && prompt.contains("switch_goal") { "switch_goal" } else { "dig_access" };
        LlmDecision { choice: c.into(), action: "resolve".into(),
            reason: "mock: 选合理候选，验证闭环".into(), priority: "P1".into(), goal: String::new(), commands: vec![], policy: Value::Null }
    } else if proactive {
        // 主动规划模式：mock 也要产出一个可推进目标，不能 wait（那是失职）
        LlmDecision { choice: "proactive".into(), action: "command".into(),
            reason: "mock: 主动规划，依据战略进度推进自给".into(),
            priority: "P2".into(), goal: "material".into(), commands: vec![], policy: Value::Null }
    } else {
        LlmDecision { choice: "none".into(), action: "wait".into(),
            reason: "mock: 无待处理事项".into(), priority: "P0".into(), goal: String::new(), commands: vec![], policy: Value::Null }
    }
}

const SYSTEM_PROMPT: &str = "你是缺氧殖民地慢系统决策者，目标是让这局游戏持续变好，不是维持现状。快系统每10秒采局面，遇卡点上报结构化诊断(含候选)。

== 目标阶梯（每次决策必须选最高可改进的层级，priority 填 P0-P4）==
P0 保命：有人倒地/窒息/膀胱爆/污染水淹没 → 立即消除，不可协商。
P1 续航：食物/氧气的『还能撑几周期』最大化。用续航比较，不用绝对存量。当前谁短就先解决谁（如食物9周期<氧气21周期 → 先采食物）。
P2 自给：从『坐吃存量』转向持续产出（种田/制氧/可持续食物/农场）。
P3 扩张：领打印舱新人(劳动力)、扩建空间、研究解锁、改善床位厕所电力。
P4 目标：完成本局胜利条件（火箭/本局目标）。

== 决策纪律 ==
1. 你的默认动作是推进主线，不是等待。选 wait 前必须能论证：所有 P0-P4 都健康且当下无任何可执行改进——否则就是失职。
2. 用户指令：只有明确、可执行的指令才优先执行；乱码/测试/无法转成 oni 命令的指令 → 忽略它，继续自主推进，绝不因它停下。
3. 可达性/落脚点等确定性问题交给快系统自救（选对应候选即可），别手算坐标。
4. 改目标/放弃路线是重大决策，必须说明依据；但断粮/窒息这类先死因优先于既有计划。

输出严格 JSON（不要Markdown）: {choice,action,reason,priority,goal,commands[]}。
- action ∈ {resolve,command,wait,custom}；priority ∈ {P0,P1,P2,P3,P4}
- reason 用1-2句说明『这提升了哪个P级、为什么』
- goal: 你决定推进的**语义目标**，快系统会翻译成具体工单（坐标由快系统算，你不要算）。
  合法值: surface/mop_dirty/material/scout/oxygen/expand/open_chamber/power/research/forage/element:元素名
  例: 断粮时填 forage；要产氧填 oxygen；要建材填 material；没新目标留空。
  **重要**: 想让游戏做事就填 goal（这是你真正的执行力），别只在 reason 里写文字描述。
- commands: 可直接执行的oni子命令(如 'build 139,197 Ladder SandStone --apply')或留空[]；优先用 goal，少用 commands。
  战略类元命令（不需要坐标）：'research <TechId>' 设定研究方向（研究站就绪且无在研时由你决定，如 research ImprovedOxygen / research FarmingTech / research PowerRegulations）；'pod claim <index>' 领取打印舱物品包（第 index 项；新人只能在游戏里选，不要为此发命令）。
- policy: **可选**，当卡点是落点判据太严或太松时调整选址策略（会落盘并立刻生效）：
  co2_max_kg=3.0 放宽选址允许的CO₂上限；strict_prefabs=[Bed] 改哪些建筑仍要求纯氧；
  relax_after_fail=1 让快系统失败1次就自动放宽。不需要时留空或省略。";

/// 失败熔断阈值：同一 goal 连续失败达到此次数 → 禁止再选，强制换策略。
const FAIL_CIRCUIT_THRESHOLD: usize = 3;
/// 熔断冷却：曾成功过的 goal，失败后多少秒内不再允许（超时后可重试）。
/// ★ 从未成功过的 goal（total_ok==0）连续失败≥阈值 → 永久熔断，不靠冷却放行。
const FAIL_CIRCUIT_COOLDOWN_SECS: i64 = 1800;

/// ★ 主动规划心跳周期（秒）：即使无 escalation/用户指令，慢系统也周期性唤起 LLM，
/// 依据战略进度（plan.json）主动选下一个 goal 推进，而不是干等卡点。
/// 60s = 12 轮慢系统循环（每轮 5s）。
const PROACTIVE_PLAN_INTERVAL_SECS: i64 = 60;

fn proactive_state_path() -> std::path::PathBuf {
    save_dir().join("last_proactive.json")
}

/// 是否到了主动规划时刻（距上次 ≥ 周期）。文件不存在 = 立即规划。
fn should_proactive() -> bool {
    let now_ts = now();
    match std::fs::read_to_string(proactive_state_path())
        .ok().and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.get("ts").and_then(|x| x.as_i64()))
    {
        Some(last) => now_ts - last >= PROACTIVE_PLAN_INTERVAL_SECS,
        None => true,
    }
}

fn mark_proactive() {
    let sd = save_dir();
    let _ = std::fs::create_dir_all(&sd);
    let rec = serde_json::json!({"ts": now()});
    let _ = std::fs::write(proactive_state_path(), serde_json::to_string(&rec).unwrap_or_default());
}

/// 单个 goal 的整局成败统计。
#[derive(Debug, Clone)]
struct GoalStatus {
    goal: String,
    /// 自上次成功以来的连续失败次数（成功清零）。
    fails_since_success: usize,
    total_fail: usize,
    total_ok: usize,
    last_fail_ts: i64,
    last_detail: String,
    /// 最近一次失败的结构化原因（no_target/no_material/...）
    last_fail_kind: String,
}

/// 纯函数：从 goal_outcomes 的 jsonl 行计算每个 goal 的状态。
/// 按时间正序处理：成功清零连续计数，失败累加。
fn compute_goal_status(lines: &[&str]) -> Vec<GoalStatus> {
    use std::collections::HashMap;
    let mut order: Vec<String> = Vec::new();
    let mut m: HashMap<String, GoalStatus> = HashMap::new();
    for line in lines {
        let Ok(o) = serde_json::from_str::<Value>(line) else { continue };
        let goal = o.get("goal").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if goal.is_empty() { continue; }
        let ok = o.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
        let ts = o.get("ts").and_then(|v| v.as_i64()).unwrap_or(0);
        let detail: String = o.get("detail").and_then(|v| v.as_str()).unwrap_or("")
            .chars().take(60).collect();
        let fail_kind: String = o.get("fail_kind").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if !m.contains_key(&goal) {
            order.push(goal.clone());
            m.insert(goal.clone(), GoalStatus {
                goal: goal.clone(), fails_since_success: 0, total_fail: 0, total_ok: 0,
                last_fail_ts: 0, last_detail: String::new(), last_fail_kind: String::new(),
            });
        }
        let e = m.get_mut(&goal).unwrap();
        // ★ 不只信 ok 字段（AGENTS.md §9）：detail 明确说"零进展/无可下单曲/推不动"时，
        //   即使被记成 ok=true，也按失败计——否则熔断计数不增长，慢系统会无限重复入队推不动的目标。
        let zero_progress = detail.contains("无可下单曲")
            || detail.contains("零进展")
            || detail.contains("推不动");
        let ok = ok && !zero_progress;
        if ok {
            e.total_ok += 1;
            e.fails_since_success = 0;
        } else {
            e.total_fail += 1;
            e.fails_since_success += 1;
            e.last_fail_ts = ts;
            e.last_detail = detail;
            if !fail_kind.is_empty() { e.last_fail_kind = fail_kind; }
        }
    }
    order.into_iter().filter_map(|g| m.remove(&g)).collect()
}

/// 读当前存档的 goal 状态快照。
fn goal_status_snapshot() -> Vec<GoalStatus> {
    let Ok(text) = std::fs::read_to_string(save_dir().join("goal_outcomes.jsonl")) else {
        return Vec::new();
    };
    let lines: Vec<&str> = text.lines().collect();
    compute_goal_status(&lines)
}

/// 该 goal 当前是否处于熔断状态。
/// 从未成功过 + 连续失败≥阈值 → 永久熔断（已证明此地图/配置下走不通，别再重试）；
/// 曾成功过 + 连续失败≥阈值 → 冷却期内熔断，到期后可重试一次。
fn is_circuit_broken(s: &GoalStatus, now_ts: i64) -> bool {
    if s.fails_since_success < FAIL_CIRCUIT_THRESHOLD { return false; }
    if s.total_ok == 0 { return true; } // 整局从未成功 → 永久禁止，直到有人改地图/代码
    (now_ts - s.last_fail_ts) < FAIL_CIRCUIT_COOLDOWN_SECS
}

/// 读取最近的 ai_goal 决策及其执行回执，生成紧凑的"决策→结果"历史，
/// 整局记忆系统：滑动窗口(最近决策) + 持久教训(失败模式统计)。
/// 上下文可控：窗口6条 + 教训5条，约1500字符。
fn recent_decision_history() -> String {
    let sd = save_dir();
    let Ok(text) = std::fs::read_to_string(sd.join("goal_outcomes.jsonl")) else { return String::new(); };
    let now_ts = now();
    let mut seen = std::collections::HashSet::new();
    let mut window: Vec<String> = Vec::new();
    let mut stats: std::collections::HashMap<String, (u32,u32)> = std::collections::HashMap::new();
    let mut fail_kinds: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut all: Vec<&str> = text.lines().collect();
    all.reverse();
    for line in &all {
        let Ok(o) = serde_json::from_str::<Value>(line) else { continue };
        let goal = o.get("goal").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if goal.is_empty() { continue; }
        let ok = o.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
        let e = stats.entry(goal.clone()).or_insert((0,0));
        if ok { e.1 += 1 } else {
            e.0 += 1;
            let fk = o.get("fail_kind").and_then(|v| v.as_str()).unwrap_or("");
            if !fk.is_empty() { fail_kinds.insert(goal.clone(), fk.to_string()); }
        }
        if seen.insert(goal.clone()) && window.len() < 6 {
            let detail: String = o.get("detail").and_then(|v| v.as_str()).unwrap_or("").chars().take(50).collect();
            let ts = o.get("ts").and_then(|v| v.as_i64()).unwrap_or(0);
            let ago = (now_ts - ts).max(0);
            let ago_s = if ago < 60 { format!("{}s", ago) } else if ago < 3600 { format!("{}m", ago/60) } else { format!("{}h", ago/3600) };
            window.push(format!("  - [{}] {} → {}: {}", ago_s, goal, if ok { "成功" } else { "失败" }, detail));
        }
    }
    if window.is_empty() { return String::new(); }
    let mut out = String::from("\n== 整局记忆 ==\n");
    out.push_str("\n最近决策(滑动窗口6):\n");
    for i in window { out.push_str(&i); out.push('\n'); }
    // ★ 教训带具体失败原因类型：让 LLM 知道「为什么失败」而非只看到次数。
    let mut lessons: Vec<String> = stats.iter().filter(|(_, (f, _))| *f >= 2)
        .map(|(g, (f, sc))| format!("  - goal={} 失败{}次/成功{}次 原因={} → 诊断根因再换策略（别只换名字）",
            g, f, sc, fail_kinds.get(g).map(|x| x.as_str()).unwrap_or("unknown"))).collect();
    lessons.sort();
    if !lessons.is_empty() {
        out.push_str("\n持久教训(整局统计):\n");
        for l in lessons.iter().take(5) { out.push_str(l); out.push('\n'); }
    }
    out.push_str("\n失败历史是开头「失败熔断」的硬依据：连续失败≥3次的目标本轮禁止再选。\n");
    out
}
/// ★ 幂等决策节流：检测最近连续 N 次决策都是同一个 goal，把"连续重复"显式告警注入 prompt，
/// 促使 LLM 升级手段或诊断根因（连续重复同一 goal 而无实质进展 = 决策空转）。
fn recent_goal_streak() -> (String, usize, String) {
    let sd = save_dir();
    let Ok(text) = std::fs::read_to_string(sd.join("decisions.jsonl")) else {
        return (String::new(), 0, String::new());
    };
    let mut goals: Vec<String> = text.lines().rev().filter_map(|l| {
        let d: Value = serde_json::from_str(l).ok()?;
        let g = d.get("goal").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if g.is_empty() { None } else { Some(g) }
    }).take(20).collect();
    goals.reverse(); // 恢复时间正序
    let mut streak = 0usize;
    if let Some(last) = goals.last() {
        for g in goals.iter().rev() {
            if g == last { streak += 1; } else { break; }
        }
    }
    let recent: Vec<String> = goals.iter().rev().take(6).cloned().collect();
    let summary = if recent.is_empty() { String::new() }
        else { format!("最近决策 goal 序列(新→旧): {}\n", recent.join(" → ")) };
    let current = goals.last().cloned().unwrap_or_default();
    (summary, streak, current)
}

/// 纯函数：从 plan 节点里挑出"requires 全部 done"的下一个可推进节点名。
/// 供 strategy_progress 使用，也便于单测（不依赖文件）。
fn next_actionable_names(nodes: &[Value], done_ids: &std::collections::HashSet<String>) -> Vec<String> {
    let mut out = Vec::new();
    for n in nodes {
        let status = n.get("status").and_then(|v| v.as_str()).unwrap_or("todo");
        if status == "done" { continue; }
        let reqs: Vec<String> = n.get("requires").and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
            .unwrap_or_default();
        if reqs.iter().all(|r| done_ids.contains(r)) {
            out.push(n.get("name").and_then(|v| v.as_str()).unwrap_or("?").to_string());
        }
    }
    out
}

/// ★ 当前目标（滚动窗口 W1）—— prompt 的**第一段**。
///
/// 约束：必须放在卡点列表之前——LLM 第一眼看到卡点列表会去"解决卡点"而不是"推进目标"，
/// 在无关目标之间来回跳。自动驾驶的对照：导航先有目的地，再算怎么绕过障碍。
///
/// 输出：目标是什么 + 已尝试几次 + 失败原因 + **本轮该做什么**（推进/绕过/标记失败让窗口重排）。
fn current_objective() -> String {
    let Some(plan) = load_window_plan() else { return String::new() };
    let done: std::collections::HashSet<&str> = plan.nodes.iter().filter(|n| n.is_done()).map(|n| n.id.as_str()).collect();
    // W1 = 第一个"现在可执行"的节点（requires 全 done、自身非 done/blocked/deferred）
    let Some(w1) = plan.nodes.iter().find(|n| {
        !n.is_done() && !n.is_blocked() && !n.is_deferred()
            && n.requires.iter().all(|r| done.contains(r.as_str()))
    }) else {
        // 没有可执行节点 → 窗口需要重排（慢系统的 refresh_window 会处理）
        let first_pending = plan.nodes.iter().find(|n| !n.is_done());
        let why = match first_pending {
            Some(n) if n.is_blocked() => format!("W1「{}」已被熔断", n.name),
            Some(n) if n.is_deferred() => format!("W1「{}」已暂缓：{}", n.name, n.deferred.as_deref().unwrap_or("")),
            Some(n) => format!("W1「{}」前置未满足（requires: {:?}）", n.name, n.requires),
            None => "窗口全部完成".to_string(),
        };
        return format!("== 🎯 当前目标 ==\n⚠ 滚动窗口无可执行节点：{why}\n→ 本轮决策：帮助窗口重排——用 commands 下一条能解除该阻塞的精确工单（如挖通/铺梯子/采材料），或说明为什么该目标在本局不可达。**不要换成无关目标（如 scout）**。\n\n");
    };
    let goal = w1.actions.iter().find_map(|a| match a {
        crate::exec::Action::Auto { goal, .. } => Some(goal.clone()),
        crate::exec::Action::AutoBuild { prefab, .. } => Some(format!("build:{prefab}")),
        _ => None,
    }).unwrap_or_default();
    // 该 goal 最近的执行结果（已尝试几次 / 为什么失败）
    let (attempts, last_fail) = goal_recent_outcome(&goal);
    let status = if attempts == 0 {
        "尚未尝试".to_string()
    } else if last_fail.is_empty() {
        format!("已尝试 {attempts} 次，最近一次成功")
    } else {
        format!("已尝试 {attempts} 次，最近失败原因：{last_fail}")
    };
    format!(
        "== 🎯 当前目标（滚动窗口 W1，本轮默认推进它）==\n  目标：{}（goal: {}）\n  判据：{}\n  状态：{}\n  → 本轮决策：**推进 W1**（选同一个 goal，或下精确工单绕过障碍）。只有当你能说明它在本局**确实不可达**时，才标记它失败并解释根因，让窗口下一轮重排。**不要因为卡点就换成无关目标**（那是上一轮在 research/scout 之间来回跳的原因）。\n\n",
        w1.name, goal,
        w1.success.lines().next().unwrap_or(""),
        status
    )
}

fn load_window_plan() -> Option<crate::decide::Plan> {
    std::fs::read_to_string("plan.json").ok().and_then(|t| serde_json::from_str(&t).ok())
}

/// 某 goal 在 goal_outcomes 里的尝试次数与最近一次失败原因。
fn goal_recent_outcome(goal: &str) -> (usize, String) {
    let sd = save_dir();
    let Ok(text) = std::fs::read_to_string(sd.join("goal_outcomes.jsonl")) else { return (0, String::new()) };
    let mut n = 0usize;
    let mut last_fail = String::new();
    for line in text.lines().rev() {
        let Ok(o) = serde_json::from_str::<Value>(line) else { continue };
        if o.get("goal").and_then(|v| v.as_str()) != Some(goal) { continue; }
        n += 1;
        let ok = o.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
        if !ok && last_fail.is_empty() {
            last_fail = o.get("detail").and_then(|v| v.as_str()).unwrap_or("").chars().take(70).collect();
        }
        if n >= 8 { break; }
    }
    (n, last_fail)
}

/// ★ 战略状态机：读 plan.json（AI 拥有的 locked plan），生成"我在哪、该往哪走"摘要。
/// 每轮 prompt 注入战略进度，让 LLM 知道已完成里程碑与下一个可推进节点（不只看当前快照做应激式决策）。
fn strategy_progress() -> String {
    // ★ 路径约束：快系统运行时用的是**仓库根目录**的 plan.json（cfg.plan_path，CWD 相对），
    //   不是 save_dir()/plan.json（每存档目录）。两者必须读**同一个文件**，慢系统才能"看见"计划。
    let plan_path = std::path::Path::new("plan.json");
    let Ok(text) = std::fs::read_to_string(plan_path) else {
        return String::new();
    };
    let Ok(plan) = serde_json::from_str::<Value>(&text) else { return String::new(); };
    let Some(nodes) = plan.get("nodes").and_then(|v| v.as_array()) else { return String::new(); };

    let mut done = Vec::new();
    let mut todo = Vec::new();
    let mut done_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    for n in nodes {
        let id = n.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let name = n.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let status = n.get("status").and_then(|v| v.as_str()).unwrap_or("todo");
        if status == "done" {
            done_ids.insert(id);
            done.push(name.to_string());
        } else {
            todo.push(n.clone());
        }
    }
    // 下一个可推进节点：requires 全部 done 的 todo 节点
    let mut next = Vec::new();
    for n in &todo {
        let reqs: Vec<String> = n.get("requires").and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
            .unwrap_or_default();
        if reqs.iter().all(|r| done_ids.contains(r)) {
            let name = n.get("name").and_then(|v| v.as_str()).unwrap_or("?");
            let succ = n.get("success").and_then(|v| v.as_str()).unwrap_or("");
            let goal = n.get("actions").and_then(|v| v.as_array())
                .and_then(|a| a.first())
                .and_then(|x| x.get("goal").or_else(|| x.get("prefab")).and_then(|g| g.as_str()))
                .unwrap_or("");
            next.push(format!("  - {} [判据: {}][建议goal: {}]", name, succ, goal));
        }
    }

    let mut out = String::from("== 🗺 战略进度（plan.json，AI拥有·locked）==\n");
    out.push_str(&format!("已完成里程碑 {}/{}：{}\n", done.len(), nodes.len(),
        if done.is_empty() { "无".into() } else { done.iter().take(6).cloned().collect::<Vec<_>>().join(" / ") }));
    if next.is_empty() {
        out.push_str("下一个可推进：无（依赖未满足或全部完成）→ 检查是否需要 AI 改 plan\n");
    } else {
        out.push_str("下一个可推进（requires 已满足）:\n");
        for l in next.iter().take(4) { out.push_str(l); out.push('\n'); }
    }
    out.push_str("决策纪律：优先推进上述节点的 goal；除非出现 P0 先死因，否则不要跳到无关目标。\n\n");
    out
}

fn build_prompt(escs: &[Value], cmds: &[Value], snap: &Value, proactive: bool) -> String {
    let mut p = String::new();
    if proactive {
        p.push_str("== 🎯 主动规划模式（当前无卡点/无新指令）==\n");
        p.push_str("没有紧急事件，这正是推进战略的最佳时机。请根据下面的战略进度，主动选择一个可推进的 goal（P2自给/P3扩张优先），让殖民地持续变好。\n");
        p.push_str("不要输出 wait，不要空等卡点——那等于失职。若战略进度显示所有节点完成，选一个合理的扩张/自给目标（expand/research/power/element:X）。\n\n");
    }
    p.push_str(&current_objective());
    p.push_str(&strategy_progress());
    // ★ 当前选址策略（慢系统可调）：让慢系统知道有哪些判据能自己改。
    {
        let pol = oni_world::preflight::site_policy();
        p.push_str(&format!(
            "== 🔧 当前选址策略（你能调）==\n  - co2_max_kg={}（选址允许的CO₂浓度上限kg/格）\n  - strict_prefabs={:?}（这些建筑仍要求纯氧）\n  - relax_after_fail={}（连续失败几次后快系统自动放宽）\n  调整方式: oni policy set co2_max_kg=3.0（放宽CO₂上限）或 oni policy set strict_prefabs=Bed（改哪些建筑仍要求纯氧）\n\n",
            pol.co2_max_kg, pol.strict_prefabs, pol.relax_after_fail
        ));
    }
    let (streak_summary, streak, cur_goal) = recent_goal_streak();
    if !streak_summary.is_empty() { p.push_str(&streak_summary); }
    if streak >= 3 {
        // ★ 约束：连续不进展**不是"换目标"的理由**，是"升级手段/诊断"的理由——
        //   挖矿/建厕所/研究都要 3 轮以上，"禁止再选它"只会打断所有需要多轮的目标。
        p.push_str(&format!("🔁 幂等节流：goal={} 已连续 {} 次无实质进展 → **不要换目标，要升级手段**：① 用 commands 下一条精确工单绕过障碍（挖通/铺梯子/换落点）；② 若判断它在本局确实不可达，说明根因并标记失败，让窗口重排。原地重选同一个 goal 而不改变手段 = 失职。\n\n",
            cur_goal, streak));
    }
    // 🔴 失败熔断：最高优先级硬约束（见开头），逼迫换策略而非重复失败目标。
    let statuses = goal_status_snapshot();
    let now_ts = now();
    let broken: Vec<&GoalStatus> = statuses.iter()
        .filter(|s| is_circuit_broken(s, now_ts)).collect();
    if !broken.is_empty() {
        p.push_str("== 🔴 失败熔断（硬性约束，非建议）==\n");
        p.push_str("以下 goal 连续失败≥3次且未过冷却，本轮严禁再选；重复选会被系统拒绝并记入对话：\n");
        for s in &broken {
            let permanent = if s.total_ok == 0 { "【从未成功·永久禁止】" } else { "" };
            p.push_str(&format!("  - {}: 连续失败{}次/成功{}次{}，原因类型={}，最近「{}」\n",
                s.goal, s.fails_since_success, s.total_ok, permanent,
                if s.last_fail_kind.is_empty() { "unknown" } else { &s.last_fail_kind },
                s.last_detail));
        }
        let working: Vec<&str> = statuses.iter()
            .filter(|s| s.total_ok > 0 && !is_circuit_broken(s, now_ts))
            .map(|s| s.goal.as_str()).collect();
        if !working.is_empty() {
            p.push_str(&format!("  - 有成功记录的可用目标（证据支持）: {}\n", working.join(" / ")));
        }
        p.push_str("  - 本轮必须换策略：选上述可用目标之一，或用 commands 下精确工单。\n\n");
    }
    p.push_str("== 当前局面（传感器统一简报，与窗口规划同一口径）==\n");
    // ★ 事实只走 window::facts_summary（sense::Snapshot 访问器），不手写另一份简报。
    let cycle = snap.get("cycle").and_then(|v| v.as_i64()).unwrap_or(0);
    p.push_str(&crate::window::facts_summary(snap, cycle));
    if !cmds.is_empty() {
        p.push_str("\n== 🎮 用户指令模式（优先于自主决策；世界已暂停） ==\n");
        for c in cmds {
            p.push_str(&format!("  #{} {}\n",
                c.get("id").unwrap_or(&Value::Null),
                c.get("text").unwrap_or(&Value::Null)));
        }
        // ★ 方向盘回执（v3 ⑥）：人打了方向，车走了没有必须回执。
        //   reason 会作为该指令的回执展示给用户：写清楚打算怎么执行（含 goal），
        //   不要只写分析。无法执行的指令要说明原因并给出替代方案。
        p.push_str("  → 你的 reason 会作为该指令的回执展示给用户：写清楚『打算怎么执行』（含 goal），\n    不要只写分析。无法执行的指令要说明原因并给出替代方案。\n");
    }
    // 检查最近一条用户指令完成状态：未完成/失败则提示 LLM 优先推动
    {
        let sd = save_dir();
        if let Ok(text) = std::fs::read_to_string(sd.join("commands.jsonl")) {
            let mut last_user: Option<Value> = None;
            for line in text.lines() {
                if let Ok(c) = serde_json::from_str::<Value>(line) {
                    if c.get("source").and_then(|v| v.as_str()) == Some("user") { last_user = Some(c); }
                }
            }
            if let Some(c) = last_user {
                let done = c.get("done").and_then(|v| v.as_bool()).unwrap_or(false);
                let result = c.get("result").and_then(|v| v.as_str()).unwrap_or("");
                let cmd = decode_unicode_escapes(c.get("text").and_then(|v| v.as_str()).unwrap_or(""));
                if !done {
                    p.push_str(&format!("\n!! 上次用户指令「{}」未完成,优先推动它,别开新方向。\n", cmd));
                } else if result.contains("失败") || result.contains("未成功") {
                    p.push_str(&format!("\n!! 上次用户指令「{}」执行失败({})→换方案别重复。\n", cmd, result));
                }
            }
        }
    }
    if !escs.is_empty() {
        p.push_str("\n== 快系统卡点(含候选,选一个) ==\n");
        for e in escs {
            p.push_str(&format!("  #{} [{}] {}\n",
                e.get("id").unwrap_or(&Value::Null),
                e.get("kind").unwrap_or(&Value::Null),
                e.get("detail").unwrap_or(&Value::Null)));
            if let Some(ctx) = e.get("context") {
                p.push_str(&format!("    卡点={} 目标={:?} 可达区={:?}\n",
                    ctx.get("obstacle").unwrap_or(&Value::Null),
                    ctx.get("target"), ctx.get("reach_bounds")));
                if let Some(cands) = ctx.get("candidates").and_then(|c| c.as_array()) {
                    for c in cands {
                        p.push_str(&format!("    候选[{}] {}: {}\n",
                            c.get("id").unwrap_or(&Value::Null),
                            c.get("action").unwrap_or(&Value::Null),
                            c.get("note").unwrap_or(&Value::Null)));
                    }
                }
            }
        }
    }
    p.push_str(&recent_decision_history());
    p.push_str("\n输出严格JSON(choice/action/reason/priority/goal/commands)。");
    p
}

fn run_oni(args: &[&str]) -> String {
    let exe = std::env::current_exe().ok()
        .and_then(|p| p.parent().map(|d| d.join("oni")))
        .unwrap_or_else(|| std::path::PathBuf::from("oni"));
    match std::process::Command::new(&exe).args(args).output() {
        Ok(o) => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        Err(e) => format!("ERR: {}", e),
    }
}

/// 把慢系统的语义目标写成 source=ai_goal 的指令，供快系统 cycle 消费执行。
/// 快系统用 autoact 把 goal 翻译成具体工单（坐标由快系统算）。
fn push_ai_goal(goal: &str, reason: &str) -> bool {
    if goal.trim().is_empty() { return false; }
    // 只接受合法语义目标，防止 LLM 输出乱字符串
    if crate::autoact::parse_goal(goal).is_none() { return false; }
    // ★ 写进独立 ai_goals.jsonl（不混进 commands.jsonl，ai_goal 不是用户指令）
    crate::commands::push_ai_goal(goal, reason) != 0
}

/// ★ 大脑→线控的**接口契约**：LLM 的 commands 只允许两类，其余拒绝并回执原因。
///
/// 为什么（v3 原则 0.2/0.3）：大脑不直接控制电机。LLM 下 `build Outhouse --apply`（无坐标）、
/// `use toilet 128,192`（不存在的命令）会**静默失败**——它不知道自己没做成，下一轮还试。
/// 智驾里大脑给导航的是目的地，不是电机扭矩。
///
/// 放行：
///   ① 带坐标的精确工单：`build x,y Prefab [Mat] [--apply]`、`order 挖 @(x,y):N`、`preflight ...`
///   ② 元命令：window / policy / hold / release / pause / resume / resolve / decision / note / research / pod
/// 拒绝：无坐标的 build、不存在的子命令、任何"use/让/去"类自然语言
/// 总开关类子命令：hold 期间大脑不得触碰（见 [`validate_llm_command`]）。
pub fn is_control_plane(sub: &str) -> bool {
    matches!(sub, "hold" | "release" | "pause" | "resume" | "window")
}

pub fn validate_llm_command(cmd: &str) -> Result<Vec<String>, String> {
    let parts: Vec<&str> = cmd.split_whitespace().collect();
    let Some(sub) = parts.first() else { return Err("空命令".into()) };
    // 坐标形式："x,y" / "@(x,y)" / "@(x,y):优先级" —— 扫描字符串找 "数字,数字" 对，不手拆 token
    let b: Vec<char> = cmd.chars().collect();
    let has_coord = (0..b.len()).any(|i| {
        if b[i] != ',' || i == 0 || i + 1 >= b.len() { return false; }
        let digits_before = b[..i].iter().rev().take_while(|c| c.is_ascii_digit()).count();
        let digits_after = b[i + 1..].iter().take_while(|c| c.is_ascii_digit()).count();
        digits_before > 0 && digits_after > 0
    });
    match *sub {
        // 精确工单：必须有坐标
        "build" | "order" | "preflight" | "dig" | "probe" => {
            if has_coord { Ok(parts.iter().map(|s| s.to_string()).collect()) }
            else { Err(format!("'{sub}' 需要坐标（如 build 135,192 Outhouse SandStone --apply / order 挖 @(120,195):9）。建造请改用 goal（toilet/oxygen/...），坐标由执行器算")) }
        }
        // 元命令：不需要坐标
        "window" | "policy" | "hold" | "release" | "pause" | "resume" | "resolve"
        | "decision" | "note" | "command" | "escalations" | "plan-check"
        | "research" | "pod" => {
            // ★ 总开关类命令在 hold 期间对大脑封口：hold = 用户关闭 AI 总开关，是"唯一的
            //   用户停止信号"（AGENTS.md §7），解除只能是人（oni release / Web「启动 AI」）。
            //   大脑能发 release/window 就等于能掀自己的总开关。
            if is_control_plane(sub) && crate::agent::hold_active() {
                return Err("hold 期间（用户已关闭 AI 总开关）大脑不能操作 hold/release/pause/resume/window；解除 hold 只能由用户执行".into());
            }
            Ok(parts.iter().map(|s| s.to_string()).collect())
        },
        _ => Err(format!("'{sub}' 不是可执行的 oni 子命令。可用：带坐标的 build/order/preflight，或元命令 window/policy/resolve。建造走 goal")),
    }
}

#[cfg(test)]
mod llm_command_tests {
    use super::validate_llm_command;
    #[test]
    fn precise_orders_pass_and_vague_ones_are_rejected_with_reason() {
        assert!(validate_llm_command("build 135,192 Outhouse SandStone --apply").is_ok());
        assert!(validate_llm_command("order 挖 @(120,195):9").is_ok());
        assert!(validate_llm_command("window 90").is_ok());
        assert!(validate_llm_command("policy set co2_max_kg=3").is_ok());
        // 三条无效命令，全部要被拒且给出可操作的原因
        let e1 = validate_llm_command("build Outhouse --apply").unwrap_err();
        assert!(e1.contains("坐标") && e1.contains("goal"), "{e1}");
        let e2 = validate_llm_command("use toilet 128,192").unwrap_err();
        assert!(e2.contains("不是可执行"), "{e2}");
        let e3 = validate_llm_command("build Toilet --apply").unwrap_err();
        assert!(e3.contains("坐标"), "{e3}");
    }
}

/// ★ 大脑事件驱动门控（v3 ⑤）：同一事件只触发一次 LLM 决策。
///
/// 约束：快系统每 10s 上报、慢系统每 5s 轮询，resolve 之前同一卡点会被看到多次，
/// 每次都是完整 prompt 却没有新信息——同一个事件不触发两次重新规划。
///
/// 规则：escalation 按 id、用户指令按 id 去重，处理过的记入 processed.json（带 TTL，避免无限膨胀）。
/// 无新事件 + 无用户指令 → 走 60s 心跳（should_proactive），否则本轮不调 LLM。
fn event_gate(escs: &[Value], cmds: &[Value]) -> Vec<Value> {
    let path = save_dir().join("processed_events.json");
    let mut done: std::collections::HashMap<i64, i64> = std::fs::read_to_string(&path)
        .ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    let now_ts = now();
    // 只保留 30 分钟内的记录
    done.retain(|_, t| now_ts - *t < 1800);
    let mut fresh = Vec::new();
    for e in escs.iter().chain(cmds.iter()) {
        let id = e.get("id").and_then(|v| v.as_i64()).unwrap_or(0);
        if id == 0 || !done.contains_key(&id) { fresh.push(e.clone()); }
    }
    if !fresh.is_empty() {
        for e in &fresh {
            if let Some(id) = e.get("id").and_then(|v| v.as_i64()) { done.insert(id, now_ts); }
        }
        let _ = std::fs::write(&path, serde_json::to_string(&done).unwrap_or_default());
    }
    fresh
}

/// 纯函数版（可测）：给定已处理集合，返回新事件。
pub fn filter_fresh_events<'a>(escs: &'a [Value], cmds: &'a [Value], done: &std::collections::HashMap<i64, i64>) -> Vec<&'a Value> {
    escs.iter().chain(cmds.iter())
        .filter(|e| {
            let id = e.get("id").and_then(|v| v.as_i64()).unwrap_or(0);
            id == 0 || !done.contains_key(&id)
        })
        .collect()
}

#[cfg(test)]
mod event_gate_tests {
    use super::filter_fresh_events;
    use std::collections::HashMap;
    #[test]
    fn same_escalation_id_triggers_llm_only_once() {
        let e = serde_json::json!({"id": 100, "kind": "Deadlock"});
        let c = serde_json::json!({"id": 200, "text": "建厕所"});
        let mut done: HashMap<i64, i64> = HashMap::new();
        assert_eq!(filter_fresh_events(&[e.clone()], &[c.clone()], &done).len(), 2, "首次都算新事件");
        done.insert(100, 0);
        let ev = [e.clone()];
        let cv = [c.clone()];
        let fresh = filter_fresh_events(&ev, &cv, &done);
        assert_eq!(fresh.len(), 1, "已处理的 escalation 不再触发");
        assert_eq!(fresh[0]["id"], 200);
        done.insert(200, 0);
        let ev2 = [e];
        let cv2 = [c];
        assert!(filter_fresh_events(&ev2, &cv2, &done).is_empty(), "全处理过 → 不调 LLM");
    }
}

fn execute_decision(dec: &LlmDecision, escs: &[Value], cmds: &[Value]) -> String {
    let mut log = Vec::new();
    // ★ 策略调整落地：慢系统直接改选址判据（落盘 + 立刻对快系统生效）。
    //   这就是"LLM 最终决策"落到可执行参数上的落点——不是每次改代码，
    //   而是慢系统按局面调整阈值/严格名单，快系统下一轮照新判据选点。
    if !dec.policy.is_null() && dec.policy.as_object().map(|o| !o.is_empty()).unwrap_or(false) {
        let mut pol = oni_world::preflight::site_policy();
        let v = &dec.policy;
        if let Some(x) = v.get("co2_max_kg").and_then(|x| x.as_f64()) {
            pol.co2_max_kg = x.clamp(0.0, 20.0);
        }
        if let Some(x) = v.get("relax_after_fail").and_then(|x| x.as_u64()) {
            pol.relax_after_fail = x.min(10) as usize;
        }
        if let Some(x) = v.get("strict_prefabs").and_then(|x| x.as_array()) {
            pol.strict_prefabs = x.iter().filter_map(|y| y.as_str().map(|s| s.to_string())).collect();
        }
        oni_world::preflight::set_site_policy(pol.clone());
        // 落盘：跨进程保持（serve 重启后仍生效）
        let sd = save_dir();
        let _ = std::fs::create_dir_all(&sd);
        let _ = std::fs::write(sd.join("site_policy.json"),
            serde_json::to_string_pretty(&pol).unwrap_or_default());
        log.push(format!(
            "🔧 选址策略已调整: co2_max_kg={} strict={:?} relax_after_fail={}",
            pol.co2_max_kg, pol.strict_prefabs, pol.relax_after_fail
        ));
    }
    // 🔴 代码级熔断：prompt 是软约束，LLM 可能无视；这里是硬闸门。
    // 连续失败≥3次且未过冷却的 goal 绝不入队，并写一条 circuit_break 留痕进对话。
    let statuses = goal_status_snapshot();
    let now_ts = now();
    let broken = statuses.iter().find(|s| s.goal == dec.goal.trim()
        && is_circuit_broken(s, now_ts));
    if let Some(s) = broken {
        let note = format!("goal={} 连续失败{}次/成功{}次，最近「{}」→ 拒绝入队，强制换策略",
            s.goal, s.fails_since_success, s.total_ok, s.last_detail);
        log.push(format!("⛔ 熔断拒绝 {}", note));
        let sd = save_dir();
        let rec = serde_json::json!({
            "id": now_ts, "ts": now_ts,
            "goal": s.goal, "fails": s.fails_since_success,
            "detail": s.last_detail, "reason": note,
        });
        // 独立文件：不污染 decisions.jsonl 的「最近决策」，避免淹没真实决策。
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true)
            .open(sd.join("circuit_breaks.jsonl")) {
            let _ = writeln!(f, "{}", rec);
        }
    } else if push_ai_goal(&dec.goal, &dec.reason) {
        // 语义目标写进独立 ai_goals.jsonl，快系统用 autoact 翻译成工单执行。
        log.push(format!("ai_goal[{}]:已入队(快系统执行)", dec.goal));
    }
    for c in &dec.commands {
        // ★ 接口契约：大脑只能给带坐标的精确工单或元命令，其余拒绝并把原因回执进对话流，
        //   否则 LLM 不知道自己没做成，下一轮还会试。
        match validate_llm_command(c) {
            Ok(parts) => {
                let refs: Vec<&str> = parts.iter().map(|s| s.as_str()).collect();
                log.push(format!("$ oni {}: {}", c, run_oni(&refs)));
            }
            Err(reason) => log.push(format!("⛔ 拒绝 LLM 命令 '{}': {}", c, reason)),
        }
    }
    // 只要有 escalation 就 resolve —— 慢系统看过即解冻，不管选了什么动作。
    // 只要看过就 resolve，保持 pending 队列干净（慢系统不阻塞快系统）。
    if !escs.is_empty() {
        for e in escs {
            let id = e.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            let note = format!("慢系统[{}]决策({})：{}", dec.priority, dec.action, dec.reason);
            log.push(format!("resolve#{}:{}", id, run_oni(&["resolve", &id.to_string(), &note])));
        }
    }
    if dec.action == "command" || !cmds.is_empty() {
        if let Some(c) = cmds.first() {
            let id = c.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            let note = format!("慢系统执行用户指令: {} ({})", dec.reason, dec.choice);
            log.push(format!("cmddone#{}:{}", id,
                run_oni(&["command", "done", &id.to_string(), &note])));
        }
    }
    log.join(" | ")
}

fn load_json(path: std::path::PathBuf, default: Value) -> Value {
    std::fs::read_to_string(path).ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(default)
}

fn save_dir() -> std::path::PathBuf {
    let base = oni_mcp::save::save_dir(
        &oni_mcp::save::last_seen()
            .map(|s| s.fingerprint)
            .unwrap_or_else(|| "unknown".into()),
    );
    base
}

/// 组装对话流：四类角色构成完整三方对话链：
///   fast      快系统卡点（escalations/history.jsonl）—— 发现问题
///   assistant 慢系统决策（decisions.jsonl source=slow_service）—— 做决策
///   result    快系统执行反馈（goal_outcomes.jsonl）—— 执行结果回执
///   user      用户指令（commands.jsonl source=user）—— 人工介入
/// 每条带 status（成功/失败/待处理）与可选 status_label。按时间排序，按角色配额截断。
fn build_conversation() -> Vec<Value> {
    let sd = save_dir();
    let mut msgs: Vec<Value> = Vec::new();

    // 1) 用户指令（source=user，ai_goal 不进对话流避免噪音）
    if let Ok(text) = std::fs::read_to_string(sd.join("commands.jsonl")) {
        for line in text.lines() {
            let Ok(c) = serde_json::from_str::<Value>(line) else { continue };
            if c.get("source").and_then(|v| v.as_str()) != Some("user") { continue; }
            let decoded = decode_unicode_escapes(c.get("text").and_then(|v| v.as_str()).unwrap_or(""));
            let done = c.get("done").and_then(|v| v.as_bool()).unwrap_or(false);
            let result = c.get("result").and_then(|v| v.as_str()).unwrap_or("");
            let (status, label) = if !done { ("pending", "待处理") }
                else if result.contains("失败") || result.contains("未成功") { ("fail", "失败") }
                else { ("success", "完成") };
            msgs.push(serde_json::json!({
                "role": "user", "text": decoded, "status": status, "status_label": label,
                "result": result, "ts": c.get("ts").and_then(|v| v.as_i64()).unwrap_or(0),
            }));
        }
    }

    // 2) 慢系统决策
    if let Ok(text) = std::fs::read_to_string(sd.join("decisions.jsonl")) {
        let dlines: Vec<&str> = text.lines().rev().take(12).collect();
        for line in dlines.iter().rev() {
            let Ok(d) = serde_json::from_str::<Value>(line) else { continue };
            if d.get("source").and_then(|v| v.as_str()) != Some("slow_service") { continue; }
            let reason = d.get("reason").and_then(|v| v.as_str()).unwrap_or("");
            let goal = d.get("goal").and_then(|v| v.as_str()).unwrap_or("");
            let action = d.get("action").and_then(|v| v.as_str()).unwrap_or("");
            let priority = d.get("priority").and_then(|v| v.as_str()).unwrap_or("");
            let text = if goal.is_empty() { format!("[{}] {}", priority, reason) }
                else { format!("[{}] goal={} → {}", priority, goal, reason) };
            msgs.push(serde_json::json!({
                "role": "assistant", "text": text, "status": "", "status_label": "",
                "result": action, "ts": d.get("ts").and_then(|v| v.as_i64()).unwrap_or(0),
            }));
        }
    }

    // 3) 快系统卡点（escalation history）—— 发现问题
    //   ★ 执行结果从 bus 读（不改写 history）：
    //     resolve 时慢系统的决策 note 写进了 bus 的 Escalated→Resolved 事件，
    //     按 id 建索引，作为 fast 消息的 result 带进对话流。
    //     这样 UI 能显示「卡点→结论」，慢系统下轮能读到自己上次怎么处理的（反馈闭环）。
    let mut resolve_notes: std::collections::HashMap<i64, String> = std::collections::HashMap::new();
    if let Ok(bus_text) = std::fs::read_to_string(sd.join("bus/events.jsonl")) {
        for line in bus_text.lines() {
            if let Ok(ev) = serde_json::from_str::<Value>(line) {
                // Escalated 类事件被 resolve 后 state=Resolved，note=决策结论
                let is_escalated = ev.get("kind").and_then(|v| v.as_str()) == Some("Escalated");
                let is_resolved = ev.get("state").and_then(|v| v.as_str()) == Some("Resolved");
                if is_escalated && is_resolved {
                    if let (Some(id), Some(note)) = (
                        ev.get("id").and_then(|v| v.as_i64()),
                        ev.get("note").and_then(|v| v.as_str()),
                    ) {
                        if !note.is_empty() { resolve_notes.insert(id, note.to_string()); }
                    }
                }
            }
        }
    }
    if let Ok(text) = std::fs::read_to_string(sd.join("escalations/history.jsonl")) {
        let elines: Vec<&str> = text.lines().rev().take(10).collect();
        for line in elines.iter().rev() {
            let Ok(e) = serde_json::from_str::<Value>(line) else { continue };
            let ts = e.get("ts").and_then(|v| v.as_i64())
                .or_else(|| e.get("id").and_then(|v| v.as_i64())).unwrap_or(0);
            if ts == 0 { continue; }
            let kind = e.get("kind").and_then(|v| v.as_str()).unwrap_or("");
            let detail: String = e.get("detail").and_then(|v| v.as_str()).unwrap_or("").chars().take(90).collect();
            let node = e.get("node").and_then(|v| v.as_str()).unwrap_or("");
            // ★ 有 resolve note = 慢系统已处理且有结论；否则待处理。
            //   不依赖 history.acked（append-only 日志不该被重写）。
            let result = resolve_notes.get(&ts).cloned().unwrap_or_default();
            let handled = !result.is_empty();
            let (status, label) = if handled { ("success", "已处理") } else { ("pending", "待处理") };
            let text = if node.is_empty() { format!("[{}] {}", kind, detail) }
                else { format!("[{}][{}] {}", kind, node, detail) };
            msgs.push(serde_json::json!({
                "role": "fast", "text": text, "status": status, "status_label": label,
                "result": result, "ts": ts,
            }));
        }
    }

    // 4) 快系统执行反馈（goal outcomes）—— 执行结果回执
    if let Ok(text) = std::fs::read_to_string(sd.join("goal_outcomes.jsonl")) {
        let olines: Vec<&str> = text.lines().rev().take(10).collect();
        for line in olines.iter().rev() {
            let Ok(o) = serde_json::from_str::<Value>(line) else { continue };
            let goal = o.get("goal").and_then(|v| v.as_str()).unwrap_or("");
            if goal.is_empty() { continue; }
            let ok = o.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
            let detail: String = o.get("detail").and_then(|v| v.as_str()).unwrap_or("").chars().take(80).collect();
            let text = format!("goal={} → {}: {}", goal, if ok { "成功" } else { "失败" }, detail);
            msgs.push(serde_json::json!({
                "role": "result", "text": text,
                "status": if ok { "success" } else { "fail" },
                "status_label": if ok { "成功" } else { "失败" },
                "result": "", "ts": o.get("ts").and_then(|v| v.as_i64()).unwrap_or(0),
            }));
        }
    }

    // 5) 熔断拒绝（circuit_breaks.jsonl）—— 系统硬闸门拦下重复失败目标
    if let Ok(text) = std::fs::read_to_string(sd.join("circuit_breaks.jsonl")) {
        let clines: Vec<&str> = text.lines().rev().take(6).collect();
        for line in clines.iter().rev() {
            let Ok(b) = serde_json::from_str::<Value>(line) else { continue };
            let goal = b.get("goal").and_then(|v| v.as_str()).unwrap_or("");
            let fails = b.get("fails").and_then(|v| v.as_u64()).unwrap_or(0);
            let detail: String = b.get("detail").and_then(|v| v.as_str()).unwrap_or("").chars().take(50).collect();
            msgs.push(serde_json::json!({
                "role": "circuit",
                "text": format!("goal={} 连续失败{}次 → 拒绝重复，强制换策略：{}", goal, fails, detail),
                "status": "fail", "status_label": "已熔断",
                "result": "", "ts": b.get("ts").and_then(|v| v.as_i64()).unwrap_or(0),
            }));
        }
    }

    // 按时间排序 → 按角色配额截断（保留各类最近若干条）→ 再按时间排序
    msgs.sort_by_key(|m| m.get("ts").and_then(|v| v.as_i64()).unwrap_or(0));
    let take_role = |role: &str, n: usize| -> Vec<Value> {
        let mut v: Vec<Value> = msgs.iter()
            .filter(|m| m.get("role").and_then(|x| x.as_str()) == Some(role))
            .cloned().collect();
        if v.len() > n { v = v[v.len()-n..].to_vec(); }
        v
    };
    let mut out = take_role("user", 12);
    out.extend(take_role("assistant", 12));
    out.extend(take_role("fast", 8));
    out.extend(take_role("result", 8));
    out.extend(take_role("circuit", 5));
    out.sort_by_key(|m| m.get("ts").and_then(|v| v.as_i64()).unwrap_or(0));
    out
}

fn collect_state(cfg: &SlowConfig) -> SlowView {
    let mut view = SlowView::default();
    let sd = save_dir();
    let snap = load_json(sd.join("live_snapshot.json"), Value::Null);
    view.cycle = snap.get("cycle").and_then(|v| v.as_i64()).unwrap_or(0);
    view.paused = snap.get("paused").and_then(|v| v.as_bool()).unwrap_or(true);
    // 局内会话纪元：指纹 + 局内状态目录的创建时刻（clear_save_state 重建目录 → 纪元变化）。
    //   前端看到纪元变了就清空对话流（换局/读档重开）。
    view.save_fingerprint = oni_mcp::save::last_seen().map(|s| s.fingerprint).unwrap_or_default();
    // ★ 方向盘模式（v3 ⑦）：有未完成的用户指令 = 指令模式；否则自主模式。前端据此显示。
    {
        let pending_cmd = std::fs::read_to_string(sd.join("commands.jsonl"))
            .map(|t| t.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok())
                .any(|c| !c.get("done").and_then(|v| v.as_bool()).unwrap_or(false)))
            .unwrap_or(false);
        view.control_mode = if pending_cmd { "指令模式" } else { "自主模式" }.into();
    }
    view.session_epoch = std::fs::metadata(&sd).and_then(|m| m.created()).ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64).unwrap_or(0);
    let diag = snap.get("diagnostics").unwrap_or(&Value::Null);
    view.food_kcal = diag.get("foodKcal").and_then(|v| v.as_f64()).unwrap_or(0.0);
    view.oxygen_kg = diag.get("oxygenKgVisible").and_then(|v| v.as_f64()).unwrap_or(0.0);
    view.breathable_cells = diag.get("breathableCellsVisible").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
    view.dupes = snap.get("dupes").and_then(|v| v.as_array()).cloned().unwrap_or_default();

    let escs = load_json(sd.join("escalations/pending.json"), serde_json::json!([]));
    view.escalations = escs.as_array().cloned().unwrap_or_default();

    let mut cmds = Vec::new();
    if let Ok(text) = std::fs::read_to_string(sd.join("commands.jsonl")) {
        for line in text.lines() {
            if let Ok(mut c) = serde_json::from_str::<Value>(line) {
                // ★ 只读真用户指令：历史遗留的 ai_goal 不算"用户指令"
                if c.get("source").and_then(|v| v.as_str()) != Some("user") { continue; }
                if c.get("done").and_then(|v| v.as_bool()).unwrap_or(false) { continue; }
                // text 里是 unicode 转义，解开给人看
                if let Some(s) = c.get("text").and_then(|v| v.as_str()) {
                    let decoded = decode_unicode_escapes(s);
                    c["text"] = Value::String(decoded);
                }
                cmds.push(c);
            }
        }
    }
    view.commands = cmds;

    let mut decs = Vec::new();
    if let Ok(text) = std::fs::read_to_string(sd.join("decisions.jsonl")) {
        for line in text.lines().rev().take(10) {
            if let Ok(d) = serde_json::from_str::<Value>(line) { decs.push(d); }
        }
    }
    view.recent_decisions = decs;
    view.conversation = build_conversation();
    view.mode = if cfg.is_mock() { "mock".into() } else { cfg.model.clone() };
    view.ai_enabled = !crate::agent::hold_active();
    view.updated_at = now();
    view
}

fn decode_unicode_escapes(s: &str) -> String {
    // 兼容两种输入：
    //   1) Web 输入框提交的**真实 UTF-8**（优先）—— 原样返回，绝不能逐字节拆
    //   2) 旧 commands.jsonl 里的 \uXXXX **字面串** —— 只解码这种
    if !s.contains("\\u") {
        return s.to_string();
    }
    let mut out = String::new();
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\\' && it.peek() == Some(&'u') {
            it.next(); // consume 'u'
            let hex: String = it.by_ref().take(4).collect();
            if let Ok(cp) = u32::from_str_radix(&hex, 16) {
                if let Some(ch) = char::from_u32(cp) { out.push(ch); continue; }
            }
            // 解析失败 → 原样拼回
            out.push('\\'); out.push('u'); out.push_str(&hex);
        } else {
            out.push(c);
        }
    }
    out
}

/// 本进程首次调用的时间戳（慢系统"只信本进程传感器"的基准）。
fn process_start_ts() -> i64 {
    static T: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
    *T.get_or_init(now)
}

/// 传感器快照是否可用于规划：ts 不早于本进程启动、传感器已报出人口、且**心跳正常**（不是过期缓存）。
pub fn sensor_snapshot_fresh(snap: &Value, process_start: i64) -> bool {
    let ts = snap.get("ts").and_then(|v| v.as_i64()).unwrap_or(0);
    let has_pop = snap.get("dupes").and_then(|v| v.as_array()).map(|a| !a.is_empty()).unwrap_or(false);
    let stale = snap.get("health").and_then(|v| v.as_str()) == Some("stale");
    ts >= process_start && has_pop && !stale
}

/// 终局判定（纯函数，可测）：只有**确知**复制人名单且全员倒地/死亡才返回终局原因。
///
/// 规则（AGENTS.md §9：人口读不到 ≠ 人口为 0）——慢系统首拍可能早于快系统第一次采集，live_snapshot 还是 Null：
///     · dupes 缺失 / 为空 → None（数据源可疑，真正的人口 0 由快系统检测）
///     · 有人 breath > 0 且 chore 不含倒地 → 活着
///     · 只有全员 breath ≤ 0 或 chore 明确倒地/死亡 → Some(终局原因)
pub fn colony_lost(snap: &Value) -> Option<&'static str> {
    let arr = snap.get("dupes")?.as_array()?;
    if arr.is_empty() { return None; }
    let alive = arr.iter().filter(|d| {
        let chore = d.get("chore").and_then(|v| v.as_str()).unwrap_or("");
        let downed = chore.contains("失去行动") || chore.contains("倒地")
            || chore.contains("Downing") || chore.contains("Unconscious")
            || chore.contains("死亡");
        let breath = d.get("breath").and_then(|v| v.as_f64()).unwrap_or(100.0);
        !downed && breath > 0.0
    }).count();
    if alive == 0 {
        Some("☠️ 全员失去行动能力且无法自救（无人能执行工单）→ 停止决策，建议回滚存档或新开一局")
    } else {
        None
    }
}

/// 跑一轮慢系统决策。返回决策摘要。
pub fn run_once(cfg: &SlowConfig, view: &Arc<Mutex<SlowView>>) -> String {
    let (escs, cmds, snap) = {
        let sd = save_dir();
        let e = load_json(sd.join("escalations/pending.json"), serde_json::json!([]));
        let mut c = Vec::new();
        if let Ok(text) = std::fs::read_to_string(sd.join("commands.jsonl")) {
            for line in text.lines() {
                if let Ok(d) = serde_json::from_str::<Value>(line) {
                    // ★ 只读真用户指令（ai_goal 已独立到 ai_goals.jsonl）
                    if d.get("source").and_then(|v| v.as_str()) != Some("user") { continue; }
                    if !d.get("done").and_then(|v| v.as_bool()).unwrap_or(false) { c.push(d); }
                }
            }
        }
        let s = load_json(sd.join("live_snapshot.json"), Value::Null);
        (e.as_array().cloned().unwrap_or_default(), c, s)
    };

    // ★ 元决策：全员倒地/人口0 → 跳过 LLM，直接给出回滚建议（快系统已识别并暂停，慢系统不再选 goal）。
    // ★ 全灭判定规则（AGENTS.md §9：人口为 0 先确认数据源没失败）：
    //     · dupes 数组缺失/为空 → **不判全灭**（采集中间态可能为空，交给快系统的人口检测）
    //     · 有人 breath > 0 且 chore 不含倒地 → 算活着（用结构化信号 breath，不靠 chore 文本）
    //     · 只有全员 breath==0 或 chore 明确倒地/死亡 → 才判全灭
    if let Some(reason) = colony_lost(&snap) {
        let sd = save_dir();
        // 节流：终局是稳定状态，首次写完整记录 + bus 告警即可，后续轮次只更新 view，
        // 避免每 5 秒刷一条 meta_give_up 污染 decisions.jsonl。
        let already_logged = std::fs::read_to_string(sd.join("decisions.jsonl"))
            .ok().and_then(|t| t.lines().rev().find(|l| !l.trim().is_empty()).map(|l| l.to_string()))
            .map(|l| l.contains("meta_give_up")).unwrap_or(false);
        if !already_logged {
            let rec = serde_json::json!({
                "id": now(), "ts": now(),
                "cycle": snap.get("cycle").and_then(|v| v.as_i64()).unwrap_or(0),
                "source": "slow_service", "choice": "meta_give_up", "action": "rollback_advice",
                "priority": "P0", "reason": reason, "goal": "", "commands": [],
            });
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true)
                .open(sd.join("decisions.jsonl")) {
                let _ = writeln!(f, "{}", rec);
            }
            let _ = crate::bus::publish(
                crate::bus::Kind::AuditFinding, crate::bus::Priority::Critical,
                "meta", reason, serde_json::json!({"dupes": snap.get("dupes").and_then(|v| v.as_array()).map(|a| a.len())}), 7200,
            );
        }
        let summary = format!("[meta][P0] ☠️ 终局判定：{}", reason);
        if let Ok(mut v) = view.lock() { v.last_decision = summary.clone(); }
        return summary;
    }

    // ★ **开局规划**（慢系统契约 §4：战略层负责"往哪走"）：
    //   计划不是 AI 生的且快系统已采好世界事实（opening_brief.json）
    //   → 用 LLM 依据**本局事实**重新规划（author=ai）→ 强校验 → 落盘。
    //   兜底链：LLM 失败/校验不过 → 本轮无战略计划（快系统保命层照常），失败冷却 10 分钟。
    // ★ 传感器新鲜度：慢系统只规划**本进程快系统采到的**快照（上一个进程留下的 live_snapshot.json
    //   对应的 session 可能已冻结）。快照 ts 早于本进程启动，或传感器还没报人口 → 本轮不重算窗口，等下一拍。
    let sensor_fresh = sensor_snapshot_fresh(&snap, process_start_ts());
    if !sensor_fresh { eprintln!("[slow:diag] 传感器快照未就绪（ts 早于本进程或缺 dupes）→ 本轮不重算窗口"); }
    if sensor_fresh {
        let sd = save_dir();
        let plan_text = std::fs::read_to_string("plan.json").unwrap_or_default();
        let need = !crate::opening::plan_author_is_ai(&plan_text);
        let brief = std::fs::read_to_string(sd.join("opening_brief.json")).ok();
        let cooling = std::fs::read_to_string(sd.join("opening_state.json")).ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .and_then(|v| v.get("last_fail_ts").and_then(|x| x.as_i64()))
            .map(|t| now() - t < 600)
            .unwrap_or(false);
        if need && !cooling {
            if let Some(brief_text) = brief {
                let fp = oni_mcp::save::last_seen().map(|s| s.fingerprint);
                let outcome = crate::opening::author_opening_plan(
                    cfg, &brief_text, "plan.json", fp.as_deref());
                match outcome {
                    Ok(summary) => {
                        let rec = serde_json::json!({
                            "id": now(), "ts": now(),
                            "cycle": snap.get("cycle").and_then(|v| v.as_i64()).unwrap_or(0),
                            "source": "slow_service", "choice": "opening_plan",
                            "action": "author_plan", "priority": "P1",
                            "reason": "开局战略由 LLM 依据本局世界事实生成（替代硬编码模板）",
                            "goal": "", "commands": [],
                        });
                        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true)
                            .open(sd.join("decisions.jsonl")) { let _ = writeln!(f, "{}", rec); }
                        let _ = std::fs::rename(sd.join("opening_brief.json"), sd.join("opening_brief.done"));
                        let out = format!("[opening] 📝 {}", summary);
                        if let Ok(mut v) = view.lock() { v.last_decision = out.clone(); }
                        eprintln!("[slow] {}", out);
                        return out;
                    }
                    Err(reason) => {
                        let rec = serde_json::json!({
                            "id": now(), "ts": now(),
                            "cycle": snap.get("cycle").and_then(|v| v.as_i64()).unwrap_or(0),
                            "source": "slow_service", "choice": "opening_plan",
                            "action": "fallback_template", "priority": "P1",
                            "reason": format!("LLM 开局规划失败：{reason}；本轮无战略计划（快系统保命层照常），10 分钟后重试"),
                            "goal": "", "commands": [],
                        });
                        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true)
                            .open(sd.join("decisions.jsonl")) { let _ = writeln!(f, "{}", rec); }
                        let _ = std::fs::write(sd.join("opening_state.json"),
                            serde_json::to_string(&serde_json::json!({"last_fail_ts": now()})).unwrap_or_default());
                        let out = format!("[opening] ⚠ LLM 开局规划失败，plan 保持空占位；快系统保命反射弧照常（不依赖 plan），10 分钟后重试：{reason}");
                        if let Ok(mut v) = view.lock() { v.last_decision = out.clone(); }
                        eprintln!("[slow] {}", out);
                        // 不 return：本轮继续正常决策（快系统保命层不依赖 plan）
                    }
                }
            }
        }
    }

    // ★ v3 滚动窗口：plan.json 是 horizon-3 的短视窗口，过期就重算（MPC 式）。
    //   静态长链在世界变化后变成枷锁。
    //   过期条件见 window::window_stale（空 / 全完成 / 枷锁 / 跨 ≥2 周期）。
    if sensor_fresh {
        let sd = save_dir();
        let plan_path = "plan.json";
        let snap_cycle = snap.get("cycle").and_then(|v| v.as_i64()).unwrap_or(0);
        let plan: Option<crate::decide::Plan> = std::fs::read_to_string(plan_path)
            .ok().and_then(|t| serde_json::from_str(&t).ok());
        // 开局规划失败留下的空占位（author=pending_llm）也允许滚动窗口接管：
        //   否则 opening 的 10 分钟冷却期里 plan 一直是空的，殖民地没战略。
        //   滚动窗口只要 3 节点 / 8000 token，比 14 节点的 opening 图稳得多。
        let author = plan.as_ref().and_then(|p| p.author.as_deref()).unwrap_or("");
        let author_is_ai = author == "ai" || author == "pending_llm";
        let stale = plan.as_ref().and_then(|p| crate::window::window_stale_with(p, snap_cycle, Some(&sd)));
        eprintln!("[window:diag] plan_loaded={} author_is_ai={} stale={:?} mock={}", plan.is_some(), author_is_ai, stale, cfg.is_mock());
        // 只对 AI 产的 plan 做滚动刷新；opening 占位（pending_llm）由上面的开局分支处理。
        // mock 模式不调 LLM（测试环境无端点）。
        if author_is_ai && stale.is_some() && !cfg.is_mock() {
            // 事实来源：**优先用当前快照**（滚动窗口要的是"现在"，不是开局时的 brief）。
            //   开局 brief 成功后会被重命名为 .done 或被 plan-reset 清掉，不能依赖它。
            // ★ 事实要**结构化摘要**，不是整坨快照 JSON：LLM 在大 JSON 里找不到关键数字
            //   （如 inventory 为空推不出"零建材 → 必须先挖"，窗口 W1 直接排建造 → 全部失败）。
            let brief = crate::window::facts_summary(&snap, snap_cycle);
            {
                let fp = oni_mcp::save::last_seen().map(|s| s.fingerprint);
                match crate::window::refresh_window(cfg, &brief, plan_path, fp.as_deref(), snap_cycle, &sd) {
                    Ok(summary) => {
                        let rec = serde_json::json!({
                            "id": now(), "ts": now(), "cycle": snap_cycle,
                            "source": "slow_service", "choice": "window_refresh",
                            "action": "author_window", "priority": "P1",
                            "reason": format!("滚动窗口过期（{}）→ 重算：{}", stale.clone().unwrap_or_default(), summary),
                            "goal": "", "commands": [],
                        });
                        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true)
                            .open(sd.join("decisions.jsonl")) { let _ = writeln!(f, "{}", rec); }
                        let out = format!("[window] 🔄 {}（过期原因：{}）", summary, stale.unwrap_or_default());
                        if let Ok(mut v) = view.lock() { v.last_decision = out.clone(); }
                        eprintln!("[slow] {}", out);
                    }
                    Err(reason) => {
                        let out = format!("[window] ⚠ 滚动窗口刷新失败（保留旧窗口）：{}", reason);
                        if let Ok(mut v) = view.lock() { v.last_decision = out.clone(); }
                        eprintln!("[slow] {}", out);
                    }
                }
            }
        }
    }

    // ★ v3 ⑤ 事件驱动：同一 escalation / 用户指令只触发一次 LLM 决策（见 event_gate）。
    let fresh = event_gate(&escs, &cmds);
    let has_fresh = !fresh.is_empty();
    // ★ 主动规划心跳：无新事件时才走 60s 心跳；有新事件直接决策。
    let proactive = !has_fresh;
    if proactive && !should_proactive() {
        return "idle".into();
    }
    if proactive { mark_proactive(); }
    // 只把**新**事件喂给 LLM：已处理过的卡点/指令不再重复出现在 prompt 里。
    let escs: Vec<Value> = if has_fresh { fresh.iter().filter(|e| e.get("kind").is_some()).cloned().collect() } else { escs };
    let cmds: Vec<Value> = if has_fresh { fresh.iter().filter(|e| e.get("kind").is_none()).cloned().collect() } else { cmds };

    let prompt = build_prompt(&escs, &cmds, &snap, proactive);
    let decision = if cfg.is_mock() {
        mock_decision(&prompt, !escs.is_empty(), !cmds.is_empty(), proactive)
    } else {
        match call_llm(cfg, SYSTEM_PROMPT, &prompt, 4096).and_then(|t| parse_decision(&t)) {
            Ok(d) => d,
            Err(e) => {
                let d = mock_decision(&prompt, !escs.is_empty(), !cmds.is_empty(), proactive);
                let mut m = d;
                m.reason = format!("LLM失败({})→降级mock: {}", e, m.reason);
                m
            }
        }
    };

    let log = execute_decision(&decision, &escs, &cmds);
    let summary = format!("[{}][{}] choice={} action={} reason={} | {}",
        if cfg.is_mock() { "mock" } else { &cfg.model },
        decision.priority, decision.choice, decision.action, decision.reason, log);

    // 写决策留痕
    let sd = save_dir();
    let rec = serde_json::json!({
        "id": now(), "ts": now(),
        "cycle": snap.get("cycle").and_then(|v| v.as_i64()).unwrap_or(0),
        "source": "slow_service", "choice": decision.choice,
        "action": decision.action, "priority": decision.priority, "reason": decision.reason,
        "goal": decision.goal, "commands": decision.commands,
    });
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true)
        .open(sd.join("decisions.jsonl")) {
        let _ = writeln!(f, "{}", rec);
    }
    if let Ok(mut v) = view.lock() {
        v.last_decision = summary.clone();
    }
    summary
}


fn json_response(stream: &mut TcpStream, status: &str, body: &str, content_type: &str) {
    let resp = format!("HTTP/1.1 {}\r\nContent-Type: {}; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        status, content_type, body.len(), body);
    let _ = stream.write_all(resp.as_bytes());
}

fn serve_web(cfg: &SlowConfig, view: Arc<Mutex<SlowView>>) {
    let addr = format!("0.0.0.0:{}", cfg.web_port);
        // ★ 绑定失败要**重试**，不能一失败就退出：重启 serve 时旧进程的端口还在 TIME_WAIT
        //   → "Address already in use" → web 线程静默死掉 → 控制台整个不可用。
    let listener = {
        let mut last_err = None;
        let mut listener = None;
        for attempt in 1..=15 {
            match TcpListener::bind(&addr) {
                Ok(l) => { listener = Some(l); break; }
                Err(e) => {
                    last_err = Some(e);
                    eprintln!("[slow] web bind {} failed (第{}次): {}", addr, attempt, last_err.as_ref().unwrap());
                    std::thread::sleep(Duration::from_secs(2));
                }
            }
        }
        match listener {
            Some(l) => l,
            None => {
                eprintln!("[slow] web bind {} 彻底失败，放弃 web 服务: {}", addr,
                    last_err.map(|e| e.to_string()).unwrap_or_default());
                return;
            }
        }
    };
    eprintln!("[slow] web: http://localhost:{}/", cfg.web_port);
    for stream in listener.incoming() {
        let mut stream = match stream { Ok(s) => s, Err(_) => continue };
        let mut buf = [0u8; 8192];
        let n = stream.read(&mut buf).unwrap_or(0);
        let req = String::from_utf8_lossy(&buf[..n]);
        let first = req.lines().next().unwrap_or("");
        let parts: Vec<&str> = first.split_whitespace().collect();
        let method = parts.get(0).copied().unwrap_or("");
        let path = parts.get(1).copied().unwrap_or("/");
        let body = req.split("\r\n\r\n").nth(1).unwrap_or("");
        match (method, path) {
            ("GET", "/") | ("GET", "/index.html") => {
                let page = std::fs::read_to_string("web/index.html")
                    .unwrap_or_else(|_| "<h1>web/index.html not found</h1>".into());
                json_response(&mut stream, "200 OK", &page, "text/html");
            }
            ("GET", "/api/state") => {
                // 实时读快系统落盘的快照，不返回慢循环缓存的 view ——
                // LLM 推理(可能几十秒)期间 view 不刷新，页面看起来卡住。
                // collect_state 只读文件(<1ms)，每次请求都拿最新真值。
                // last_decision 是慢系统独有的，从缓存保留；其余字段全部实时。
                let mut fresh = collect_state(cfg);
                if let Ok(old) = view.lock() {
                    fresh.last_decision = old.last_decision.clone();
                }
                let v = serde_json::to_string(&fresh).unwrap_or_default();
                json_response(&mut stream, "200 OK", &v, "application/json");
            }
            ("POST", "/api/command") => {
                // ★ 前端发 JSON {"text": ...}；裸 body 只作兼容回退。
                //   裸 body 经过 from_utf8_lossy 后多字节字符可能被截断在 8192 缓冲边界。
                let text = serde_json::from_str::<Value>(body).ok()
                    .and_then(|v| v.get("text").and_then(|t| t.as_str()).map(|s| s.to_string()))
                    .unwrap_or_else(|| decode_unicode_escapes(body.trim()));
                let out = run_oni(&["command", "add", &text]);
                json_response(&mut stream, "200 OK", &serde_json::json!({"ok":true,"out":out}).to_string(), "application/json");
            }
            ("POST", "/api/decide") => {
                let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
                let id = v.get("id").and_then(|x| x.as_u64()).unwrap_or(0);
                let choice = v.get("choice").and_then(|x| x.as_str()).unwrap_or("");
                let out = run_oni(&["resolve", &id.to_string(), &format!("web pick [{}]", choice)]);
                json_response(&mut stream, "200 OK", &serde_json::json!({"ok":true,"out":out}).to_string(), "application/json");
            }
            (m, p) if m == "POST" && p.starts_with("/api/ctl/") => {
                let op = &p[9..]; // after "/api/ctl/"
                let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
                let out = match op {
                    // ★ AI 总开关：
                    //   stop = hold（持久停 agent+游戏）
                    //   start = release（清 hold）+ window（授权 agent 恢复游戏并接管）
                    //   只 release 不 window 的话：游戏暂停来源是原生/owner=none，
                    //   快系统保守判「用户暂停」不越权恢复 → 点了启动却不动。
                    "stop" => run_oni(&["hold", "web 停止AI"]),
                    "start" => {
                        let r = run_oni(&["release"]);
                        let w = run_oni(&["window", "120"]);
                        format!("{} | {}", r, w)
                    },
                    // 兼容旧按钮（保留但 UI 不再暴露）
                    "pause" => run_oni(&["pause", "web 暂停"]),
                    "resume" => run_oni(&["resume", "web 继续"]),
                    "window" => run_oni(&["window", "120"]),
                    "speed" => {
                        let level = v.get("speed").and_then(|x| x.as_u64()).unwrap_or(1);
                        run_oni(&["call", "game_control",
                            &format!("{{\"domain\":\"speed\",\"action\":\"set_speed\",\"speed\":{},\"task\":\"web 调速\"}}", level)])
                    },
                    _ => "unknown op".into(),
                };
                json_response(&mut stream, "200 OK", &serde_json::json!({"ok":true,"op":op,"out":out}).to_string(), "application/json");
            }
            _ => json_response(&mut stream, "404 Not Found", "not found", "text/plain"),
        }
    }
}

pub fn run(cfg: SlowConfig, once: bool) {
    let view = Arc::new(Mutex::new(SlowView::default()));
    eprintln!("[slow] start mode={} interval={}s web=:{}",
        if cfg.is_mock() { "mock" } else { &cfg.model }, cfg.interval_secs, cfg.web_port);
    if !once {
        let web_view = Arc::clone(&view);
        let web_cfg = cfg.clone();
        std::thread::spawn(move || serve_web(&web_cfg, web_view));
    }
    loop {
        let snap_view = collect_state(&cfg);
        if let Ok(mut v) = view.lock() { *v = snap_view; }
        let summary = run_once(&cfg, &view);
        if summary != "idle" { eprintln!("[slow] {}", summary); }
        if once { break; }
        std::thread::sleep(Duration::from_secs(cfg.interval_secs));
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    /// hold = 用户关闭 AI 总开关（唯一用户停止信号，AGENTS.md §7）：大脑不得发总开关类命令。
    /// hold 期间大脑不得发总开关类命令（hold/release/pause/resume/window）。
    #[test]
    fn llm_cannot_flip_master_switch_during_hold() {
        crate::agent::isolate_state_dir();
        let _ = crate::agent::clear_hold();
        for c in ["release", "window 120", "resume", "pause", "hold x"] {
            assert!(validate_llm_command(c).is_ok(), "非 hold 期 {c} 应放行");
        }
        crate::agent::write_hold("test", 7).expect("write hold");
        assert!(crate::agent::hold_active(), "isolate 后 hold 必须可见（且不碰真实目录）");
        for c in ["release", "window 120", "resume", "pause", "hold x"] {
            let e = validate_llm_command(c).unwrap_err();
            assert!(e.contains("总开关"), "{c}: {e}");
        }
        assert!(validate_llm_command("research ImprovedOxygen").is_ok(), "研究方向不是总开关，hold 期间仍可下");
        assert!(validate_llm_command("policy set co2_max_kg=3").is_ok(), "非总开关元命令不受影响");
        assert!(validate_llm_command("resolve 1").is_ok());
        assert!(validate_llm_command("preflight dig 124,192").is_ok(), "精确工单不受影响");
        // 研究方向/打印舱是战略决定（归大脑）：元命令不需要坐标，hold 期间也不该被误拦
        assert!(validate_llm_command("research ImprovedOxygen").is_ok(), "研究元命令");
        assert!(validate_llm_command("pod claim 1").is_ok(), "打印舱元命令");
        crate::agent::clear_hold();
        assert!(!crate::agent::hold_active());
    }

    /// 慢系统只规划**本进程**快系统采到的快照：上一个进程留下的文件（session 已冻结）不算。
    #[test]
    fn sensor_snapshot_fresh_requires_this_process_and_population() {
        let start = 1000;
        let fresh = serde_json::json!({"ts": 1005, "dupes": [{"name": "t"}]});
        assert!(sensor_snapshot_fresh(&fresh, start));
        let from_old_process = serde_json::json!({"ts": 900, "dupes": [{"name": "t"}]});
        assert!(!sensor_snapshot_fresh(&from_old_process, start), "早于本进程启动的快照不能用来规划");
        let no_pop = serde_json::json!({"ts": 1005, "dupes": []});
        assert!(!sensor_snapshot_fresh(&no_pop, start), "传感器没报人口 → 不规划");
        assert!(!sensor_snapshot_fresh(&Value::Null, start), "Null 快照 → 不规划");
        let stale = serde_json::json!({"ts": 1005, "dupes": [{"name": "t"}], "health": "stale"});
        assert!(!sensor_snapshot_fresh(&stale, start), "心跳未推进的过期快照 → 不规划");
        let degraded = serde_json::json!({"ts": 1005, "dupes": [{"name": "t"}], "health": "degraded"});
        assert!(sensor_snapshot_fresh(&degraded, start), "降级但心跳正常 → 仍可规划（简报里已标注不可信字段）");
    }

    /// 终局判定必须门控"确知名单"：快照 Null / dupes 缺失 / 空数组都不算全灭。
    #[test]
    fn colony_lost_requires_known_dupe_list() {
        assert_eq!(colony_lost(&Value::Null), None, "快照 Null ≠ 全灭");
        assert_eq!(colony_lost(&serde_json::json!({"cycle": 1})), None, "dupes 缺失 ≠ 全灭");
        assert_eq!(colony_lost(&serde_json::json!({"dupes": []})), None, "dupes 为空 ≠ 全灭");
    }

    /// 有人活着就不判终局；全员倒地或呼吸 0 才判。
    #[test]
    fn colony_lost_only_when_everyone_down() {
        let alive = serde_json::json!({"dupes": [
            {"chore": "空闲", "breath": 100.0},
            {"chore": "Downing", "breath": 0.0},
        ]});
        assert_eq!(colony_lost(&alive), None, "还有一人活着");
        let lost = serde_json::json!({"dupes": [
            {"chore": "Downing", "breath": 0.0},
            {"chore": "空闲", "breath": 0.0},
        ]});
        assert!(colony_lost(&lost).is_some(), "全员倒地/呼吸 0 → 终局");
    }

    /// 战略状态机：只挑 requires 全 done 的节点，跳过来满足依赖的。
    #[test]
    fn next_actionable_respects_dependencies() {
        let nodes: Vec<Value> = vec![
            serde_json::json!({"name":"A","status":"done","requires":[]}),
            serde_json::json!({"name":"B","status":"todo","requires":["A"]}),
            serde_json::json!({"name":"C","status":"todo","requires":["A","X"]}),
            serde_json::json!({"name":"D","status":"todo","requires":[]}),
        ];
        let mut done = std::collections::HashSet::new();
        done.insert("A".to_string());
        let next = next_actionable_names(&nodes, &done);
        assert!(next.contains(&"B".to_string()), "A done → B 应可推进");
        assert!(!next.contains(&"C".to_string()), "X 未 done → C 不可推进");
        assert!(next.contains(&"D".to_string()), "无依赖节点应可推进");
    }

    #[test]
    /// 回归：detail 说"零进展"时，即使 ok=true 也必须按失败计（否则熔断计数不增长，快系统恢复→失败→冻回无限乒乓）。
    #[test]
    fn zero_progress_counts_as_failure_even_when_ok_flagged_true() {
        let lines = [
            r#"{"goal":"material","ok":true,"detail":"前沿 14 格无可下单曲（全部需梯子或已有标记）→ 零进展，等梯子/复制人消化；该目标当前推不动","ts":100}"#,
            r#"{"goal":"material","ok":true,"detail":"前沿 14 格无可下单曲（全部需梯子或已有标记）→ 零进展，等梯子/复制人消化；该目标当前推不动","ts":101}"#,
            r#"{"goal":"material","ok":true,"detail":"前沿 14 格无可下单曲（全部需梯子或已有标记）→ 零进展，等梯子/复制人消化；该目标当前推不动","ts":102}"#,
        ];
        let st = compute_goal_status(&lines);
        let m = st.iter().find(|s| s.goal == "material").unwrap();
        assert_eq!(m.fails_since_success, 3, "零进展必须累计为连续失败");
        assert_eq!(m.total_fail, 3);
        assert_eq!(m.total_ok, 0, "零进展绝不能计入成功次数");
        assert!(is_circuit_broken(m, 200), "连续3次零进展 → 必须熔断，阻止无限重试");
    }

    #[test]
    fn circuit_breaker_counts_consecutive_failures_and_resets_on_success() {
        let lines = [
            r#"{"goal":"oxygen","ok":false,"detail":"fail1","ts":100}"#,
            r#"{"goal":"material","ok":true,"detail":"ok","ts":101}"#,
            r#"{"goal":"oxygen","ok":false,"detail":"fail2","ts":102}"#,
            r#"{"goal":"oxygen","ok":false,"detail":"fail3","ts":103}"#,
            r#"{"goal":"oxygen","ok":false,"detail":"fail4","ts":104}"#,
            r#"{"goal":"material","ok":false,"detail":"m-fail","ts":105}"#,
        ];
        let st = compute_goal_status(&lines);
        let o = st.iter().find(|s| s.goal == "oxygen").unwrap();
        assert_eq!(o.fails_since_success, 4);
        assert_eq!(o.total_fail, 4); assert_eq!(o.total_ok, 0);
        assert_eq!(o.last_fail_ts, 104);
        // oxygen 从未成功(total_ok=0) + 连续失败4次 → 永久熔断（冷却过期也不放行）
        assert!(is_circuit_broken(o, 104 + 100));
        assert!(is_circuit_broken(o, 104 + FAIL_CIRCUIT_COOLDOWN_SECS + 1),
            "从未成功的 goal 应永久熔断，防止 oxygen 0/8 反复重试拖垮整局");
        // material 只有 1 次失败（且此前成功过）→ 不熔断
        let m = st.iter().find(|s| s.goal == "material").unwrap();
        assert_eq!(m.fails_since_success, 1);
        assert_eq!(m.total_ok, 1);
        assert!(!is_circuit_broken(m, 200));
        // 失败后出现成功 → 连续计数清零
        let lines2 = [
            r#"{"goal":"oxygen","ok":false,"detail":"f","ts":100}"#,
            r#"{"goal":"oxygen","ok":false,"detail":"f","ts":101}"#,
            r#"{"goal":"oxygen","ok":false,"detail":"f","ts":102}"#,
            r#"{"goal":"oxygen","ok":true,"detail":"ok","ts":103}"#,
        ];
        let st2 = compute_goal_status(&lines2);
        let o2 = st2.iter().find(|s| s.goal == "oxygen").unwrap();
        assert_eq!(o2.fails_since_success, 0);
        assert_eq!(o2.total_ok, 1);
        assert!(!is_circuit_broken(o2, 200));
    }

    #[test]
    fn decode_preserves_real_utf8_and_decodes_escapes() {
        // Web 输入框的真实 UTF-8：必须原样，不能被逐字节拆成 Latin-1
        assert_eq!(decode_unicode_escapes("优先采集食物"), "优先采集食物");
        // 旧 jsonl 的 \uXXXX 字面串：必须解码
        assert_eq!(decode_unicode_escapes(r"\u4f18\u5148"), "优先");
        // 混合
        assert_eq!(decode_unicode_escapes(r"abc\u4f18"), "abc优");
        // 反斜杠非 \u：原样
        assert_eq!(decode_unicode_escapes("a\\b"), "a\\b");
    }
}
