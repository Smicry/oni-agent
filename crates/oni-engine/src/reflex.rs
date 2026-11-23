//! REFLEX —— 安全壳（v3 架构第三层的"脊髓"）。
//!
//! 这是**整个代理里唯一允许硬编码的地方**。为什么：
//!   红线（人不能死、不能窒息、膀胱爆必须立刻有厕所）不是"策略"，是物理约束——
//!   它们不该等 LLM 思考（5s 一轮 + 网络延迟，等它判断人已经死了），也不该靠
//!   自然语言匹配去找 plan 节点（实测 T1 的 why 含"厕所"→ 误匹配到采建材节点）。
//!   所以：**触发条件是硬阈值，反射动作是封闭集合，每个都有回归测试。**
//!
//! 边界（v3 原则 4：规则只放红线）：
//!   - 这里只回答"人现在有没有生命危险"和"世界在不在跑"，**不回答"该往哪走"**（那是 LLM）。
//!   - 反射动作用语义目标（toilet / mop_dirty / oxygen / forage），由 autoact 运行时翻译，
//!     不写死 prefab/坐标。
//!   - 反射失败就如实回报失败，不美化（v3 原则 2）。

use crate::priority::Action;
use crate::exec::Action as ExecAction;
use oni_mcp::McpSession;

/// 实时读游戏运行状态。**只问传感器**（v3 原则 1：感知不推断）。
///
/// 判据三重信号，任一为停即判停：
///   ① `isPaused`（暂停按钮）
///   ② `gameSpeed <= 0`（速度 0 等于没跑；mod 出现过 isPaused=false 但速度 0）
///   ③ 读不到 → 保守判停（宁可不下工单，也不可在停着的世界里下单）
///
/// `oni://colony/status` 走 1s TTL（session.rs 的 per-URI 覆盖），同一套 singleflight
/// 合并并发读 —— 判定时基本必读实时值，又不会打爆 mod 端点。
pub fn world_paused(session: &McpSession) -> bool {
    session
        .read_json("oni://colony/status")
        .ok()
        .map(|v| {
            let paused = v.get("isPaused").and_then(|b| b.as_bool()).unwrap_or(false);
            let speed = v.get("gameSpeed").and_then(|s| s.as_f64()).unwrap_or(-1.0);
            paused || (speed >= 0.0 && speed <= 0.0)
        })
        // 读不到传感器 = 不知道世界状态 = 当作停着（保守）
        .unwrap_or(true)
}

/// 把红线动作翻译成**不依赖 plan 依赖图**的反射动作。
///
/// 封闭集合，只有这 4 类保命反射；供电/扩张属于战略层，不在此列。
/// 每个反射用语义目标，由运行时探测 prefab 与落点。
pub fn vital_reflex(action: Action) -> Vec<ExecAction> {
    match action {
        Action::BuildToilet => vec![ExecAction::Auto {
            goal: "toilet".into(),
            why: "膀胱告急 → 安全壳反射（语义目标 toilet，运行时选 prefab/落点）".into(),
        }],
        Action::Rescue => vec![ExecAction::Auto {
            goal: "mop_dirty".into(),
            why: "有人受污染威胁 → 安全壳反射清污".into(),
        }],
        Action::ProduceOxygen | Action::Ventilate => vec![ExecAction::Auto {
            goal: "oxygen".into(),
            why: "氧气红线 → 安全壳反射产氧".into(),
        }],
        Action::ForageFood => vec![ExecAction::Auto {
            goal: "forage".into(),
            why: "食物告急 → 安全壳反射采集".into(),
        }],
        Action::Power | Action::FollowPlan => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 反射集合是封闭的：每个保命动作恰好一个反射，且语义目标可解析。
    #[test]
    fn reflex_set_is_closed_and_parseable() {
        for (act, goal) in [
            (Action::BuildToilet, "toilet"),
            (Action::Rescue, "mop_dirty"),
            (Action::ProduceOxygen, "oxygen"),
            (Action::Ventilate, "oxygen"),
            (Action::ForageFood, "forage"),
        ] {
            let v = vital_reflex(act);
            assert_eq!(v.len(), 1);
            match &v[0] {
                ExecAction::Auto { goal: g, .. } => assert_eq!(g, goal),
                other => panic!("反射必须用语义目标，实际 {other:?}"),
            }
            assert!(crate::autoact::parse_goal(goal).is_some(), "{goal} 必须可解析");
        }
        assert!(vital_reflex(Action::Power).is_empty(), "供电不是保命反射");
        assert!(vital_reflex(Action::FollowPlan).is_empty());
    }
}
