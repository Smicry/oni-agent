//! oni —— 可持续游戏智能体的命令行入口。
//!
//! 子命令：
//!   observe    采全局面，输出全局态势卡 + 账本 + 趋势
//!   doctor     连通性/目录/协议自检
//!   preflight  下单前预检（挖掘工位 / 建造占格+支撑+材料）
//!   plan       按依赖图选本轮唯一目标
//!   act        执行本轮目标
//!   watch      常驻观察：危急自动暂停、条件解除自动恢复

use std::process::ExitCode;
use std::time::{Duration, Instant};

use oni_engine::decide::{decide, load_plan};
use oni_engine::exec::execute;
use oni_engine::judge::judge;
use oni_engine::ledger::ledger;
use oni_engine::sense::sense;
use oni_engine::trend::{compute, load_samples, record};
use oni_mcp::{Catalog, McpSession};
use oni_world::preflight::{preflight_build, preflight_dig};
use oni_world::rules::parse_points;

const ENDPOINT: &str = "http://127.0.0.1:8788/mcp/";
const PLAN: &str = "plan.json";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(|s| s.as_str()).unwrap_or("observe");
    let rest: Vec<String> = args.iter().skip(1).cloned().collect();
    let r = match cmd {
        "observe" => cmd_observe(&rest),
        "doctor" => cmd_doctor(),
        "preflight" => cmd_preflight(&rest),
        "plan-check" => cmd_plan_check(&rest),
        "plan-llm" => cmd_plan_llm(),
        "cells" => cmd_reach_cells(&rest),
        "plan" => cmd_plan(),
        "plan-reset" => cmd_plan_reset(),
        "escalations" => cmd_escalations(&rest),
        "resolve" => cmd_resolve(&rest),
        "status" => cmd_status(),
        "audit" => cmd_audit(),
        "bus" => cmd_bus(&rest),
        "brief" => cmd_brief(),
        "command" => cmd_command(&rest),
        "decision" => cmd_decision(&rest),
        "scout" => cmd_scout(),
        "state" => cmd_state(),
        "note" => cmd_note(&rest),
        "policy" => cmd_policy(&rest),
        "promote" => cmd_promote(&rest),
        "restart" => cmd_restart(&rest),
        "act" => cmd_act(),
        "watch" => cmd_watch(&rest),
        "map" => cmd_map(&rest),
        "area" => cmd_area(&rest),
        "probe" => cmd_probe(&rest),
        "reach" => cmd_reach(),
        "graph" => cmd_graph(&rest),
        "order" => cmd_order(&rest),
        "build" => cmd_build(&rest),
        "raw" => cmd_raw(&rest),
        "call" => cmd_call(&rest),
        "agent" => cmd_agent(&rest),
        "slow" => cmd_slow(&rest),
        "serve" => cmd_serve(),
        "window" => cmd_window(&rest),
        "hold" => cmd_hold(&rest),
        "release" => cmd_release(&rest),
        "pause" => cmd_pause_resume(&rest, true),
        "resume" => cmd_pause_resume(&rest, false),
        "help" | "--help" | "-h" => {
            println!("{}", HELP);
            Ok(())
        }
        other => Err(anyhow::anyhow!("未知子命令: {other}"))
    };
    match r {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("错误: {e:#}");
            ExitCode::FAILURE
        }
    }
}

const HELP: &str = r#"oni <子命令>
  serve              ★ 一键启动：快系统 + 慢系统 + Web 控制台(:8789)（主力入口）
  observe [--cached]  采全局面（多源并行），输出全局态势卡 + 账本 + 趋势
                    （--cached = 零 MCP 读守护进程快照）
  doctor             连通性 / 建造目录 / 协议自检
  cells <x1,y1,x2,y2>            世界模型逐格诊断（可站/支撑/可达集 vs 实时元素）
  reach              打印世界模型可达集（与游戏真值不一致时以游戏为准）
  scout              开局资源侦察：关键资源位置+距离，落盘 scout.json
  plan-check                    依赖图体检：节点 status / 判据 / 假 done 清单
  plan-llm                      手动触发 LLM 开局规划（采事实→生成→校验→落盘）
  plan-reset                    重开规划：清旧 plan + 局内状态 → 同步生成新开局战略
  restart [存档索引]             回滚存档 + 重置 plan + 清局内状态 + 开施工窗口
  preflight dig  <x1,y1,x2,y2>   挖掘预检：目标可挖? 有落脚点?
  preflight build <x,y> <Prefab>    建造预检：占格空? 支撑实心? 材料合法?
  plan               按依赖图选本轮唯一目标
  act                执行本轮目标（预检 → dry-run → apply）
  watch [--interval N]  常驻观察：危急自动暂停，解除自动恢复
  map <x1,y1,x2,y2>     打印区域文本地图（规划用）
  pause|resume [原因]   暂停 / 恢复游戏（与判断配对使用）
  probe <x1,y1,x2,y2>    逐格探针：元素/实心/质量/建筑
  graph [--full]        气室连通图 + 空气预算 + 到地表最短挖掘路径（--full 读全图）
  order <命令>          原样执行一条运维命令并打印完整返回
  build <x,y> <Prefab> [材料] [--apply]   建造落点：地图 token 补丁（默认 dry-run，--apply 落地）
  raw <oni://URI>        打印任意 MCP 资源的原始 JSON（审计用）
  call <tool> <json>   直接调用任意 MCP 工具（绕过语义封装，调试用）
  agent [--interval N]  常驻循环守护进程（后台跑，不依赖主 agent 的回合速度）
  slow [--port=N] [--interval=N]   只起慢系统（LLM 决策 + Web 控制台）
  window <秒>          请求开一个施工窗口（守护进程拥有，到点自动冻回暂停）
  hold [原因]          冻结世界（主 agent 改代码期间用）：暂停 + 代理只观察不自动恢复
  release               解除 hold，让代理恢复自主循环
  escalations [ack N|clear|history]   快系统喊上来的升级请求（慢系统工作台）
  resolve <id> [备注]    统一收口：同时确认 escalation + bus 两个通道
  status                 慢系统一屏态势：暂停态/hold + 升级/指令/总线（零 MCP）
  audit                  慢系统主动巡检：体检当前局面，发现写入 bus + 红线告警
  area <y0> <y1>         一次读图层摘要（水体/气室/建筑/植物/危险点）
  bus [timeline|ack N|resolve N]  协同总线：快慢系统的统一事件时间线（QoS 过滤/可回放）
  command [add <指令>|list|done <id>]  用户指令队列（下发指令给慢系统）
  decision add <json>|list [n]|suggestions|resolve <id> <实际结果>
                                  慢系统决策记录（依据/预期/回填）
  state                 存档身份 / 指纹 / 局内缓存目录（跨局隔离检查）
  note <文本>            写进本存档的 notes.md（局内笔记，不进通用记忆）
  promote <kind> <文本>  上提到通用记忆：rule→AGENTS.md knowledge→knowledge-base.md issue→optimization-backlog"#;

fn connect() -> anyhow::Result<McpSession> {
    Ok(McpSession::connect(ENDPOINT).map_err(|e| anyhow::anyhow!("连接 {ENDPOINT} 失败: {e}"))?)
}

fn cmd_observe(args: &[String]) -> anyhow::Result<()> {
    // --cached：读守护进程落盘的共享快照，**不碰 MCP**。
    //   用途：主 agent "看局势"时零 MCP 压力；两边看到的也是同一份真值。
    //   需要实时数据（要下指令前）时用默认的实时读。
    if args.iter().any(|a| a == "--cached") {
        if let Some(snap) = oni_engine::live::load(120) {
            let samples = load_samples(200);
            let trend = compute(&samples, &snap);
            let j = judge(&snap, Some(&trend));
            let l = ledger(&snap, &trend);
            print_card(&snap, &j, &l, &trend);
            return Ok(());
        }
        println!("（共享快照不可用或已过期，回退到实时读取）");
    }
    let s = connect()?;
    let snap = sense(&s);
    let _ = record(&snap);
    let samples = load_samples(200);
    let trend = compute(&samples, &snap);
    let j = judge(&snap, Some(&trend));
    let l = ledger(&snap, &trend);
    print_card(&snap, &j, &l, &trend);
    Ok(())
}

fn cmd_doctor() -> anyhow::Result<()> {
    let t0 = Instant::now();
    let s = connect()?;
    println!("✓ MCP 握手成功（{} ms）", t0.elapsed().as_millis());
    let cat = Catalog::load(&s).map_err(|e| anyhow::anyhow!("{e}"))?;
    println!("✓ 建造目录: {} 项（含 includeUnavailable 全量）", cat.buildings.len());
    for key in ["Electrolyzer", "AlgaeHabitat", "FarmStation", "ManualGenerator", "Battery", "Wire", "Tile", "Ladder"] {
        if let Some(d) = cat.get(key) {
            println!("   · {:<18} {}  {}x{}", d.prefab_id, d.name, d.width, d.height);
        } else {
            println!("   · {:<18} ✖ 不在目录里", key);
        }
    }
    let snap = sense(&s);
    println!("✓ 全局快照 {} ms，{} 个观察源，{} 条错误", snap.latency_ms, oni_engine::sense::SOURCES.len(), snap.errors.len());
    for e in &snap.errors {
        println!("   ✖ {e}");
    }
    Ok(())
}

/// **用模板重置 plan**（换存档时用）：把 plan.json 恢复成 plan.template.json 的开局状态。
///
/// 为什么需要：换存档后 plan.json 里上一局的 done 状态会被继承，
///   代理以为建材/产氧已经做过 → 跳过关键步骤 → 开局就卡住。

// ---- 存档相关命令------------------------

/// 识别当前存档并设为进程内"当前存档"。
///
/// 指纹来自**存档内不变**的事实：存档名 + 世界名 + 世界 id + 打印舱锚点。
/// 
fn current_save() -> Option<oni_mcp::save::SaveId> {
    let session = connect().ok()?;
    let id = oni_mcp::save::identify(&session)?;
    oni_mcp::save::set_current(id.clone());
    oni_mcp::save::remember(&id);
    Some(id)
}

fn ts() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M").to_string()
}

