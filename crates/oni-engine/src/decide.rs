//! DECIDE —— 按**依赖图**选本轮唯一目标，而不是按"当前最痛的问题"乱修。

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::exec::Action;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub requires: Vec<String>,
    #[serde(default)]
    pub unlocks: Vec<String>,
    #[serde(default)]
    pub actions: Vec<Action>,
    /// 机器可判的成功判据（JSON）
    #[serde(default, rename = "success_check")]
    pub success_check: Option<Value>,
    /// 人类可读的成功判据
    #[serde(default)]
    pub success: String,
    /// **救援节点**：有人倒地/窒息时，代理必须优先执行这类节点。
    ///
    /// 为什么需要：
    ///   倒地的复制人**不会执行任何差事** —— 代理还在下挖掘工单也白搭，
    ///   人继续泡在 CO2 里直到死亡。救援节点优先级高于一切普通施工。
    #[serde(default)]
    pub rescue: bool,
    #[serde(default)]
    pub note: String,
    /// P0-54 显式优先级：survival/rescue/urgent/normal。
    ///   decide() 先按 priority，再按 unlocks，最后按文件顺序。
    ///   解决"食物只剩 2 周期却因 unlocks=0 排最后"的问题。
    #[serde(default)]
    pub priority: Option<String>,
    /// **暂不可执行的原因**（如被研究锁）。
    ///
    /// 与 `blocked`（熔断）严格区分：
    ///   - `blocked` = 连续无进展熔断 → 代理反复试同一个方案 → 必须换方案；
    ///   - `research_locked` = **外部条件未满足**（研究没完成）→ 试多少次都一样，
    ///     但**不是代理的错** → 不该反复试、也不该冻回停摆，应该跳过它做别的。
    ///   旧逻辑把两者混为一谈 → 被研究锁的节点反复失败 → 殖民地反复停摆。
    #[serde(default)]
    pub deferred: Option<String>,
    /// 暂缓发生的**轮次**（用于冷却期计算，避免"暂缓-恢复"抖动）。
    #[serde(default)]
    pub deferred_at: Option<u64>,
}

