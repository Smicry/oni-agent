//! 元素搜索：跨出本地网格视野，用 mod 的权威搜索找元素位置。
//!
//! ★ 为什么需要（2026-11-21 实测）：
//!   本地 Grid::load 只覆盖**相机视野**（viewport，典型 Y=176~216）。
//!   深埋资源（实测铁矿在 Y=171~176，走廊下方 17~22 格）根本不在网格里
//!   → grid.kind() 判不出实心 → 元素搜索永远找不到 → 节点卡死
//!   （实测报错："语义目标在当前地图上找不到（element:IronOre）"）。
//!   mod 的 world search 能搜全图 → 返回候选坐标。

use oni_mcp::McpSession;
use serde::Deserialize;

/// mod 的元素搜索结果（oni://world/search?query=<元素>&kinds=cells）。
#[derive(Debug, Clone, Deserialize)]
struct SearchCluster {
    // ★ mod 返回的是 camelCase（elementId / suggestedCells）——
    //   不写 rename 的话 serde 找不到字段 → 整段解析失败 → 搜索永远返回空！
    #[serde(default, rename = "elementId")]
    element_id: String,
    #[serde(default, rename = "suggestedCells")]
    suggested_cells: Vec<(i32, i32)>,
}

#[derive(Debug, Clone, Deserialize)]
struct SearchResult {
    #[serde(default)]
    clusters: Vec<SearchCluster>,
}

/// 用 mod 的权威搜索找元素位置。
///
/// 返回按"离复制人近"排序的候选格（调用方再按可爬性筛选）。
pub fn search_element(
    session: &McpSession,
    element: &str,
    near: Option<(i32, i32)>,
    limit: usize,
) -> Vec<(i32, i32)> {
    let uri = format!("oni://world/search?query={element}&kinds=cells");
    let Ok(v) = session.read_json(&uri) else {
        return Vec::new();
    };
    let Ok(sr) = serde_json::from_value::<SearchResult>(v) else {
        return Vec::new();
    };
    let mut out: Vec<(i32, i32)> = Vec::new();
    for c in sr.clusters {
        if !c.element_id.is_empty() && !c.element_id.eq_ignore_ascii_case(element) {
            continue;
        }
        for p in c.suggested_cells {
            if !out.contains(&p) {
                out.push(p);
            }
        }
    }
    if let Some((nx, ny)) = near {
        out.sort_by_key(|c| (c.0 - nx).abs() + (c.1 - ny).abs());
    }
    out.truncate(limit);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ★ mod 的 world search 返回 camelCase（elementId / suggestedCells）。
    ///   字段名写错 → serde 静默丢字段 → 搜索返回空 → 元素目标永远找不到。
    #[test]
    fn parses_camel_case_search_result() {
        let raw = r#"{"clusters":[{"elementId":"IronOre","suggestedCells":[[107,171],[108,172]]}]}"#;
        let sr: SearchResult = serde_json::from_str(raw).expect("parse");
        assert_eq!(sr.clusters.len(), 1);
        assert_eq!(sr.clusters[0].element_id, "IronOre");
        assert_eq!(sr.clusters[0].suggested_cells, vec![(107, 171), (108, 172)]);
    }

    /// 空结果也要能解析（不能 panic）。
    #[test]
    fn parses_empty_result() {
        let sr: SearchResult = serde_json::from_str(r#"{"clusters":[]}"#).expect("parse");
        assert!(sr.clusters.is_empty());
    }
}