/// 存档状态：当前是谁、局内缓存在哪、有哪些存档见过。
fn cmd_state() -> anyhow::Result<()> {
    println!("== 存档状态 ==");
    match current_save() {
        Some(id) => {
            println!("  存档: {}  世界: {} (id {})", id.name, id.world, id.world_id);
            println!("  指纹: {}", id.fingerprint);
            println!("  打印舱锚点: {}", id.anchor);
            println!("  局内状态目录: {}", oni_mcp::save::save_dir(&id.fingerprint).display());
        }
        None => println!("  （识别失败：游戏没加载？）"),
    }
    println!();
    println!("== 已知存档（局内缓存，互不污染） ==");
    let saves = oni_mcp::save::known_saves();
    if saves.is_empty() {
        println!("  （还没有按存档隔离的缓存）");
    }
    for (fp, at) in saves {
        println!("  {}  首次见到 {}", fp, at);
    }
    println!();
    println!("== 计划文件 ==");
    match oni_engine::saveswitch::plan_fingerprint(PLAN) {
        Some(f) => println!("  plan.json 指纹: {f}"),
        None => println!("  plan.json 指纹: （无 —— 未标记出身，按可疑处理）"),
    }
    Ok(())
}

/// ★ 选址策略：把"什么算安全落点"从硬编码变成**慢系统可调参数**。
///
/// 为什么需要：开局氧气稀薄时"厕所必须纯氧"的硬编码判据会把所有落点否掉 →
///   反复挖同一格 → 厕所永远建不成（实测）。这类权衡属于慢系统的活，
///   但阈值必须能落到代码可执行的参数上，而不是每次改代码。
///
/// 用法：
///   oni policy show                打印当前策略
///   oni policy set co2_max_kg=3.0   调整策略（立即生效 + 落盘）
///   oni policy reset               恢复默认
fn cmd_policy(args: &[String]) -> anyhow::Result<()> {
    use oni_world::preflight::{site_policy, SitePolicy};
    let sub = args.first().map(|s| s.as_str()).unwrap_or("show");
    let id = current_save().ok_or_else(|| anyhow::anyhow!("识别不到当前存档"))?;
    let dir = oni_mcp::save::save_dir(&id.fingerprint);
    let path = dir.join("site_policy.json");
    match sub {
        "show" => {
            // 落盘优先（跨进程可见），否则用进程内当前值。
            let p: SitePolicy = std::fs::read_to_string(&path).ok()
                .and_then(|t| serde_json::from_str(&t).ok())
                .unwrap_or_else(site_policy);
            println!("== 选址策略（慢系统可调） ==");
            println!("  co2_max_kg        = {}   (选址允许的 CO₂ 浓度上限 kg/格)", p.co2_max_kg);
            println!("  strict_prefabs    = {:?} (这些建筑仍要求纯氧)", p.strict_prefabs);
            println!("  relax_after_fail  = {}   (连续失败几次后自动放宽)", p.relax_after_fail);
            Ok(())
        }
        "set" => {
            let text = args.iter().skip(1).cloned().collect::<Vec<_>>().join(" ");
            if text.trim().is_empty() {
                anyhow::bail!("用法: oni policy set co2_max_kg=3.0（或完整 JSON）");
            }
            // 支持两种写法：完整 JSON，或 key=value 简写（慢系统更不容易写错）。
            let v: serde_json::Value = match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(_) => {
                    // key=value / k=v,k=v 解析
                    let mut map = serde_json::Map::new();
                    for pair in text.split(',') {
                        let pair = pair.trim();
                        if pair.is_empty() { continue; }
                        let Some((k, val)) = pair.split_once('=') else {
                            anyhow::bail!("无法解析: {pair}（用 JSON 或 key=value）");
                        };
                        let k = k.trim();
                        let val = val.trim();
                        let parsed: serde_json::Value = match val {
                            "true" => serde_json::Value::Bool(true),
                            "false" => serde_json::Value::Bool(false),
                            _ => match val.parse::<f64>() {
                                Ok(n) => serde_json::json!(n),
                                Err(_) => serde_json::Value::String(val.to_string()),
                            },
                        };
                        map.insert(k.to_string(), parsed);
                    }
                    serde_json::Value::Object(map)
                }
            };
            let mut p = site_policy();
            if let Some(x) = v.get("co2_max_kg").and_then(|x| x.as_f64()) { p.co2_max_kg = x; }
            if let Some(x) = v.get("relax_after_fail").and_then(|x| x.as_u64()) {
                p.relax_after_fail = x as usize;
            }
            if let Some(x) = v.get("strict_prefabs").and_then(|x| x.as_array()) {
                p.strict_prefabs = x.iter().filter_map(|y| y.as_str().map(|s| s.to_string())).collect();
            }
            std::fs::create_dir_all(&dir)?;
            std::fs::write(&path, serde_json::to_string_pretty(&p)?)?;
            oni_world::preflight::set_site_policy(p.clone());
            println!("✅ 选址策略已更新并落盘: co2_max_kg={} strict={:?} relax_after_fail={}",
                p.co2_max_kg, p.strict_prefabs, p.relax_after_fail);
            Ok(())
        }
        "reset" => {
            let _ = std::fs::remove_file(&path);
            oni_world::preflight::set_site_policy(SitePolicy::default());
            println!("✅ 选址策略已恢复默认");
            Ok(())
        }
        other => anyhow::bail!("用法: oni policy [show|set <json>|reset]（未知子命令 {other}）"),
    }
}

/// 局内笔记：写进**本存档**的 notes.md（只对有意义，不进通用记忆）。
fn cmd_note(args: &[String]) -> anyhow::Result<()> {
    let text = args.join(" ");
    if text.trim().is_empty() {
        anyhow::bail!("用法: oni note <文本>");
    }
    let id = current_save().ok_or_else(|| anyhow::anyhow!("识别不到当前存档"))?;
    let dir = oni_mcp::save::save_dir(&id.fingerprint);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("notes.md");
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    use std::io::Write;
    writeln!(f, "- [{}] {}", ts(), text)?;
    println!("✅ 已记入本存档笔记: {}", path.display());
    Ok(())
}

/// 记忆上提：把**可复用**的结论写进仓库的通用记忆文件。
///
///   rule      → AGENTS.md                       硬规则
///   knowledge → docs/knowledge-base.md           机制知识（存档无关的事实）
///   issue     → records/optimization-backlog.md 待优化清单（还没落地的改进）
///
/// 分层纪律：局内发现先落在 notes.md / session-*.md；
///   **确认换个存档也成立**，再上提到这里。
fn cmd_promote(args: &[String]) -> anyhow::Result<()> {
    let kind = args.first().map(|s| s.as_str()).unwrap_or("");
    let text = args.iter().skip(1).cloned().collect::<Vec<_>>().join(" ");
    if text.trim().is_empty() {
        anyhow::bail!("用法: oni promote <rule|knowledge|issue> <文本>");
    }
    let file = match kind {
        "rule" => "AGENTS.md",
        "knowledge" => "docs/knowledge-base.md",
        "issue" => "records/optimization-backlog.md",
        _ => anyhow::bail!("用法: oni promote <rule|knowledge|issue> <文本>"),
    };
    let src = current_save()
        .map(|i| format!("{} / {}", i.name, i.fingerprint))
        .unwrap_or_else(|| "未知存档".to_string());
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(file)?;
    use std::io::Write;
    writeln!(f, "\n<!-- 上提自 {} @ {} -->", src, ts())?;
    writeln!(f, "- {}", text)?;
    println!("✅ 已上提到 {}（来源: {}）", file, src);
    Ok(())
}

/// **慢系统工作台**：读快系统（代理）喊上来的升级请求。
///
/// 快慢分工的交接点：
///   快系统遇到规则解不开的情况 → 写 escalation + 停手；
///   慢系统（AI / 主 agent）用这个命令读出来 → 诊断 → 改计划 / 改代码 / 改策略
/// P0-48 统一收口：同时确认 escalations + bus 两个通道的同一事件。
///   避免慢系统处理后只清一处、另一处残留过时升级误导后续判断。
fn cmd_resolve(args: &[String]) -> anyhow::Result<()> {
    let _ = current_save();
    let Some(id) = args.first().and_then(|x| x.parse::<u64>().ok()) else {
        anyhow::bail!("用法: oni resolve <id> [备注]");
    };
    let note = args.iter().skip(1).cloned().collect::<Vec<_>>().join(" ");
    let note = if note.is_empty() { "慢系统已统一收口".to_string() } else { note };
    let esc_ok = oni_engine::escalate::ack(id);
    let bus_ok = oni_engine::bus::resolve(id, &note);
    println!("✅ resolve #{}: escalations={} bus={}", id,
        if esc_ok { "已确认" } else { "无此条" },
        if bus_ok { "已解决" } else { "无此条" });
    Ok(())
}

/// 慢系统主动中枢：一屏聚合 audit + bus + commands + escalations + 暂停态（零 MCP 读缓存）。
fn cmd_status() -> anyhow::Result<()> {
    let _ = current_save();
    let paused = oni_engine::live::load(120).map(|s| s.paused).unwrap_or(false);
    println!("== 慢系统态势（零 MCP 缓存）==");
    println!("  世界: {}", if paused { "⏸ 暂停" } else { "▶ 运行" });
    println!("  hold: {}", if oni_engine::agent::hold_active() { "🔒 激活" } else { "无" });
    println!("\n-- 升级请求 --\n{}", oni_engine::escalate::summary());
    println!("\n-- 用户指令 --\n{}", oni_engine::commands::summary());
    println!("\n-- 协同总线 --\n{}", oni_engine::bus::summary());
    Ok(())
}