impl Node {
    /// 机器可判的成功判据（兼容 `success` 字段是字符串或对象两种写法）
    pub fn success_obj(&self) -> Option<Value> {
        if let Some(v) = &self.success_check {
            return Some(v.clone());
        }
        None
    }
    pub fn is_done(&self) -> bool {
        self.status.eq_ignore_ascii_case("done")
    }
    /// 被熔断的节点：连续无进展 → 本轮不再选它（等人工或换方案）
    pub fn is_blocked(&self) -> bool {
        self.status.eq_ignore_ascii_case("blocked")
    }
    /// 暂不可执行（外部条件未满足，如被研究锁）→ 本轮跳过，但**不算熔断**。
    pub fn is_deferred(&self) -> bool {
        self.deferred.is_some()
    }
    /// 显式优先级排序值：越大越优先。unknown 按 normal 处理。
    pub fn priority_rank(&self) -> i32 {
        match self.priority.as_deref().unwrap_or("normal").to_ascii_lowercase().as_str() {
            "survival" | "rescue" => 3,
            "urgent" => 2,
            "low" => -1,
            _ => 0,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Plan {
    #[serde(default)]
    pub nodes: Vec<Node>,
    /// **保命节点**：空气告急 / 有人倒下时强制执行这个节点（通常是产氧/换气）
    #[serde(default)]
    pub survival_node: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Decision {
    pub chosen: Option<Node>,
    pub ready: Vec<String>,
    pub blocked: Vec<(String, String)>,
    /// 死锁解锁：正常模式下没有任何可选节点（全被熔断/完成），
    /// 复活了这批 blocked 节点。agent 需把它们写回 todo。
    ///
    /// 为什么必须有：熔断是"换方案"，不是"让依赖图永远死掉"。
    ///   N1 被熔断后 N2/N3/N4 全 requires N1 → 整张图没有 ready
    ///   日志刷屏"依赖图里没有可执行目标"，代理彻底停摆（真事故）。
    pub revived: Vec<String>,
}

pub fn load_plan(path: &Path) -> Result<Plan, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("读不到 {}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("计划文件格式错误: {e}"))
}

fn decide_inner(plan: &Plan, revive: bool) -> (Option<Node>, Vec<String>, Vec<(String, String)>) {
    // revive 模式下：blocked 节点视为"已尝试过"，依赖它的下游可以往下走。
    let done: BTreeMap<&str, bool> = plan
        .nodes
        .iter()
        .map(|n| (n.id.as_str(), n.is_done() || (revive && n.is_blocked())))
        .collect();
    let mut ready: Vec<&Node> = Vec::new();
    let mut blocked: Vec<(String, String)> = Vec::new();
    for n in &plan.nodes {
        if n.is_done() || (n.is_blocked() && !revive) {
            continue;
        }
        // 暂不可执行（研究锁 / 连续失败）→ 本轮跳过。
        //   但**不阻塞下游**：
        //     旧逻辑把 deferred 当成"没完成" → requires 判定不通过
        //     下游全被挡 → 整张图没有可执行目标 → 代理彻底停摆。
        //     暂缓 4 个节点 → 下游 7 个节点全被堵 → "依赖图里没有可执行目标"。
        //   正解：暂缓 = "这条现在做不了"，不是"这条永远做不了"
        //     下游照常参与调度（它自己可能不依赖暂缓节点的效果）。
        if n.is_deferred() {
            blocked.push((n.id.clone(), format!("暂缓: {}", n.deferred.clone().unwrap_or_default())));
            continue;
        }
        let unmet: Vec<&str> = n
            .requires
            .iter()
            // 依赖里若有被暂缓的节点 → 视为"暂时不阻塞"
            //   （下游自己可能能做；真做不了时它会自己失败并被暂缓）
            .filter(|r| {
                let dep_done = done.get(r.as_str()).copied().unwrap_or(false);
                if dep_done {
                    return false;
                }
                // 依赖被暂缓 → 不阻塞
                !plan
                    .nodes
                    .iter()
                    .find(|m| &m.id == *r)
                    .map(|m| m.is_deferred())
                    .unwrap_or(false)
            })
            .map(|s| s.as_str())
            .collect();
        if unmet.is_empty() {
            ready.push(n);
        } else {
            blocked.push((n.id.clone(), unmet.join(", ")));
        }
    }
    // Tie-break: prefer the node that unlocks the most; on a tie keep the
    // one appearing EARLIER in the plan file (file order = author priority).
    // Rust's max_by_key returns the LAST max on ties, which made the plan pick
    // a later node and starve the earlier one (air ventilation got skipped).
    let mut best: Option<&Node> = None;
    for n in &ready {
        match best {
            None => best = Some(n),
            Some(b) => {
                // P0-54：先比显式 priority，再比 unlocks。
                let nk = (n.priority_rank(), n.unlocks.len());
                let bk = (b.priority_rank(), b.unlocks.len());
                if nk > bk {
                    best = Some(n);
                }
            }
        }
    }
    let chosen = best.map(|n| (*n).clone());
    (chosen, ready.iter().map(|n| n.id.clone()).collect(), blocked)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk(id: &str, requires: &[&str], deferred: bool) -> Node {
        Node {
            id: id.into(),
            name: id.into(),
            status: "todo".into(),
            requires: requires.iter().map(|s| s.to_string()).collect(),
            unlocks: vec![],
            actions: vec![],
            success_check: None,
            success: String::new(),
            rescue: false,
            note: String::new(),
            priority: None,
            deferred: if deferred { Some("暂缓".into()) } else { None },
            deferred_at: None,
        }
    }

    /// 暂缓节点**不能阻塞下游**。
    ///
    /// 旧逻辑：deferred 当成"没完成" → requires 不通过 → 下游全被挡
    /// 整张图没有可执行目标 → 代理彻底停摆。
    #[test]
    fn deferred_node_does_not_block_downstream() {
        let plan = Plan {
            nodes: vec![
                mk("A", &[], true),      // 被暂缓
                mk("B", &["A"], false),  // 依赖被暂缓的 A
            ],
            survival_node: None,
        };
        let (chosen, ready, _blocked) = decide_inner(&plan, false);
        assert_eq!(chosen.map(|n| n.id), Some("B".to_string()),
            "暂缓节点不应阻塞下游（否则整张图死锁）");
        assert!(ready.iter().any(|r| r == "B"));
    }

    /// 暂缓节点**自己**不应被选中（避免立刻重试同一个失败方案）。
    #[test]
    fn deferred_node_is_not_selected() {
        let plan = Plan {
            nodes: vec![mk("A", &[], true)],
            survival_node: None,
        };
        let (chosen, _ready, _blocked) = decide_inner(&plan, false);
        assert!(chosen.is_none(), "暂缓节点本轮不应被选中");
    }

    /// P0-54：显式 priority 优先于 unlocks 数量。
    ///   食物告急节点 unlocks=0 但 priority=urgent，必须排在 unlocks=3 的 normal 节点前。
    #[test]
    fn priority_beats_unlock_count() {
        let mut urgent = mk("food", &[], false);
        urgent.priority = Some("urgent".into()); // unlocks=0，但 urgent
        let mut normal = mk("expand", &[], false);
        normal.unlocks = vec!["a".into(), "b".into(), "c".into()]; // unlocks=3，但 normal
        let plan = Plan {
            nodes: vec![normal, urgent],
            survival_node: None,
        };
        let (chosen, _ready, _blocked) = decide_inner(&plan, false);
        assert_eq!(chosen.map(|n| n.id), Some("food".to_string()),
            "urgent 节点即使 unlocks=0 也应优先于 normal 节点");
    }
}

/// 选节点：只看"前置已完成"的节点；在这些里选**能解锁最多下游**的那个。
///
/// 死锁兜底：正常模式无任何可选节点时，复活所有 blocked 节点重试。
///   熔断是为了换方案，不是让整张依赖图停摆。agent 会把 revived 里的节点写回 todo。
pub fn decide(plan: &Plan) -> Decision {
    let (chosen, ready, blocked) = decide_inner(plan, false);
    if chosen.is_some() {
        return Decision { chosen, ready, blocked, revived: Vec::new() };
    }
    // 死锁：没有任何 ready 节点 → 复活 blocked 节点，打破"全员熔断"的死局。
    let (chosen, ready, blocked) = decide_inner(plan, true);
    let revived: Vec<String> = chosen.as_ref().map(|n| vec![n.id.clone()]).unwrap_or_default();
    Decision { chosen, ready, blocked, revived }
}
