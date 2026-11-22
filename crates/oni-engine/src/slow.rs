//! **慢系统服务**（slow system）—— 常驻决策者，通过文件总线与快系统协同。
//!
//! 快系统（oni agent）每10s采快照、发现卡点，写结构化 escalation + live_snapshot。
//! 慢系统（本模块）常驻轮询：escalation → 调 LLM 从候选里选 → 执行 → 留痕。
//! 用户指令（oni command add 已实现入队+暂停）：LLM 决策 → 执行 → 回执 → 恢复自主。
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
    pub updated_at: i64,
}
impl Default for SlowView {
    fn default() -> Self {
        Self {
            mode: "mock".into(), cycle: 0, paused: true,
            food_kcal: 0.0, oxygen_kg: 0.0, breathable_cells: 0,
            dupes: vec![], escalations: vec![], commands: vec![],
            recent_decisions: vec![], conversation: vec![], last_decision: String::new(), updated_at: 0,
        }
    }
}

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn call_llm(cfg: &SlowConfig, system: &str, prompt: &str) -> Result<String, String> {
    let endpoint = cfg.endpoint.as_ref().ok_or("no endpoint")?;
    let key = cfg.api_key.as_ref().ok_or("no api key")?;

    let (url, body) = if cfg.is_anthropic() {
        // Anthropic Messages API: {baseURL}/messages
        let url = format!("{}/messages", endpoint.trim_end_matches('/'));
        let body = serde_json::json!({
            "model": cfg.model,
            "max_tokens": 4096,
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
            "max_tokens": 4096,
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

fn mock_decision(prompt: &str, has_escalation: bool, has_command: bool) -> LlmDecision {
    if has_command {
        LlmDecision { choice: "custom".into(), action: "command".into(),
            reason: "mock: 用户指令优先，已记录并恢复自主".into(),
            priority: "P3".into(), goal: String::new(), commands: vec!["window 90".into()] }
    } else if has_escalation {
        let c = if prompt.contains("食物") && prompt.contains("switch_goal") { "switch_goal" } else { "dig_access" };
        LlmDecision { choice: c.into(), action: "resolve".into(),
            reason: "mock: 选合理候选，验证闭环".into(), priority: "P1".into(), goal: String::new(), commands: vec![] }
    } else {
        LlmDecision { choice: "none".into(), action: "wait".into(),
            reason: "mock: 无待处理事项".into(), priority: "P0".into(), goal: String::new(), commands: vec![] }
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
- commands: 可直接执行的oni子命令(如 'build 139,197 Ladder SandStone --apply')或留空[]；优先用 goal，少用 commands。";

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
    let mut all: Vec<&str> = text.lines().collect();
    all.reverse();
    for line in &all {
        let Ok(o) = serde_json::from_str::<Value>(line) else { continue };
        let goal = o.get("goal").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if goal.is_empty() { continue; }
        let ok = o.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
        let e = stats.entry(goal.clone()).or_insert((0,0));
        if ok { e.1 += 1 } else { e.0 += 1 }
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
    let mut lessons: Vec<String> = stats.iter().filter(|(_, (f, _))| *f >= 2)
        .map(|(g, (f, sc))| format!("  - goal={} 失败{}次/成功{}次 → 换策略或解释为何坚持", g, f, sc)).collect();
    lessons.sort();
    if !lessons.is_empty() {
        out.push_str("\n持久教训(整局统计):\n");
        for l in lessons.iter().take(5) { out.push_str(l); out.push('\n'); }
    }
    out.push_str("\n历史仅供参考,非当前指令。\n");
    out
}
fn build_prompt(escs: &[Value], cmds: &[Value], snap: &Value) -> String {
    let mut p = String::from("== 当前局面 ==\n");
    let diag = snap.get("diagnostics").unwrap_or(&Value::Null);
    p.push_str(&format!("周期={} 暂停={} 食物={}kcal 氧气={}kg\n",
        snap.get("cycle").unwrap_or(&Value::Null),
        snap.get("paused").unwrap_or(&Value::Null),
        diag.get("foodKcal").unwrap_or(&Value::Null),
        diag.get("oxygenKgVisible").unwrap_or(&Value::Null)));
    if let Some(dupes) = snap.get("dupes").and_then(|d| d.as_array()) {
        p.push_str("复制人:\n");
        for d in dupes {
            p.push_str(&format!("  {} @({},{}) {} 膀胱={}\n",
                d.get("name").unwrap_or(&Value::Null),
                d.get("x").unwrap_or(&Value::Null),
                d.get("y").unwrap_or(&Value::Null),
                d.get("chore").unwrap_or(&Value::Null),
                d.get("bladder").unwrap_or(&Value::Null)));
        }
    }
    if !cmds.is_empty() {
        p.push_str("\n== 用户指令(优先,世界已暂停) ==\n");
        for c in cmds {
            p.push_str(&format!("  #{} {}\n",
                c.get("id").unwrap_or(&Value::Null),
                c.get("text").unwrap_or(&Value::Null)));
        }
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
    let sd = save_dir();
    let ts = now();
    let cmd = serde_json::json!({
        "id": ts, "text": goal, "source": "ai_goal",
        "ts": ts, "done": false, "result": reason,
    });
    use std::io::Write;
    if let Some(d) = sd.parent() { let _ = std::fs::create_dir_all(d); }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true)
        .open(sd.join("commands.jsonl")) {
        let _ = writeln!(f, "{}", cmd);
        return true;
    }
    false
}

fn execute_decision(dec: &LlmDecision, escs: &[Value], cmds: &[Value]) -> String {
    let mut log = Vec::new();
    // B方案：语义目标写进 commands.jsonl，快系统用 autoact 翻译成工单执行。
    if push_ai_goal(&dec.goal, &dec.reason) {
        log.push(format!("ai_goal[{}]:已入队(快系统执行)", dec.goal));
    }
    for c in &dec.commands {
        let parts: Vec<&str> = c.split_whitespace().collect();
        if parts.is_empty() { continue; }
        log.push(format!("$ oni {}: {}", c, run_oni(&parts)));
    }
    // 只要有 escalation 就 resolve —— 慢系统看过即解冻，不管选了什么动作。
    // 尤其 wait：若不 resolve，escalation_hold 不解除，世界会永久冻结（残留 bug）。
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

/// 组装对话流：用户指令 + 慢系统决策，按时间排序（最近30条）。
/// 每条用户指令带完成状态（done? 成功/失败/待处理），慢系统决策带 goal 和 reason。
fn build_conversation() -> Vec<Value> {
    let sd = save_dir();
    let mut msgs: Vec<Value> = Vec::new();
    // 用户指令（source=user）
    if let Ok(text) = std::fs::read_to_string(sd.join("commands.jsonl")) {
        for line in text.lines() {
            let Ok(c) = serde_json::from_str::<Value>(line) else { continue };
            let source = c.get("source").and_then(|v| v.as_str()).unwrap_or("");
            if source != "user" { continue; } // ai_goal 不进对话流，避免噪音
            let text = c.get("text").and_then(|v| v.as_str()).unwrap_or("");
            let decoded = decode_unicode_escapes(text);
            let done = c.get("done").and_then(|v| v.as_bool()).unwrap_or(false);
            let result = c.get("result").and_then(|v| v.as_str()).unwrap_or("");
            let status = if !done { "pending" }
                else if result.contains("失败") || result.contains("未成功") { "fail" }
                else { "success" };
            msgs.push(serde_json::json!({
                "role": "user", "text": decoded, "status": status,
                "result": result, "ts": c.get("ts").and_then(|v| v.as_i64()).unwrap_or(0),
            }));
        }
    }
    // 慢系统决策
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
                "role": "assistant", "text": text, "status": "",
                "result": action, "ts": d.get("ts").and_then(|v| v.as_i64()).unwrap_or(0),
            }));
        }
    }
    msgs.sort_by_key(|m| m.get("ts").and_then(|v| v.as_i64()).unwrap_or(0));
    // 配额截断：保留最近15条 user + 最近15条 assistant，保证对话里用户指令不被淹没
    let mut users: Vec<Value> = msgs.iter().filter(|m| m.get("role").and_then(|v| v.as_str()) == Some("user")).cloned().collect();
    let mut ais: Vec<Value> = msgs.iter().filter(|m| m.get("role").and_then(|v| v.as_str()) != Some("user")).cloned().collect();
    if users.len() > 15 { users = users[users.len()-15..].to_vec(); }
    if ais.len() > 15 { ais = ais[ais.len()-15..].to_vec(); }
    let mut out = users; out.extend(ais);
    out.sort_by_key(|m| m.get("ts").and_then(|v| v.as_i64()).unwrap_or(0));
    out
}

fn collect_state(cfg: &SlowConfig) -> SlowView {
    let mut view = SlowView::default();
    let sd = save_dir();
    let snap = load_json(sd.join("live_snapshot.json"), Value::Null);
    view.cycle = snap.get("cycle").and_then(|v| v.as_i64()).unwrap_or(0);
    view.paused = snap.get("paused").and_then(|v| v.as_bool()).unwrap_or(true);
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
    view.updated_at = now();
    view
}

fn decode_unicode_escapes(s: &str) -> String {
    // 兼容两种输入：
    //   1) Web 输入框提交的**真实 UTF-8**（优先）—— 原样返回，绝不能逐字节拆
    //   2) 旧 commands.jsonl 里的 \uXXXX **字面串** —— 只解码这种
    // 旧实现逐字节 b[i] as char 把 UTF-8 中文拆成 Latin-1（æµè¯...），已修。
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

/// 跑一轮慢系统决策。返回决策摘要。
pub fn run_once(cfg: &SlowConfig, view: &Arc<Mutex<SlowView>>) -> String {
    let (escs, cmds, snap) = {
        let sd = save_dir();
        let e = load_json(sd.join("escalations/pending.json"), serde_json::json!([]));
        let mut c = Vec::new();
        if let Ok(text) = std::fs::read_to_string(sd.join("commands.jsonl")) {
            for line in text.lines() {
                if let Ok(d) = serde_json::from_str::<Value>(line) {
                    if !d.get("done").and_then(|v| v.as_bool()).unwrap_or(false) { c.push(d); }
                }
            }
        }
        let s = load_json(sd.join("live_snapshot.json"), Value::Null);
        (e.as_array().cloned().unwrap_or_default(), c, s)
    };

    if escs.is_empty() && cmds.is_empty() {
        return "idle".into();
    }

    let prompt = build_prompt(&escs, &cmds, &snap);
    let decision = if cfg.is_mock() {
        mock_decision(&prompt, !escs.is_empty(), !cmds.is_empty())
    } else {
        match call_llm(cfg, SYSTEM_PROMPT, &prompt).and_then(|t| parse_decision(&t)) {
            Ok(d) => d,
            Err(e) => {
                let d = mock_decision(&prompt, !escs.is_empty(), !cmds.is_empty());
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
    let listener = match TcpListener::bind(&addr) {
        Ok(l) => l,
        Err(e) => { eprintln!("[slow] web bind {} failed: {}", addr, e); return; }
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
                // 旧实现在 LLM 推理(可能几十秒)期间 view 不刷新，页面看起来卡住。
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
                let text = decode_unicode_escapes(body.trim());
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