///   → `oni escalations ack` 告诉快系统"我处理了，继续"。
fn cmd_escalations(args: &[String]) -> anyhow::Result<()> {
    // 必须先识别当前存档：save::current() 是**进程内**状态，
    //   不先识别就读不到正确的存档目录（会读到 saves/unknown）。
    let _ = current_save();
    let sub = args.first().map(|s| s.as_str()).unwrap_or("");
    match sub {
        "ack" => {
            let Some(id) = args.get(1).and_then(|x| x.parse::<u64>().ok()) else {
                anyhow::bail!("用法: oni escalations ack <id>");
            };
            if oni_engine::escalate::ack(id) {
                println!("✅ 已确认处理 escalation #{}（快系统可以继续了）", id);
            } else {
                println!("⚠️ 找不到 escalation #{}（可能已被处理）", id);
            }
        }
        "clear" => {
            let esc_n = oni_engine::escalate::read_pending().len();
            oni_engine::escalate::clear();
            // P0-48：escalations 与 bus 是同一问题的两个通道，必须一起清，
            //   否则过时事件残留总线误导后续判断。
            let inbox = oni_engine::bus::inbox();
            let mut bus_n = 0;
            for ev in &inbox {
                if oni_engine::bus::resolve(ev.id, "escalations clear 统一收口") {
                    bus_n += 1;
                }
            }
            println!("✅ 已清空升级请求 {} 条 + 总线事件 {} 条（双通道同步）", esc_n, bus_n);
        }
        "history" => {
            let p = oni_mcp::save::save_dir(
                &oni_mcp::save::current()
                    .map(|s| s.fingerprint)
                    .unwrap_or_else(|| "unknown".into()),
            )
            .join("escalations")
            .join("history.jsonl");
            let text = std::fs::read_to_string(&p).unwrap_or_default();
            let lines: Vec<&str> = text.lines().rev().take(20).collect();
            if lines.is_empty() {
                println!("（暂无历史）");
            }
            for l in lines {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(l) {
                    println!(
                        "[{}] {} {} —— {}",
                        v.get("ts").and_then(|x| x.as_i64()).unwrap_or(0),
                        v.get("kind").and_then(|x| x.as_str()).unwrap_or("?"),
                        v.get("node").and_then(|x| x.as_str()).unwrap_or(""),
                        v.get("detail").and_then(|x| x.as_str()).unwrap_or("")
                    );
                }
            }
        }
        _ => {
            let list = oni_engine::escalate::read_pending();
            if list.is_empty() {
                println!("✅ 没有待处理的升级请求（快系统在自主运行）");
                return Ok(());
            }
            println!("🆘 待慢系统处理的升级请求（{} 条）：", list.len());
            for e in &list {
                println!();
                println!("  #{}  [{}]  节点={}", e.id, e.kind.label(), e.node);
                println!("     情况: {}", e.detail);
                if !e.suggestion.is_empty() {
                    println!("     快系统建议: {}", e.suggestion);
                }
                println!(
                    "     快照: 周期{} 人口{} 氧气{:.0}kg 食物{:.0}kcal",
                    e.cycle, e.dupes, e.oxygen_kg, e.food_kcal
                );
                // 结构化诊断：慢系统据此**选**决策，不手算坐标。
                let c = &e.context;
                if !c.obstacle.is_empty() || !c.candidates.is_empty() {
                    println!("     诊断: 卡点={} 目标={} 坐标={:?} 可达区={:?}",
                        c.obstacle, c.goal, c.target, c.reach_bounds);
                    for cand in &c.candidates {
                        println!("       候选[{}] {}{}：{}", cand.id, cand.action,
                            cand.target.map(|t| format!("@{},{}", t.0, t.1)).unwrap_or_default(),
                            cand.note);
                    }
                    println!("     决策: oni resolve <id> '选候选X：<理由>'（选一个候选后快系统继续）");
                }
            }
            println!();
            println!("处理完用: oni escalations ack <id>");
        }
    }
    Ok(())
}

/// **慢系统主动巡检**：体检当前局面，列出所有"不对劲"的地方。
///
/// 这是慢系统的**主动**发现通道（escalation 是被动喊人）。
///   很多问题是慢慢恶化的：CO₂ 占比在涨、产氧器没投料、电网没负载、
///   梯子链悄悄断了…… 等出事再喊就晚了。
fn cmd_audit() -> anyhow::Result<()> {
    let _ = current_save();
    // 慢系统**零 MCP** 读局势：守护进程（唯一采集者）已经把快照落盘了。
    //   自己再采一次 = 双倍 MCP 成本 + 两份可能不一致的真相。
    //   缓存太旧（守护进程没跑/挂了）才退回实时采集。
    let snap = match oni_engine::live::load(120) {
        Some(s) => s,
        None => {
            let s = connect()?;
            oni_engine::sense::sense(&s)
        }
    };
    let tr = oni_engine::trend::compute(&oni_engine::trend::load_samples(200), &snap);
    let pop = snap.dupes.len();
    let last_pop = std::fs::read_to_string(
        oni_mcp::save::save_file("last_pop"),
    ).ok()
    .and_then(|t| t.trim().parse::<usize>().ok())
    .unwrap_or(pop);
    let report = oni_engine::audit::run(
        &snap,
        snap.cycle,
        last_pop,
        tr.food_cycles_left,
        Some(snap.power_summary()),
    );
    // 慢系统主动能力：发现写入 bus（去重/TTL/可回放），红线时显式提示介入。
    let published = oni_engine::audit::persist(&report);
    if published > 0 {
        println!("📡 已写入协同总线 {} 条巡检发现", published);
    }
    if oni_engine::audit::redline(&report) {
        println!("🚨 红线告警：建议慢系统用 `oni hold` 冻结世界后介入（决策权在你）");
    }
    println!("🩺 巡检报告（周期 {}，人口 {}）", snap.cycle, pop);
    if report.issues.is_empty() {
        println!("   ✅ 未发现问题");
    }
    for i in &report.issues {
        println!(
            "   {}[{}] {}",
            match i.severity {
                oni_engine::audit::Severity::Critical => "🔴",
                oni_engine::audit::Severity::Warn => "🟡",
                oni_engine::audit::Severity::Info => "⚪",
            },
            i.code,
            i.detail
        );
        println!("        → {}", i.suggestion);
    }
    let pending = oni_engine::commands::pending();
    if !pending.is_empty() {
        println!();
        println!("📋 待处理的用户指令：");
        for c in pending {
            println!("   #{} [{}] {}", c.id, c.source, c.text);
        }
    }
    let esc = oni_engine::escalate::read_pending();
    if !esc.is_empty() {
        println!();
        println!("🆘 快系统喊上来的问题：");
        for e in esc {
            println!("   #{} [{}] {} {}", e.id, e.kind.label(), e.node, e.detail);
        }
    }
    Ok(())
}

/// **用户指令队列**：add / list / done。
fn cmd_command(args: &[String]) -> anyhow::Result<()> {
    let _ = current_save();
    let sub = args.first().map(|s| s.as_str()).unwrap_or("list");
    match sub {
        "add" => {
            let text = args.iter().skip(1).cloned().collect::<Vec<_>>().join(" ");
            if text.trim().is_empty() {
                anyhow::bail!("用法: oni command add <指令>");
            }
            let id = oni_engine::commands::push(&text, "user");
            println!("📌 已入队指令 #{}：{}", id, text);
            println!("   （慢系统会读它 → 翻译成计划/工单 → 回执）");
            // 用户下指令 = **立刻冻结世界**：慢系统翻译指令要花时间，
            //   这段时间快系统照旧施工 → 指令还没生效就可能做错事。
            //   （无人倒地时冻结是安全的：ONI 暂停会冻结倒计时。）
            if let Ok(s) = connect() {
                let _ = s.call_text(
                    "game_control",
                    serde_json::json!({
                        "domain": "speed", "action": "pause",
                        "task": "收到用户指令：冻结世界，等慢系统翻译"
                    }),
                );
                println!("   ⏸ 世界已冻结（等慢系统处理指令；快系统下一轮会看到并停手）");
            }
        }
        "list" => {
            let all = oni_engine::commands::read_all();
            if all.is_empty() {
                println!("（队列为空）");
                return Ok(());
            }
            for c in all.iter().rev().take(20) {
                println!(
                    "{} #{} [{}] {}{}",
                    if c.done { "✅" } else { "📌" },
                    c.id,
                    c.source,
                    c.text,
                    if c.result.is_empty() { String::new() } else { format!("  →  {}", c.result) }
                );
            }
        }
        "done" => {
            let Some(id) = args.get(1).and_then(|x| x.parse::<u64>().ok()) else {
                anyhow::bail!("用法: oni command done <id> [回执]");
            };
            let note = args.iter().skip(2).cloned().collect::<Vec<_>>().join(" ");
            if oni_engine::commands::complete(id, &note) {
                println!("✅ 指令 #{} 已处理{}", id, if note.is_empty() { String::new() } else { format!("：{note}") });
            } else {
                println!("⚠️ 找不到指令 #{}", id);
            }
        }
        _ => anyhow::bail!("用法: oni command [add <指令> | list | done <id> [回执]]"),
    }
    Ok(())
}

