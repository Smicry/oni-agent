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
    pub last_decision: String,
    pub updated_at: i64,
}
impl Default for SlowView {
    fn default() -> Self {
        Self {
            mode: "mock".into(), cycle: 0, paused: true,
            food_kcal: 0.0, oxygen_kg: 0.0, breathable_cells: 0,
            dupes: vec![], escalations: vec![], commands: vec![],
            recent_decisions: vec![], last_decision: String::new(), updated_at: 0,
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
            commands: vec!["window 90".into()] }
    } else if has_escalation {
        let c = if prompt.contains("食物") && prompt.contains("switch_goal") { "switch_goal" } else { "dig_access" };
        LlmDecision { choice: c.into(), action: "resolve".into(),
            reason: "mock: 选合理候选，验证闭环".into(), commands: vec![] }
    } else {
        LlmDecision { choice: "none".into(), action: "wait".into(),
            reason: "mock: 无待处理事项".into(), commands: vec![] }
    }
}

const SYSTEM_PROMPT: &str = "你是缺氧殖民地慢系统决策者。快系统每10秒采集局面，遇卡点上报结构化诊断(含候选)。从候选中选一个，或对用户指令决策。原则:1)先死因优先(食物/氧气续航短) 2)可达性问题让快系统自救 3)改目标说明依据 4)用户指令优先(世界已暂停)。输出严格JSON: {choice,action,reason,commands[]}，action∈{resolve,command,wait,custom}。commands只能是可直接执行的oni子命令(如 'build 139,197 Ladder SandStone --apply'、'order 挖 土@(127,197):9')或留空[]，绝不能是自然语言描述。";

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
    p.push_str("\n输出严格JSON(choice/action/reason/commands)。");
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

