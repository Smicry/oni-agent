//! Structured decision log for the slow system (the AI agent).
//!
//! Fast-system judgements are code (thresholds in `judge.rs`); slow-system decisions
//! (plan changes, precise orders, code edits) are recorded here so they can be reviewed.
//! A record carries the alternatives considered, the rationale, an evidence level,
//! and the expected outcome. The next cycle fills in the actual outcome.
//! Without this feedback loop, a rule base grows but does not become more accurate.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Evidence {
    /// Game API / official virtual file
    Official,
    /// Shared daemon snapshot
    Snapshot,
    /// Inference from multiple sources
    Inferred,
    /// Heuristic or community value
    Heuristic,
}

impl Evidence {
    pub fn label(&self) -> &'static str {
        match self {
            Evidence::Official => "official",
            Evidence::Snapshot => "snapshot",
            Evidence::Inferred => "inferred",
            Evidence::Heuristic => "heuristic",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Open,
    Resolved,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Decision {
    pub id: u64,
    pub ts: i64,
    #[serde(default)]
    pub cycle: i64,
    /// Short description of the situation that triggered the decision
    pub context: String,
    #[serde(default)]
    pub candidates: Vec<String>,
    pub chosen: String,
    pub rationale: String,
    pub evidence: Evidence,
    /// What should be true if the decision was right (machine-checkable if possible)
    pub expected: String,
    #[serde(default)]
    pub actual: String,
    pub state: State,
}

fn path() -> PathBuf {
    match oni_mcp::save::last_seen() {
        Some(id) => oni_mcp::save::save_dir(&id.fingerprint).join("decisions.jsonl"),
        None => oni_mcp::state_dir().join("decisions.jsonl"),
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn all() -> Vec<Decision> {
    std::fs::read_to_string(path())
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<Decision>(l).ok())
        .collect()
}

fn write_all(items: &[Decision]) {
    let p = path();
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let mut buf = String::new();
    for it in items {
        if let Ok(l) = serde_json::to_string(it) {
            buf.push_str(&l);
            buf.push('\n');
        }
    }
    let _ = std::fs::write(&p, buf);
}

/// Record a decision. Returns its id.
pub fn record(
    cycle: i64,
    context: &str,
    candidates: &[String],
    chosen: &str,
    rationale: &str,
    evidence: Evidence,
    expected: &str,
) -> u64 {
    let d = Decision {
        id: now() as u64,
        ts: now(),
        cycle,
        context: context.to_string(),
        candidates: candidates.to_vec(),
        chosen: chosen.to_string(),
        rationale: rationale.to_string(),
        evidence,
        expected: expected.to_string(),
        actual: String::new(),
        state: State::Open,
    };
    let mut items = all();
    items.push(d);
    write_all(&items);
    let _ = crate::bus::publish(
        crate::bus::Kind::PlanUpdated,
        crate::bus::Priority::Info,
        "decision",
        chosen,
        serde_json::json!({"cycle": cycle, "evidence": evidence.label()}),
        0,
    );
    items.last().map(|x| x.id).unwrap_or(0)
}

/// Fill in the actual outcome of a decision.
pub fn resolve(id: u64, actual: &str) -> bool {
    let mut items = all();
    let mut hit = false;
    for d in items.iter_mut() {
        if d.id == id {
            d.actual = actual.to_string();
            d.state = State::Resolved;
            hit = true;
        }
    }
    if hit {
        write_all(&items);
    }
    hit
}

pub fn list(limit: usize) -> Vec<Decision> {
    let mut items = all();
    items.reverse();
    items.truncate(limit);
    items
}

pub fn summary() -> String {
    summary_for(10)
}

pub fn summary_for(n: usize) -> String {
    let items = list(n);
    if items.is_empty() {
        return "No decisions recorded yet.".to_string();
    }
    items
        .iter()
        .map(|d| {
            format!(
                "{} #{} [{}] {} — {}",
                if d.state == State::Resolved { "✓" } else { "○" },
                d.id,
                d.evidence.label(),
                d.chosen,
                if d.actual.is_empty() { &d.expected } else { &d.actual },
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn evidence_ordering_is_explicit() {
        // Evidence levels are distinct labels; callers must pick one explicitly.
        assert_ne!(Evidence::Official.label(), Evidence::Heuristic.label());
    }
}