/// **协同总线**：快慢系统的统一事件时间线。
///
///   oni bus              待处理事件（QoS 过滤：过期剔除 + 去重 + 按优先级排序）
///   oni bus timeline     全部事件（可回放）
///   oni bus ack <id>     确认接手
///   oni bus resolve <id> 确认已解决（正反馈回路）
fn cmd_decision(args: &[String]) -> anyhow::Result<()> {
    let _ = current_save();
    let sub = args.first().map(|s| s.as_str()).unwrap_or("list");
    match sub {
        "add" => {
            let text = args.iter().skip(1).cloned().collect::<Vec<_>>().join(" ");
            let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| anyhow::anyhow!("JSON 无效: {e}"))?;
            let cycle = v.get("cycle").and_then(|x| x.as_i64()).unwrap_or(0);
            let context = v.get("context").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let candidates: Vec<String> = v.get("candidates")
                .and_then(|x| x.as_array())
                .map(|a| a.iter().filter_map(|s| s.as_str().map(|x| x.to_string())).collect())
                .unwrap_or_default();
            let chosen = v.get("chosen").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let rationale = v.get("rationale").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let expected = v.get("expected").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let evidence = match v.get("evidence").and_then(|x| x.as_str()).unwrap_or("snapshot") {
                "official" => oni_engine::decisions::Evidence::Official,
                "inferred" => oni_engine::decisions::Evidence::Inferred,
                "heuristic" => oni_engine::decisions::Evidence::Heuristic,
                _ => oni_engine::decisions::Evidence::Snapshot,
            };
            let id = oni_engine::decisions::record(cycle, &context, &candidates, &chosen, &rationale, evidence, &expected);
            println!("✅ 已记录决策 #{id}: {chosen}");
        }
        "resolve" => {
            let Some(id) = args.get(1).and_then(|x| x.parse::<u64>().ok()) else {
                anyhow::bail!("用法: oni decision resolve <id> <实际结果>");
            };
            let actual = args.iter().skip(2).cloned().collect::<Vec<_>>().join(" ");
            if oni_engine::decisions::resolve(id, &actual) {
                println!("✅ 决策 #{id} 已回填实际结果");
            } else {
                anyhow::bail!("找不到决策 #{id}");
            }
        }
        "list" => {
            let n = args.get(1).and_then(|x| x.parse::<usize>().ok()).unwrap_or(10);
            println!("{}", oni_engine::decisions::summary_for(n));
        }
        "suggestions" => {
            let items = oni_engine::decisions::pending_suggestions();
            if items.is_empty() {
                println!("（暂无机械重规划建议）");
            } else {
                for s in items.iter().rev().take(10) {
                    println!("[{}] {} → {}",
                        s.get("ts").and_then(|x| x.as_i64()).unwrap_or(0),
                        s.get("reason").and_then(|x| x.as_str()).unwrap_or(""),
                        s.get("strategy").and_then(|x| x.as_str()).unwrap_or(""));
                }
            }
        }
        other => anyhow::bail!("用法: oni decision add <json> | list [n] | resolve <id> <实际结果>（未知: {other}）"),
    }
  Ok(())
}
fn cmd_bus(args: &[String]) -> anyhow::Result<()> {
    let _ = current_save();
    let sub = args.first().map(|s| s.as_str()).unwrap_or("");
    match sub {
        "timeline" => {
            let all = oni_engine::bus::timeline();
            if all.is_empty() {
                println!("（总线为空）");
                return Ok(());
            }
            for e in all.iter().rev().take(40) {
                println!(
                    "{} [{}] {} {} — {}{}",
                    match e.state {
                        oni_engine::bus::State::Open => "📌",
                        oni_engine::bus::State::Acknowledged => "👌",
                        oni_engine::bus::State::Resolved => "✅",
                    },
                    e.kind.label(),
                    e.priority.label(),
                    e.node,
                    e.detail,
                    if e.note.is_empty() { String::new() } else { format!("  → {}", e.note) }
                );
            }
        }
        "ack" => {
            let Some(id) = args.get(1).and_then(|x| x.parse::<u64>().ok()) else {
                anyhow::bail!("用法: oni bus ack <id>");
            };
            let note = args.iter().skip(2).cloned().collect::<Vec<_>>().join(" ");
            println!(
                "{}",
                if oni_engine::bus::acknowledge(id, &note) {
                    format!("👌 事件 #{} 已确认接手", id)
                } else {
                    format!("⚠️ 找不到事件 #{}", id)
                }
            );
        }
        "resolve" => {
            let Some(id) = args.get(1).and_then(|x| x.parse::<u64>().ok()) else {
                anyhow::bail!("用法: oni bus resolve <id>");
            };
            let note = args.iter().skip(2).cloned().collect::<Vec<_>>().join(" ");
            println!(
                "{}",
                if oni_engine::bus::resolve(id, &note) {
                    format!("✅ 事件 #{} 已确认解决", id)
                } else {
                    format!("⚠️ 找不到事件 #{}", id)
                }
            );
        }
        _ => {
            println!("📡 协同总线 · 待处理事件");
            println!("{}", oni_engine::bus::summary());
        }
    }
    Ok(())
}

fn cmd_plan_reset() -> anyhow::Result<()> {
    // ★ 新局不再复制模板 playbook（硬编码套路换地图就不适用，已删除）：
    //   落一份「待 LLM 规划」占位，随后**同步**采集本局事实并调 LLM 生成正式计划。
    //   同步 = 消除空窗：reset 返回时 plan 已就绪，不会出现 0 节点死锁期。
    let id = current_save();
    let fp = id.as_ref().map(|i| i.fingerprint.clone());
    if !oni_engine::saveswitch::reset_plan(PLAN, fp.as_deref().unwrap_or("unknown")) {
        anyhow::bail!("写 plan.json 占位失败");
    }
    // 只清**当前存档**的局内状态；别的存档的缓存在各自的目录里，互不影响。
    oni_engine::saveswitch::clear_save_state();
    println!("✅ plan.json 已重置（author=pending_llm），正在同步生成开局战略…");
    if let Some(i) = &fp {
        println!("   存档指纹: {}（已写入 plan.json）", i);
    }

    // LLM 配置与 serve 同源：读 agent.toml（没有则回落到 SlowConfig::default 的
    //   环境变量 / MCode config.yaml 读取）。
    let mut cfg = oni_engine::slow::SlowConfig::default();
    if let Some((_fi, _fw, _si, _port, ep, key, model, fmt)) = read_agent_toml() {
        if !ep.is_empty() { cfg.endpoint = Some(ep); }
        if !key.is_empty() { cfg.api_key = Some(key); }
        if !model.is_empty() { cfg.model = model; }
        if !fmt.is_empty() { cfg.api_format = fmt; }
    }
    if cfg.is_mock() {
        println!("⚠ 未配置 LLM → 跳过同步规划；plan 保持占位，快系统下一轮会采 brief，慢系统异步重试。");
        return Ok(());
    }
    match connect() {
        Ok(s) => {
            let snap = oni_engine::sense::sense(&s);
            let facts = oni_engine::opening::collect_facts(&s, &snap);
            let scout_report = oni_engine::autoact::scout_resources(&s, None);
            let rec = serde_json::json!({
                "facts": serde_json::to_value(&facts).unwrap_or_default(),
                "scout": scout_report,
                "cycle": snap.cycle,
                "population": snap.dupes.len(),
            });
            let brief = serde_json::to_string_pretty(&rec).unwrap_or_default();
            println!("📋 世界事实已采集（{} 字节）→ 同步调用 LLM 生成开局战略…", brief.len());
            match oni_engine::opening::author_opening_plan(&cfg, &brief, PLAN, fp.as_deref()) {
                Ok(msg) => println!("✅ {}", msg),
                Err(reason) => {
                    // 失败不 bail：reset 本身已完成；退回原异步路径（快系统采 brief → 慢系统冷却后重试）。
                    println!("⚠ LLM 开局规划失败：{}", reason);
                    println!("   plan 保持「待 LLM 规划」占位；快系统下一轮采 brief，慢系统冷却后重试。");
                }
            }
        }
        Err(e) => {
            println!("⚠ 连不上游戏（{}）→ 跳过同步规划；plan 保持占位，快系统下一轮采 brief。", e);
        }
    }
    Ok(())
}

/// **开局资源侦察**：探测关键资源分布并落盘 ~/.oni-agent/scout.json。
///
///  的对策：不知道资源在哪就深挖 → 挖开地下 CO₂ → 三人。
fn cmd_scout() -> anyhow::Result<()> {
    let s = connect()?;
    let cat = Catalog::load(&s).map_err(|e| anyhow::anyhow!("{e}"))?;
    let report = oni_engine::autoact::scout_resources(&s, Some(&cat));
    println!("{report}");
    println!("（已落盘 ~/.oni-agent/scout.json）");
    Ok(())
}

/// **一键重开**：回滚到指定存档 + 重置 plan + 清局内状态 + 开施工窗口。
///
/// 用法：`oni restart [存档索引]`（索引来自 `oni call game_control {"domain":"save","action":"list"}`）
/// 不给索引时用最近的一个 auto_save（Cycle N）。
///
/// 为什么需要：换存档后 plan.json 的 done 状态、目标锁、趋势样本都会继承上一局的脏数据，
///   代理会以为关键步骤做过了 → 开局卡住（）。本命令一次性清干净。
fn cmd_restart(args: &[String]) -> anyhow::Result<()> {
    let s = connect()?;
    let list = s
        .call_text(
            "game_control",
            serde_json::json!({ "domain": "save", "action": "list",
                                "task": "restart: 列出存档" }),
        )
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    // 解析 "index,文件名,..." 行，选目标
    let want: Option<usize> = args.first().and_then(|a| a.parse().ok());
    let mut picked: Option<String> = None;
    for line in list.lines() {
        let t = line.trim();
        if !t.starts_with(char::is_numeric) {
            continue;
        }
        let cols: Vec<&str> = t.split(',').collect();
        let idx: usize = cols.first().and_then(|x| x.trim().parse().ok()).unwrap_or(usize::MAX);
        // 用 name而不是 fileName
        //   —— restart_load 的 name 参数要的是显示名。
        let name = cols.get(2).map(|x| x.trim().to_string()).unwrap_or_default();
        // auto_save 判定：path 里含 auto_save 目录
        let is_auto = cols.iter().any(|c| c.contains("auto_save"));
        match want {
            Some(w) if w == idx => {
                picked = Some(name);
                break;
            }
            Some(_) => {}
            None => {
                if is_auto && picked.is_none() {
                    picked = Some(name);
                }
            }
        }
    }
    let Some(name) = picked else {
        anyhow::bail!("没找到可用的存档（给个索引试试）");
    };
    let out = s
        .call_text(
            "game_control",
            serde_json::json!({ "domain": "launch", "action": "restart_load",
                                "name": name, "confirm": true,
                                "task": format!("oni restart: 回滚到 {name}") }),
        )
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    println!("↩️ 已请求回滚到 {name}");
    for l in out.lines().take(3) {
        println!("   {l}");
    }
    // 清掉上一局的局内状态（plan / 目标锁 / 侦察报告 / 趋势样本）
    let state = oni_mcp::state_dir();
    let _ = std::fs::remove_file(state.join("goal_lock.json"));
    let _ = std::fs::remove_file(state.join("scout.json"));
    let _ = std::fs::remove_file(state.join("snapshots.jsonl"));
    let _ = std::fs::remove_file(state.join("current-session"));
    let _ = std::fs::remove_file(state.join("hold"));
    if std::path::Path::new("plan.template.json").exists() {
        let tpl = std::fs::read_to_string("plan.template.json")?;
        std::fs::write("plan.json", &tpl)?;
        println!("✅ plan.json 已按模板重置");
    }
    println!("✅ 已清：目标锁 / 侦察报告 / 趋势样本 / hold");
    Ok(())
}

fn cmd_window(args: &[String]) -> anyhow::Result<()> {
    let secs: u64 = args.first().and_then(|v| v.parse().ok()).unwrap_or(300);
    let path = oni_engine::agent::window_request_path();
    if let Some(dir) = path.parent() { std::fs::create_dir_all(dir).ok(); }
    std::fs::write(&path, secs.to_string())?;
    println!("已请求 {secs}s 施工窗口（守护进程下一轮接手，到点自动冻回暂停）");
    Ok(())
}