fn execute_decision(dec: &LlmDecision, escs: &[Value], cmds: &[Value]) -> String {
    let mut log = Vec::new();
    for c in &dec.commands {
        let parts: Vec<&str> = c.split_whitespace().collect();
        if parts.is_empty() { continue; }
        log.push(format!("$ oni {}: {}", c, run_oni(&parts)));
    }
    if dec.action == "resolve" {
        if let Some(e) = escs.first() {
            let id = e.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            let note = format!("慢系统({})选[{}]: {}", dec.choice, dec.choice, dec.reason);
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
    view.mode = if cfg.is_mock() { "mock".into() } else { cfg.model.clone() };
    view.updated_at = now();
    view
}

fn decode_unicode_escapes(s: &str) -> String {
    // commands.jsonl 里 text 存的是 \uXXXX 字面串，还原成中文
    let mut out = String::new();
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if i + 6 <= b.len() && b[i] == 0x5c && b[i+1] == b'u' {
            let hex = std::str::from_utf8(&b[i+2..i+6]).unwrap_or("");
            if let Ok(cp) = u32::from_str_radix(hex, 16) {
                if let Some(ch) = char::from_u32(cp) { out.push(ch); i += 6; continue; }
            }
        }
        out.push(b[i] as char); i += 1;
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
    let summary = format!("[{}] choice={} action={} reason={} | {}",
        if cfg.is_mock() { "mock" } else { &cfg.model },
        decision.choice, decision.action, decision.reason, log);

    // 写决策留痕
    let sd = save_dir();
    let rec = serde_json::json!({
        "id": now(), "ts": now(),
        "cycle": snap.get("cycle").and_then(|v| v.as_i64()).unwrap_or(0),
        "source": "slow_service", "choice": decision.choice,
        "action": decision.action, "reason": decision.reason,
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

const PAGE: &str = r#"<!DOCTYPE html>
<html lang="zh"><head><meta charset="utf-8"><title>ONI 慢系统控制台</title>
<style>
body{font-family:-apple-system,sans-serif;background:#1a1d24;color:#d8dee9;margin:0;padding:20px;font-size:13px}
h1{font-size:18px;color:#88c0d0}h2{font-size:14px;color:#81a1c1;border-bottom:1px solid #3b4252;padding-bottom:4px}
.card{background:#2e3440;border-radius:6px;padding:12px;margin:10px 0}
.grid{display:grid;grid-template-columns:repeat(auto-fit,minmax(150px,1fr));gap:8px}
.kv{background:#3b4252;padding:8px;border-radius:4px}.kv b{color:#88c0d0}
.dupe{padding:4px 0;border-bottom:1px solid #3b4252}
.esc{background:#3b4252;padding:8px;margin:6px 0;border-radius:4px;border-left:3px solid #ebcb8b}
.cand{background:#4c566a;padding:4px 8px;margin:4px 0;border-radius:3px;cursor:pointer}
.cand:hover{background:#5e81ac}
input,button{font-size:13px;padding:6px 10px;border-radius:4px;border:1px solid #4c566a;background:#3b4252;color:#d8dee9}
button{background:#5e81ac;cursor:pointer}button:hover{background:#81a1c1}
#cmd{width:60%}.small{color:#a3be8c;font-size:11px}
</style></head><body>
<h1>ONI 慢系统控制台 <span id="mode" class="small"></span></h1>
<div class="card"><div class="grid" id="stats"></div></div>
<h2>复制人</h2><div class="card" id="dupes"></div>
<h2>待决策卡点 <span class="small" id="esccount"></span></h2><div id="escs"></div>
<h2>用户指令（输入后世界暂停，慢系统处理后恢复自主）</h2>
<div class="card"><input id="cmd" placeholder="输入指令，如：优先采食物"><button onclick="sendCmd()">下发</button></div>
<div id="cmds"></div>
<h2>最近决策</h2><div class="card" id="decisions"></div>
<script>
function esc(t){return String(t).replace(/</g,'&lt;');}
async function refresh(){var r=await fetch('/api/state');var s=await r.json();
document.getElementById('mode').textContent='['+s.mode+'] 更新 '+new Date(s.updated_at*1000).toLocaleTimeString();
document.getElementById('stats').innerHTML=
 '<div class="kv"><b>周期</b><br>'+s.cycle+'</div>'+
 '<div class="kv"><b>世界</b><br>'+(s.paused?'暂停':'运行')+'</div>'+
 '<div class="kv"><b>食物</b><br>'+Math.round(s.food_kcal)+' kcal</div>'+
 '<div class="kv"><b>氧气</b><br>'+Math.round(s.oxygen_kg)+' kg</div>'+
 '<div class="kv"><b>可呼吸格</b><br>'+s.breathable_cells+'</div>';
document.getElementById('dupes').innerHTML=s.dupes.map(function(d){
 return '<div class="dupe">'+esc(d.name)+' @('+d.x+','+d.y+') '+esc(d.chore)+' 膀胱'+Math.round(d.bladder)+'</div>';}).join('');
document.getElementById('esccount').textContent=s.escalations.length+' 条';
document.getElementById('escs').innerHTML=s.escalations.map(function(e){
 var c=e.context||{};var cands=(c.candidates||[]).map(function(x){
  return '<div class="cand" onclick="decide('+e.id+',''+x.id+'')">候选['+x.id+'] '+esc(x.action)+': '+esc(x.note||'')+'</div>';}).join('');
 return '<div class="esc">#'+e.id+' '+esc(e.detail)+'<div class="small">卡点='+esc(c.obstacle||'-')+'</div>'+cands+'</div>';}).join('');
document.getElementById('cmds').innerHTML=s.commands.map(function(c){
 return '<div class="small">#'+c.id+' '+esc(c.text)+'</div>';}).join('');
document.getElementById('decisions').innerHTML='<div class="small">'+esc(s.last_decision)+'</div>'+
 s.recent_decisions.slice(0,5).map(function(d){return '<div class="small">['+esc(d.source||'慢系统')+'] '+esc(d.action||'')+': '+esc(d.reason||'')+'</div>';}).join('');
}
async function sendCmd(){var t=document.getElementById('cmd').value;if(!t)return;
 await fetch('/api/command',{method:'POST',body:t});document.getElementById('cmd').value='';refresh();}
async function decide(id,choice){await fetch('/api/decide',{method:'POST',body:JSON.stringify({id:id,choice:choice})});refresh();}
refresh();setInterval(refresh,2000);
</script></body></html>"#;

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
            ("GET", "/") | ("GET", "/index.html") =>
                json_response(&mut stream, "200 OK", PAGE, "text/html"),
            ("GET", "/api/state") => {
                let v = view.lock().map(|g| serde_json::to_string(&*g).unwrap_or_default()).unwrap_or_default();
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