fn cmd_hold(args: &[String]) -> anyhow::Result<()> {
    // 暂停游戏 + 写 hold 标记：主 agent 改代码/深挖期间冻结世界，保持观察权。
    let reason = args.first().cloned().unwrap_or_else(|| "主 agent 正在处理".into());
    let s = connect()?;
    // 记录 hold 时的周期：release 时校验世界有没有被推进（见 cmd_release）
    let cycle = s.read_json("oni://colony/status").ok()
        .and_then(|v| v.get("cycle").and_then(|c| c.as_i64())).unwrap_or(0);
    oni_engine::agent::write_hold(&reason, cycle).map_err(|e| anyhow::anyhow!("写 hold 标记失败: {e}"))?;
    oni_engine::control::pause(oni_engine::control::Owner::Hold, &reason);
    let out = s.call_text("game_control", serde_json::json!({
        "domain": "speed", "action": "pause",
        "task": format!("oni: hold — {reason}"),
    })).map_err(|e| anyhow::anyhow!("{e}"))?;
    println!("⏸ hold 模式：游戏已暂停，代理只观察不自动恢复。");
    println!("   原因: {reason}");
    println!("   改完代码后执行 `oni release` 解除。");
    println!("{}", out.lines().take(2).collect::<Vec<_>>().join(" | "));
    Ok(())
}

fn cmd_release(args: &[String]) -> anyhow::Result<()> {
    let force = args.iter().any(|a| a == "--force" || a == "-f");
    // ★ 安全壳：release 前校验世界没有漂走。
    //   hold 的语义是"冻结世界"，但只有代理进程在跑时才守得住——进程被 kill 后
    //   游戏可能被手动/残留进程推进。实测：hold 期间游戏跑了 13 个周期、殖民地全灭，
    //   而 release 脚本无脑解除 → 代理在一个已死的局上继续决策。
    //   规则：hold 文件里记录 hold 时的周期；release 时周期变了 → 拒绝（除非 --force）。
    if !force {
        if let Some(held_cycle) = oni_engine::agent::hold_cycle() {
            if let Ok(s) = connect() {
                let now_cycle = s.read_json("oni://colony/status").ok()
                    .and_then(|v| v.get("cycle").and_then(|c| c.as_i64())).unwrap_or(0);
                if now_cycle != held_cycle {
                    anyhow::bail!(
                        "⛔ release 拒绝：hold 时周期 {}，现在周期 {} —— 世界在 hold 期间被推进了 {} 个周期。\n   代理的 decisions/窗口还是 hold 时的，直接 release 会让它用旧记忆面对新世界。\n   先 oni observe 核对局面；确认无误用 oni release --force（会清局内上下文后解除）。",
                        held_cycle, now_cycle, (now_cycle - held_cycle).abs()
                    );
                }
            }
        }
    }
    oni_engine::agent::clear_hold();
    oni_engine::control::resume(oni_engine::control::Owner::Hold);
    println!("✅ hold 已解除：代理恢复自主循环（需要时用 `oni window <秒>` 开施工窗口）");
    Ok(())
}


/// 轻量解析 agent.toml（[section] key = value），供 serve 读取快慢系统配置。
/// 返回 (fast_interval, fast_window, slow_interval, web_port, llm_endpoint, llm_key, llm_model, llm_format)
fn read_agent_toml() -> Option<(u64, u64, u64, u16, String, String, String, String)> {
    let path = std::path::Path::new("agent.toml");
    let text = std::fs::read_to_string(path).ok()?;
    let mut section = String::new();
    let mut fast_i = 10u64; let mut fast_w = 120u64;
    let mut slow_i = 5u64; let mut port = 8789u16;
    let mut ep = String::new(); let mut key = String::new();
    let mut model = String::new(); let mut fmt = String::new();
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') { continue; }
        if t.starts_with('[') && t.ends_with(']') {
            section = t[1..t.len()-1].trim().to_string();
            continue;
        }
        let (k, v) = match t.split_once('=') {
            Some((k, v)) => (k.trim(), v.trim().trim_matches('"').trim_matches('\'').to_string()),
            None => continue,
        };
        match (section.as_str(), k) {
            ("fast", "interval") => fast_i = v.parse().unwrap_or(fast_i),
            ("fast", "window") => fast_w = v.parse().unwrap_or(fast_w),
            ("slow", "interval") => slow_i = v.parse().unwrap_or(slow_i),
            ("slow", "web_port") => port = v.parse().unwrap_or(port),
            ("llm", "endpoint") => ep = v,
            ("llm", "key") => key = v,
            ("llm", "model") => model = v,
            ("llm", "format") => fmt = v,
            _ => {},
        }
    }
    Some((fast_i, fast_w, slow_i, port, ep, key, model, fmt))
}

/// ★ 进程互斥：同一时间只允许一个快系统守护进程（serve / agent）。
///
/// 事故：改代码时 pkill -f "oni serve" 杀不到早先 nohup 起的 oni agent（命令名不同），
/// 残留的旧二进制 agent 继续裸跑 1 小时（没有慢系统、没有 v3 修复），
/// 它开紧急窗口把游戏放跑了 13 个周期 → 全灭。
///
/// 做法：pid 文件 + 进程存活探测（kill -0，macOS/Linux 自带，不引第三方 crate）。
/// 不是猜意图，是进程生命周期的硬约束。
fn acquire_daemon_lock(cmd: &str) -> anyhow::Result<DaemonLock> {
    let path = oni_mcp::state_dir().join("daemon.pid");
    if let Ok(text) = std::fs::read_to_string(&path) {
        if let Some(pid) = text.lines().next().and_then(|l| l.trim().parse::<i32>().ok()) {
            let alive = std::process::Command::new("kill").args(["-0", &pid.to_string()])
                .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status()
                .map(|s| s.success()).unwrap_or(false);
            if alive {
                let what = text.lines().nth(1).unwrap_or("?").to_string();
                anyhow::bail!(
                    "⛔ 已有守护进程在跑（pid {pid}，{what}）。\n   先停它：kill {pid}（或 pkill -f \"release/oni (serve|agent)\"），再启动。\n   不要让两个快系统同时操作同一个世界。"
                );
            }
        }
    }
    std::fs::write(&path, format!("{}\n{}\n{}", std::process::id(), cmd, chrono::Local::now().format("%m-%d %H:%M")))
        .map_err(|e| anyhow::anyhow!("写 pid 文件失败: {e}"))?;
    Ok(DaemonLock { path })
}

struct DaemonLock { path: std::path::PathBuf }
impl Drop for DaemonLock {
    fn drop(&mut self) { let _ = std::fs::remove_file(&self.path); }
}

/// 统一启动：一个进程同时跑快系统(agent循环) + 慢系统(LLM决策+Web)。
/// 配置从项目根 agent.toml 读取（复制 agent.example.toml 填写）。
/// 打印「大脑此刻看到的事实简报」（window::facts_summary，决策与窗口 prompt 共用）。
/// 审计传感器信息是否准确、足够时，一条命令就能看到慢系统拿到什么。
fn cmd_brief() -> anyhow::Result<()> {
    let s = connect()?;
    let snap = oni_engine::sense::sense(&s);
    let v = serde_json::to_value(&snap)?;
    let cycle = v.get("cycle").and_then(|c| c.as_i64()).unwrap_or(0);
    println!("{}", oni_engine::window::facts_summary(&v, cycle));
    Ok(())
}

fn cmd_serve() -> anyhow::Result<()> {
    let _lock = acquire_daemon_lock("serve")?;
    use oni_engine::agent::{Agent, AgentConfig};
    let s = connect()?;
    let cat = Catalog::load(&s).map_err(|e| anyhow::anyhow!("{}", e))?;

    // ★ 加载本存档的选址策略（慢系统此前调整过的判据 → 跨进程保持生效）。
    if let Some(id) = current_save() {
        let p = oni_mcp::save::save_dir(&id.fingerprint).join("site_policy.json");
        if let Ok(t) = std::fs::read_to_string(&p) {
            if let Ok(v) = serde_json::from_str::<oni_world::preflight::SitePolicy>(&t) {
                oni_world::preflight::set_site_policy(v);
            }
        }
    }

    let mut fast = AgentConfig::default();
    let mut slow = oni_engine::slow::SlowConfig::default();
    if let Some((fi, fw, si, port, ep, key, model, fmt)) = read_agent_toml() {
        fast.interval_secs = fi;
        fast.work_window_secs = fw;
        slow.interval_secs = si;
        slow.web_port = port;
        if !ep.is_empty() { slow.endpoint = Some(ep); }
        if !key.is_empty() { slow.api_key = Some(key); }
        if !model.is_empty() { slow.model = model; }
        if !fmt.is_empty() { slow.api_format = fmt; }
        println!("已加载 agent.toml 配置");
    } else {
        println!("未找到 agent.toml，使用默认配置（可复制 agent.example.toml 填写）");
    }

    // 慢系统线程（自带 Web server）
    let web_port = slow.web_port;
    let slow_thread = std::thread::spawn(move || oni_engine::slow::run(slow, false));

    let mut agent = Agent::new(s, cat, fast);
    println!("oni serve 启动：快系统每{}s/窗口{}s + 慢系统+Web(:{})",
        agent.cfg.interval_secs, agent.cfg.work_window_secs, web_port);
    loop {
        let r = agent.cycle();
        let mut line = format!("[{}] cycle={} state={} verdict={} 节点={} 动作={}",
            chrono::Local::now().format("%H:%M:%S"), r.cycle, r.run_state.label(), r.verdict.label(),
            r.node.clone().unwrap_or_else(|| "-".into()), if r.acted { "已执行" } else { "无" });
        for n in &r.notes { line.push_str(" | "); line.push_str(n); }
        println!("{}", line);
        std::thread::sleep(std::time::Duration::from_secs(agent.cfg.interval_secs));
    }
    #[allow(unreachable_code)]
    let _ = slow_thread.join();
}

/// 慢系统服务：常驻轮询 escalation/用户指令 → LLM 决策 → 执行 + Web 页面。
fn cmd_slow(args: &[String]) -> anyhow::Result<()> {
    let once = args.iter().any(|a| a == "--once");
    let mut cfg = oni_engine::slow::SlowConfig::default();
    for a in args {
        if let Some(v) = a.strip_prefix("--port=") {
            cfg.web_port = v.parse().map_err(|_| anyhow::anyhow!("bad port"))?;
        }
        if let Some(v) = a.strip_prefix("--interval=") {
            cfg.interval_secs = v.parse().map_err(|_| anyhow::anyhow!("bad interval"))?;
        }
    }
    let _ = current_save();
    oni_engine::slow::run(cfg, once);
    Ok(())
}

fn cmd_agent(args: &[String]) -> anyhow::Result<()> {
    let _lock = acquire_daemon_lock("agent")?;
    use oni_engine::agent::{Agent, AgentConfig};
    let s = connect()?;
    let cat = Catalog::load(&s).map_err(|e| anyhow::anyhow!("{e}"))?;
    let mut cfg = AgentConfig::default();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--interval" => { cfg.interval_secs = args.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or(10); i += 2; }
            "--plan" => { cfg.plan_path = args.get(i + 1).cloned().unwrap_or_else(|| "plan.json".into()); i += 2; }
            "--cooldown" => { cfg.pause_cooldown = args.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or(600); i += 2; }
            "--window" => { cfg.work_window_secs = args.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or(90); i += 2; }
            _ => i += 1,
        }
    }
    let mut agent = Agent::new(s, cat, cfg);
    println!("oni-agent 守护进程启动：每 {}s 一轮；施工窗口 {}s（AI总开关=hold/release）",
        agent.cfg.interval_secs, agent.cfg.work_window_secs);
    loop {
        let r = agent.cycle();
        let mut line = format!("[{}] cycle={} state={} verdict={} 节点={} 动作={}",
            chrono::Local::now().format("%H:%M:%S"), r.cycle, r.run_state.label(), r.verdict.label(),
            r.node.clone().unwrap_or_else(|| "-".into()), if r.acted { "已执行" } else { "无" });
        for n in &r.notes { line.push_str(" | "); line.push_str(n); }
        println!("{line}");
        std::thread::sleep(std::time::Duration::from_secs(agent.cfg.interval_secs));
    }
}

fn cmd_order(args: &[String]) -> anyhow::Result<()> {
    let s = connect()?;
    let cmd = args.first().ok_or_else(|| anyhow::anyhow!("用法: oni order <命令>"))?.clone();
    let out = oni_engine::exec::run_order_raw(&s, &cmd, false).map_err(|e| anyhow::anyhow!("{e}"))?;
    println!("{out}");
    Ok(())
}

fn cmd_build(args: &[String]) -> anyhow::Result<()> {
    let s = connect()?;
    let cat = Catalog::load(&s).map_err(|e| anyhow::anyhow!("{e}"))?;
    let mut positional: Vec<String> = Vec::new();
    let mut apply = false;
    for a in args {
        if a == "--apply" { apply = true; } else { positional.push(a.clone()); }
    }
    if positional.len() < 2 {
        anyhow::bail!("用法: oni build <x,y> <Prefab> [材料] [--apply]");
    }
    let pts = parse_points(&positional[0]).map_err(|e| anyhow::anyhow!("{e}"))?;
    let a = pts[0];
    let def = cat.find(&positional[1])
        .ok_or_else(|| anyhow::anyhow!("目录里找不到 {}（目录必须是 includeUnavailable 全量）", positional[1]))?;
    let mat = match positional.get(2) {
        Some(m) if !m.is_empty() => m.clone(),
        _ => def.available_materials.first().map(|m| m.tag.clone()).unwrap_or_default(),
    };
    let token = oni_engine::mapedit::build_token(&def.prefab_id, &mat);
    let mut placements = std::collections::BTreeMap::new();
    placements.insert(a, token.clone());
    let (ok, out) = oni_engine::mapedit::apply_build(&s, &placements, !apply)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    println!("{} {} @({},{}) 材料={} → {}", if apply { "已落地" } else { "dry-run" },
        def.prefab_id, a.0, a.1, mat, if ok { "OK" } else { "失败" });
    println!("{out}");
    Ok(())
}

fn cmd_raw(args: &[String]) -> anyhow::Result<()> {
    let uri = args.first().ok_or_else(|| anyhow::anyhow!("用法: oni raw <oni://资源URI>"))?.clone();
    let s = connect()?;
    let v = s.read_json(&uri).map_err(|e| anyhow::anyhow!("读 {uri} 失败: {e}"))?;
    println!("{}", serde_json::to_string_pretty(&v)?);
    Ok(())
}

fn cmd_call(args: &[String]) -> anyhow::Result<()> {
    let tool = args.first().ok_or_else(|| anyhow::anyhow!("用法: oni call <tool> '<json args>'"))?.clone();
    let raw = args.get(1).cloned().unwrap_or_else(|| "{}".into());
    let mut v: serde_json::Value = serde_json::from_str(&raw).map_err(|e| anyhow::anyhow!("args 不是合法 JSON: {e}"))?;
    if let Some(o) = v.as_object_mut() {
        o.entry("task").or_insert_with(|| serde_json::Value::String("oni call: 原始工具调用".into()));
    }
    let s = connect()?;
    let out = s.call_text(&tool, v).map_err(|e| anyhow::anyhow!("{e}"))?;
    println!("{out}");
    Ok(())
}

fn cmd_reach() -> anyhow::Result<()> {
    let s = connect()?;
    let cat = Catalog::load(&s).ok();
    let Some(r) = oni_world::preflight::load_reach(&s, cat.as_ref()) else {
        anyhow::bail!("读不到世界模型");
    };
    let mut cells: Vec<(i32, i32)> = r.set.iter().copied().collect();
    cells.sort();
    println!("可达格数 = {}（ONI 自己报的是 12 左右，量级一致说明模型对）", cells.len());
    let (mut x0, mut y0) = (i32::MAX, i32::MAX);
    let (mut x1, mut y1) = (i32::MIN, i32::MIN);
    for (x, y) in &cells {
        x0 = x0.min(*x); y0 = y0.min(*y); x1 = x1.max(*x); y1 = y1.max(*y);
    }
    if !cells.is_empty() {
        println!("范围 X={x0}..{x1}  Y={y0}..{y1}");
        for y in y1..=y0 {
            let row: String = (x0..=x1)
                .map(|x| if r.set.contains(&(x, y)) { 'O' } else { '.' })
                .collect();
            println!("  Y={y:>4} {row}");
        }
    }
    Ok(())
}

fn cmd_graph(args: &[String]) -> anyhow::Result<()> {
    let s = connect()?;
    let cat = Catalog::load(&s).ok();
    // 复制人数：用全局态势里的人数（气室续航按当前人口算）
    let snap = oni_engine::sense::sense(&s);
    // --full：读全图（不受相机视野限制），换地图/大地图时用
    let full = args.iter().any(|a| a == "--full");
    let g = if full {
        oni_world::MapGraph::build_full(&s, cat.as_ref(), None, None)
            .map_err(|e| anyhow::anyhow!("读全图失败: {e}"))?
    } else {
        oni_world::MapGraph::build(&s, cat.as_ref()).map_err(|e| anyhow::anyhow!("{e}"))?
    };
    if full {
        println!("[全图模式：不受相机视野限制]");
    }
    println!("{}", g.report(snap.dupes.len().max(1)));
    Ok(())
}

fn cmd_probe(args: &[String]) -> anyhow::Result<()> {
    let s = connect()?;
    let rect = args.first().ok_or_else(|| anyhow::anyhow!("用法: oni probe x1,y1,x2,y2"))?;
    let pts = oni_world::rules::parse_points(rect).map_err(|e| anyhow::anyhow!(e))?;
    let cells = oni_world::cell::fetch_many(&s, &pts);
    let mut keys: Vec<_> = cells.keys().copied().collect();
    keys.sort();
    for k in keys {
        let c = &cells[&k];
        // 挖掘标记只在 cell 地图文件里有，资源接口不返回 → 逐格读
        let mapfile = s.call_text("world_editor", serde_json::json!({
            "command": "read", "path": format!("/active/map/cell_{}_{}.md", k.0, k.1),
            "confirm": true, "task": "probe: 挖掘标记"
        })).unwrap_or_default();
        let dig = if mapfile.contains("挖掘标记: 有") { "有" } else { "无" };
        println!("  ({},{}) {:<16} solid={:<5} mass={:<8} 挖掘标记={} 建筑={:?}", k.0, k.1, c.element, c.is_solid(), c.mass_kg, dig, c.buildings.iter().map(|b| b.name.clone()).collect::<Vec<_>>());
    }
    Ok(())
}

fn cmd_pause_resume(args: &[String], pause: bool) -> anyhow::Result<()> {
    let s = connect()?;
    let action = if pause { "pause" } else { "resume" };
    let why = args.first().cloned().unwrap_or_else(|| if pause { "手动暂停".into() } else { "手动恢复".into() });
    let out = s.call_text("game_control", serde_json::json!({
        "domain": "speed", "action": action, "task": format!("oni: {why}")
    })).map_err(|e| anyhow::anyhow!("{e}"))?;
    // 暂停所有权：CLI 的 pause 来自用户/hook → 落盘 owner=User（跨进程重启保留）；
    //   resume 是用户显式恢复 → 清掉 owner，代理才能接管。
    if pause {
        oni_engine::control::pause(oni_engine::control::Owner::User, &why);
    } else {
        oni_engine::control::clear();
    }
    println!("{}", out.lines().take(3).collect::<Vec<_>>().join(" | "));
    Ok(())
}

/// P1-57：一次读一层地图的 **Visual Spatial Summary**（水体/气室/建筑/植物/危险点），
///   比逐格 probe 高效一个量级。图层按 32 行分块。
fn cmd_area(args: &[String]) -> anyhow::Result<()> {
    let y0: i32 = args.first().and_then(|s| s.parse().ok()).unwrap_or(192);
    let y1: i32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(223);
    if y0 > y1 { anyhow::bail!("用法: oni area <y0> <y1>（y0<=y1）"); }
    let s = connect()?;
    let l0 = (y0 / 32) * 32;
    let l1 = (y1 / 32) * 32;
    let mut lo = l0;
    while lo <= l1 {
        let path = format!("/active/map/layers/layer_{}_{}.md", lo, lo + 31);
        let args = serde_json::json!({ "command": "read", "path": path, "task": "区域摘要" });
        match s.call_text("world_editor", args) {
            Ok(text) => {
                // 只打印摘要段，跳过庞大的网格。
                // 暂停态下 mod 可能不生成摘要段 → fallback 打印文件头元信息，不空手而归。
                let mut in_summary = false;
                let mut summary_lines: Vec<&str> = Vec::new();
                let mut header_lines: Vec<&str> = Vec::new();
                for line in text.lines() {
                    if line.contains("## Grid Map") { break; }
                    if line.contains("## Visual Spatial Summary") { in_summary = true; continue; }
                    if in_summary {
                        if !line.trim().is_empty() { summary_lines.push(line); }
                    } else if line.starts_with('-') || line.starts_with('#') {
                        header_lines.push(line);
                    }
                }
                let out = if summary_lines.is_empty() { &header_lines } else { &summary_lines };
                for line in out { println!("{}", line); }
            }
            Err(e) => println!("（读 {} 失败: {}）", path, e),
        }
        lo += 32;
    }
    Ok(())
}

fn cmd_map(args: &[String]) -> anyhow::Result<()> {
    let s = connect()?;
    let rect = args.first().ok_or_else(|| anyhow::anyhow!("用法: oni map x1,y1,x2,y2"))?;
    let rect = rect.replace(",", "&");
    let uri = format!("oni://world/text-map?{rect}&sparse=true&includeElements=true&includeBuildings=true&includeDupes=true&detail=full&profile=minimal");
    let text = s.read_text(&uri).map_err(|e| anyhow::anyhow!("{e}"))?;
    // 打印全部章节标题与表格行（不同 profile 的章节结构不一致，做容错）
    let mut rows = 0usize;
    for line in text.lines() {
        if line.starts_with('#') {
            println!("{}", line);
            continue;
        }
        if line.starts_with('|') && !line.contains("---") && !line.starts_with("| Y ") && !line.starts_with("| Cell ") {
            rows += 1;
            println!("{}", line.trim_start_matches('|').trim());
        }
    }
    if rows == 0 {
        println!("(无表格行；原始响应前 600 字)\n{}", &text[..text.len().min(600)]);
    }
    Ok(())
}

/// **世界模型逐格诊断**：把「模型认为的」和「游戏里的」并排打出来。
///
/// 为什么需要：模型比游戏宽松时，代理会下没人能执行的工单，
///   而日志只显示"预检通过"——看不出模型到底哪一格判错了。
///   本命令打印每格的：网格类型 / 是否可站 / 脚下是不是地板 /
///   在不在自算可达集 / 在不在官方可达集 / 实时元素。
/// 用法：oni reach x1,y1,x2,y2
fn cmd_reach_cells(args: &[String]) -> anyhow::Result<()> {
    let s = connect()?;
    let cat = Catalog::load(&s).map_err(|e| anyhow::anyhow!("{e}"))?;
    let rect = args.get(0).ok_or_else(|| anyhow::anyhow!("用法: oni reach x1,y1,x2,y2"))?;
    let pts = parse_points(rect).map_err(|e| anyhow::anyhow!(e))?;
    let r = oni_world::preflight::load_reach(&s, Some(&cat))
        .ok_or_else(|| anyhow::anyhow!("读不到世界模型"))?;
    let live = oni_world::cell::fetch_many(&s, &pts);
    println!(
        "(x,y)  kind      可站 脚下实心 自算集 爬梯集 官方集 幻影 | 实时元素 [建筑锚点]"
    );
    for c in &pts {
        let kind = r.grid.kind(*c);
        let walk = r.grid.walkable(*c);
        let below = (c.0, c.1 - 1);
        let below_ok = r.grid.is_support(below);
        let l = live.get(c);
        let bl = l.map(|cd| {
            cd.buildings
                .iter()
                .map(|b| format!("{}@({},{})", b.prefab_id, b.anchor_x, b.anchor_y))
                .collect::<Vec<_>>()
                .join(",")
        });
        println!(
            "({},{}) {:<9} {:<4} {:<8} {:<6} {:<6} {:<6} {} | {} [{}]",
            c.0, c.1,
            format!("{kind:?}"),
            walk,
            below_ok,
            r.set.contains(c),
            r.climbable.contains(c),
            r.official.contains(c),
            if r.grid.phantom.contains(c) { "幻影" } else { "    " },
            l.map(|cd| format!("{} solid={}", cd.element, cd.is_solid()))
                .unwrap_or_else(|| "?".into()),
            bl.unwrap_or_default(),
        );
    }
    println!(
        "→ 自算可达集 {} 格 / 爬梯集 {} 格 / 官方集 {} 格",
        r.set.len(), r.climbable.len(), r.official.len()
    );
    Ok(())
}

/// **依赖图体检**：列出每个节点的 status / 判据 / 判据当前是否成立。
///
/// 为什么要这个命令（实测本局教训）：
///   计划里的 status=done 可以**未经任何判据校验**就存在。
///   实测：E1「地表换气」done 但可达上界只有 194（判据要 216）、
///   B1「床位」done 但 beds=0 → 依赖图以为这些都完成了
///   → 再没有任何节点要求产氧 → 基地慢慢耗尽。
/// 用法：oni plan-check [fix]
///       oni plan-check             （只打印，不改）
/// **手动触发 LLM 开局规划**：采集本局世界事实 → LLM 生成计划 → 强校验 → 落盘。
///
/// 用途：换局/回滚后想立刻重新规划，不用等慢系统自动触发。
fn cmd_plan_llm() -> anyhow::Result<()> {
    let s = connect()?;
    let snap = oni_engine::sense::sense(&s);
    let facts = oni_engine::opening::collect_facts(&s, &snap);
    let scout_report = oni_engine::autoact::scout_resources(&s, None);
    let rec = serde_json::json!({
        "facts": serde_json::to_value(&facts).unwrap_or_default(),
        "scout": scout_report,
        "cycle": snap.cycle,
        "population": snap.dupes.len(),
    });
    let brief = serde_json::to_string_pretty(&rec).unwrap_or_default();
    println!("📋 世界事实已采集（{} 字节）→ 调用 LLM 规划…", brief.len());
    let cfg = oni_engine::slow::SlowConfig::default();
    // 打上**当前存档指纹**：计划自证出身（换局检测靠它）
    let fp = current_save().as_ref().map(|i| i.fingerprint.clone());
    match oni_engine::opening::author_opening_plan(&cfg, &brief, "plan.json", fp.as_deref()) {
        Ok(msg) => println!("✅ {}", msg),
        Err(reason) => {
            println!("❌ LLM 规划失败：{reason}");
            anyhow::bail!(reason);
        }
    }
    Ok(())
}

fn cmd_plan_check(args: &[String]) -> anyhow::Result<()> {
    let path = "plan.json";
    // 用法：oni plan-check        （只体检）
    //       oni plan-check fix    （并把假 done 打回 todo）
    let fix = args.first().map(|s| s == "fix").unwrap_or(false);
    let text = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("读不到 {path}: {e}"))?;
    let v: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| anyhow::anyhow!("计划格式错误: {e}"))?;
    let nodes = v.get("nodes").and_then(|n| n.as_array()).cloned().unwrap_or_default();

    // ★ **真复核**：连游戏取一份快照，逐个 done 节点跑同一套判据。
    //   只有静态打印等于"我看到这里写着 done"，那正是本局翻车的根源
    //   （E1「地表换气」done 但可达上界只有 194 / B1「床位」done 但 beds=0）。
    let snap = match connect() {
        Ok(sess) => Some(oni_engine::sense::sense(&sess)),
        Err(e) => {
            eprintln!("（连不上游戏，跳过判据复核：{e}）");
            None
        }
    };

    println!("{:<6} {:<9} {:<7} {:<7} {}", "id", "status", "stall", "判据", "复核");
    let mut fake_done: Vec<String> = Vec::new();
    let mut unverifiable: Vec<String> = Vec::new();
    for n in &nodes {
        let id = n.get("id").and_then(|i| i.as_str()).unwrap_or("?");
        let status = n.get("status").and_then(|s| s.as_str()).unwrap_or("todo");
        let stall = n.get("stall").and_then(|s| s.as_u64()).unwrap_or(0);
        let check = n.get("success_check").cloned().unwrap_or(serde_json::Value::Null);
        let kind = check.get("kind").and_then(|k| k.as_str()).unwrap_or("").to_string();
        let mut verdict = String::new();
        if status == "done" {
            match snap.as_ref().and_then(|sn| oni_engine::agent::check_success(sn, &check)) {
                Some(true) => verdict = "✅ 判据成立".into(),
                Some(false) => {
                    verdict = "❌ **判据不成立 → 假 done**".into();
                    fake_done.push(id.to_string());
                }
                None => {
                    verdict = "⚠ 无机器判据（done 无法复核）".into();
                    unverifiable.push(id.to_string());
                }
            }
        } else if !kind.is_empty() {
            verdict = "待达成".into();
        }
        println!("{:<6} {:<9} {:<7} {:<7} {}", id, status, stall, kind, verdict);
    }
    println!("→ 共 {} 个节点", nodes.len());
    if !fake_done.is_empty() {
        println!("  ❌ 假 done（判据不成立）：{}", fake_done.join(", "));
        println!("     done 必须有证据：快系统每轮自动复核并打回 todo（契约不变量 7）");
    }
    if !unverifiable.is_empty() {
        println!("  ⚠ 无机器判据的 done：{}（无法自动复核）", unverifiable.join(", "));
    }
    if fix && !fake_done.is_empty() {
        if let Some(sn) = snap.as_ref() {
            let reopened = oni_engine::agent::recheck_done_nodes(path, sn);
            println!("→ 已打回 todo：{}", reopened.join(", "));
        }
    }
    Ok(())
}

fn cmd_preflight(args: &[String]) -> anyhow::Result<()> {
    let s = connect()?;
    let cat = Catalog::load(&s).map_err(|e| anyhow::anyhow!("{e}"))?;
    match args.first().map(|s| s.as_str()) {
        Some("dig") => {
            let rect = args.get(1).ok_or_else(|| anyhow::anyhow!("用法: oni preflight dig x1,y1,x2,y2"))?;
            let cells = parse_points(rect).map_err(|e| anyhow::anyhow!(e))?;
            let pf = preflight_dig(&s, &cells, Some(&cat));
            for t in &pf.targets {
                println!(
                    "({},{}) {:<16} 可挖={:<5} 落脚点={:<12} {}{} {}",
                    t.x, t.y, t.element, t.diggable,
                    t.work_position.map(|w| format!("({},{})", w.0, w.1)).unwrap_or_else(|| "✖无".into()),
                    if t.needs_ladder { "[需要梯子列]" } else { "" },
                    if t.is_floor { "[脚下地板]" } else { "" },
                    t.reasons.join("; ")
                );
            }
            // 前沿（当前就能开挖的格）：这决定了代理这一轮会下什么单
            println!("→ 前沿(可立即开挖) {} 格: {:?}", pf.ready.len(), pf.ready);
            println!("→ 复制人可达格数 = {}", pf.reachable_count);
            println!("→ 预检{}", if pf.ok { "通过" } else { "未通过（不会执行）" });
            if !pf.ok { std::process::exit(2); }
        }
        Some("build") => {
            let anchor = args.get(1).ok_or_else(|| anyhow::anyhow!("用法: oni preflight build x,y Prefab"))?;
            let prefab = args.get(2).ok_or_else(|| anyhow::anyhow!("用法: oni preflight build x,y Prefab"))?;
            let def = cat.find(prefab).ok_or_else(|| anyhow::anyhow!("目录里没有 {prefab}"))?;
            // 修正：**不能取目录第一个材料**（目录顺序 ≠ 地上有货）。
            //   取**第一个真实可用量 > 0** 的；都没有就如实报"需要采建材"。
            // **材料必须问实时 mod**（oni://buildings/materials），
            //   不能用 catalog 缓存里的 available_kg —— 缓存 TTL 30 分钟，
            //   库存早就变了。
            let live = oni_engine::exec::mod_materials_with_kg(&s, &def.prefab_id);
            let usable = live
                .iter()
                .filter(|(_, kg)| *kg > 0.0)
                .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
            let mat = usable.map(|(t, _)| t.clone()).unwrap_or_default();
            let found = usable.map(|(t, kg)| (t.clone(), *kg));
            let pts = parse_points(anchor).map_err(|e| anyhow::anyhow!(e))?;
            let pf = preflight_build(&s, pts[0], (def.width, def.height),
                found.as_ref().map(|(t, kg)| format!("{t} 可用 {kg:.0}kg")),
                found.as_ref().map(|(_, kg)| *kg > 0.0), Some(&cat), &def.prefab_id);
            println!("{} {} @({},{}) {}x{}", def.prefab_id, def.name, pts[0].0, pts[0].1, def.width, def.height);
            println!("  占格全空={} 支撑实心={} 落脚点={:?} 材料={:?}", pf.empty_cells, pf.supported, pf.work_position, pf.material_note);
            for r in &pf.reasons { println!("  · {r}"); }
            println!("→ 预检{}", if pf.ok { "通过" } else { "未通过（不会执行）" });
            if !pf.ok { std::process::exit(2); }
        }
        _ => anyhow::bail!("用法: oni preflight dig <rect> | oni preflight build <x,y> <Prefab>"),
    }
    Ok(())
}

fn cmd_plan() -> anyhow::Result<()> {
    let plan = load_plan(std::path::Path::new(PLAN)).map_err(|e| anyhow::anyhow!("{e}"))?;
    let d = decide(&plan);
    println!("=== 依赖图 ===");
    for n in &plan.nodes {
        let mark = if n.is_done() { "✓" } else { "·" };
        println!(" {mark} {:<8} {}", n.id, n.name);
    }
    println!("\n本轮目标: {}", match &d.chosen {
        Some(n) => format!("{} — {}", n.id, n.name),
        None => "（无可执行目标）".into(),
    });
    if let Some(n) = &d.chosen {
        if !n.note.is_empty() { println!("  理由: {}", n.note); }
        for a in &n.actions {
            println!("  动作: {:?}", a);
        }
        if !n.success.is_empty() { println!("  成功判据: {}", n.success); }
    }
    if !d.blocked.is_empty() {
        println!("\n被阻塞:");
        for (id, by) in &d.blocked { println!("  {id} ← 缺 {by}"); }
    }
    Ok(())
}

fn cmd_act() -> anyhow::Result<()> {
    let s = connect()?;
    let cat = Catalog::load(&s).map_err(|e| anyhow::anyhow!("{e}"))?;
    let plan = load_plan(std::path::Path::new(PLAN)).map_err(|e| anyhow::anyhow!("{e}"))?;
    let d = decide(&plan);
    let Some(node) = d.chosen else {
        println!("没有可执行目标");
        return Ok(());
    };
    println!("🎯 执行 [{}] {}", node.id, node.name);
    if node.actions.is_empty() {
        println!("  (该目标没有动作定义)");
        return Ok(());
    }
    let out = execute(&s, &cat, &node.actions);
    for r in &out.results {
        println!("  {} {} — {}", if r.ok { "✅" } else { "❌" }, r.kind, r.detail);
        if !r.why.is_empty() { println!("     ({})", r.why); }
    }
    println!("→ {}", if out.ok { "执行完成" } else { "执行失败（预检或落地被拦下）" });
    if !out.ok { std::process::exit(3); }
    Ok(())
}

fn cmd_watch(args: &[String]) -> anyhow::Result<()> {
    let interval = args.iter().position(|a| a == "--interval")
        .and_then(|i| args.get(i + 1)).and_then(|v| v.parse::<u64>().ok()).unwrap_or(5);
    let s = connect()?;
    println!("观察器启动：每 {interval}s 采全局面；危急自动暂停，条件解除自动恢复");
    let mut paused = false;
    let mut last_danger: Option<String> = None;
    loop {
        let snap = sense(&s);
        let samples = load_samples(200);
        let trend = compute(&samples, &snap);
        let j = judge(&snap, Some(&trend));
        let _ = record(&snap);
        let danger_key = j.dangers.join(";");
        let is_new = danger_key != last_danger.clone().unwrap_or_default();
        if !j.dangers.is_empty() && (is_new || !paused) {
            eprintln!("⛔ 紧急: {}", j.dangers.join(" | "));
            if !paused {
                let _ = s.call_text("game_control", serde_json::json!({
                    "domain": "speed", "action": "pause", "task": "观察器: 紧急暂停"
                }));
                paused = true;
            }
            last_danger = Some(danger_key);
        } else if j.dangers.is_empty() && paused {
            eprintln!("▶ 危急解除 → 自动恢复运行");
            let _ = s.call_text("game_control", serde_json::json!({
                "domain": "speed", "action": "resume", "task": "观察器: 恢复运行"
            }));
            paused = false;
            last_danger = None;
        }
        println!(
            "[{}] cycle={} paused={} oxy={:.0} breath={} idle={}/{} verdict={} ({}ms)",
            chrono::Local::now().format("%H:%M:%S"),
            snap.cycle, snap.paused, snap.oxygen_kg(),
            snap.min_breath().map(|v| format!("{v:.0}")).unwrap_or_else(|| "-".into()),
            snap.idle_count(), snap.dupes.len(),
            j.verdict.label(), snap.latency_ms
        );
        std::thread::sleep(Duration::from_secs(interval));
    }
}

fn f64s(v: Option<f64>) -> String {
    v.map(|x| format!("{x:.1}")).unwrap_or_else(|| "-".into())
}

fn print_card(snap: &oni_engine::sense::Snapshot, j: &oni_engine::judge::Judgment,
              l: &oni_engine::ledger::Ledger, t: &oni_engine::trend::Trend) {
    println!("┌─ 全局态势卡 ─────────────────────────────────");
    println!("│ 周期 {}  {}  快照 {}ms", snap.cycle, if snap.paused { "⏸ 已暂停" } else { "▶ 运行中" }, snap.latency_ms);
    println!("│ 食物 {:.0} kcal  氧气 {:.0} kg  可呼吸格 {}  最高压力 {}",
        snap.food_kcal(), snap.oxygen_kg(),
        snap.diagnostics.get("breathableCellsVisible").and_then(|v| v.as_i64()).unwrap_or(0),
        snap.max_stress());
    println!("│ 建筑 床位{} 厕所{} 研究站{} 电池{} 制氧{}",
        snap.building("beds"), snap.building("toilets"), snap.building("researchStations"),
        snap.building("batteries"), snap.building("oxygenProducers"));
    if let Some(p) = snap.power.get("summary") {
        println!("│ 电力 发电机{} 电池{} 净{}W",
            p.get("generatorCount").and_then(|v| v.as_i64()).unwrap_or(0),
            p.get("batteryCount").and_then(|v| v.as_i64()).unwrap_or(0),
            p.get("netActiveWatts").and_then(|v| v.as_f64()).unwrap_or(0.0));
    }
    if let Some(a) = snap.atmos.get("airQuality").and_then(|v| v.as_str()) {
        println!("│ 大气 窒息风险={} 空气质量={} 人均氧气={}",
            snap.atmos.get("suffocationRisk").and_then(|v| v.as_str()).unwrap_or("?"), a,
            snap.atmos.get("oxygenPerDupeKg").and_then(|v| v.as_f64()).unwrap_or(0.0));
    }
    if let Some(arr) = snap.gas.get("dominantGases").and_then(|v| v.as_array()) {
        let s: Vec<String> = arr.iter().take(4).map(|g| format!("{} {:.0}kg/{}格",
            g.get("element").and_then(|v| v.as_str()).unwrap_or("?"),
            g.get("massKg").and_then(|v| v.as_f64()).unwrap_or(0.0),
            g.get("cellCount").and_then(|v| v.as_i64()).unwrap_or(0))).collect();
        println!("│ 气体 {}", s.join("  "));
    }
    for d in &snap.dupes {
        println!("│ · {:<12} @({},{}) {:<8} 呼吸{} 压力{} 膀胱{} 可达{} {} 阻塞{}",
            d.name, d.x, d.y, d.chore, f64s(d.breath), f64s(d.stress), f64s(d.bladder),
            d.reachable.map(|v| v.to_string()).unwrap_or_else(|| "-".into()), d.element, d.blocked);
    }
    for a in snap.alerts() { println!("│ ⚠ {a}"); }
    println!("│ 判定 {}", j.verdict.label());
    for x in &j.dangers { println!("│   ✖ {x}"); }
    for x in &j.actions { println!("│   → {x}"); }
    for x in &j.notes { println!("│   · {x}"); }
    println!("│ 账本 食物{}周期 氧气{}周期 电力缺口{:.0}W  ← 先死因: {}",
        f64s(l.food_cycles), f64s(l.oxygen_cycles), l.power_gap_w, l.first_death_cause);
    for w in &l.warnings { println!("│   · {w}"); }
    if t.oxygen_per_cycle.is_some() || t.food_per_cycle.is_some() {
        println!("│ 趋势 样本{} 氧气衰减{}kg/周期 食物消耗{}kcal/周期",
            t.samples, f64s(t.oxygen_per_cycle), f64s(t.food_per_cycle));
    }
    if !snap.errors.is_empty() {
        println!("│ ⚠ 观察源失败 {} 个（局部失败≠正常）", snap.errors.len());
        for e in &snap.errors { println!("│   ✖ {e}"); }
    }
    println!("└────────────────────────────────────────────────");
}
