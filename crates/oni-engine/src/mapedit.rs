//! 地图 token 编辑 —— 用 `.*` 通配符做补丁。
//!
//! **为什么用通配符**：地图网格里含复制人位置，游戏在跑时快照秒级过期，
//! 精确 token 补丁会 `SEARCH did not match`。模组文档明确：
//! SEARCH 里只有 `.*` 是通配符；REPLACE 里 `?`/`*`/`.*` 表示"保持原值"。
//! 于是补丁与复制人位置无关 → **写操作不需要暂停游戏**。

use std::collections::BTreeMap;

use oni_mcp::McpSession;

pub const MAP_PATH: &str = "/active/map/viewport.md";
pub const WILDCARD: &str = ".*";

/// 生成建造 token。
///
/// 关键：material 为空时**绝不能**留下 `Prefab:7#` 的悬空 `#`。
/// `Ladder` 的 `availableMaterials` 是空数组 → 旧代码拼出 `Ladder:7#SandStone`，
/// mod 报 `Unsupported map cell transitions ... x->Ladder:7#`（材料不在白名单 +
/// 悬空 `#`）→ 梯子永远铺不下去 → 竖井挖不动。**空材料必须省略整个 `#` 段**。
pub fn build_token(prefab: &str, material: &str) -> String {
    if material.is_empty() {
        format!("{prefab}:7")
    } else {
        format!("{prefab}:7#{material}")
    }
}

#[derive(Debug, Clone, Default)]
pub struct MapView {
    pub headers: Vec<String>,
    pub rows: BTreeMap<i32, Vec<String>>,
    pub xs: Vec<i32>,
}

pub fn read_map(session: &McpSession) -> Result<MapView, String> {
    let text = session
        .call_text(
            "world_editor",
            serde_json::json!({
                "command": "read", "path": MAP_PATH,
                "format": "edit", "compact": false,
                "confirm": true, "task": "地图: 读取可编辑视图"
            }),
        )
        .map_err(|e| e.to_string())?;
    let mut headers = Vec::new();
    let mut cols: Vec<Vec<i32>> = Vec::new();
    let mut rows = BTreeMap::new();
    for line in text.lines() {
        if line.starts_with("百位X:") || line.starts_with("十位X:") || line.starts_with("个位X:") {
            headers.push(line.to_string());
            let nums: Vec<i32> = line
                .split_once(':')
                .map(|(_, r)| r)
                .unwrap_or("")
                .split_whitespace()
                .filter_map(|t| t.parse::<i32>().ok())
                .collect();
            cols.push(nums);
        } else if let Some(rest) = line.strip_prefix("Y=") {
            if let Some((ys, body)) = rest.split_once(':') {
                if let Ok(y) = ys.trim().parse::<i32>() {
                    let toks: Vec<String> = body
                        .split_whitespace()
                        .filter(|t| *t != "┆")
                        .map(|t| t.to_string())
                        .collect();
                    rows.insert(y, toks);
                }
            }
        }
    }
    if cols.len() != 3 {
        return Err(format!("地图表头不完整（{} 行）", cols.len()));
    }
    let n = cols[0].len();
    let xs: Vec<i32> = (0..n)
        .map(|i| cols[0][i] * 100 + cols[1][i] * 10 + cols[2][i])
        .collect();
    Ok(MapView { headers, rows, xs })
}

pub fn render_row(y: i32, toks: &[String], xs: &[i32]) -> String {
    let cells: BTreeMap<i32, String> = xs.iter().cloned().zip(toks.iter().cloned()).collect();
    let mut chunks: Vec<String> = Vec::new();
    for chunk in xs.chunks(5) {
        let parts: Vec<String> = chunk
            .iter()
            .map(|x| cells.get(x).cloned().unwrap_or_else(|| "?".to_string()))
            .collect();
        chunks.push(parts.join(" "));
    }
    format!("Y={y}: {}", chunks.join(" ┆ "))
}

/// 生成通配符补丁：`(x,y) -> token`。
/// SEARCH：目标格写当前 token，其余写 `.*`；REPLACE：目标格写新 token，其余写 `.*`（保持原值）。
pub fn patch_wild(
    map: &MapView,
    placements: &BTreeMap<(i32, i32), String>,
) -> Result<Option<(String, String)>, String> {
    let mut by_row: BTreeMap<i32, BTreeMap<i32, String>> = BTreeMap::new();
    for ((x, y), tok) in placements {
        let row = map
            .rows
            .get(y)
            .ok_or_else(|| format!("Y={y} 不在地图视图内"))?;
        let idx = map
            .xs
            .iter()
            .position(|v| v == x)
            .ok_or_else(|| format!("X={x} 不在地图视图内"))?;
        if row.get(idx).map(|s| s == tok).unwrap_or(false) {
            continue; // 已经是目标值
        }
        by_row.entry(*y).or_default().insert(*x, tok.clone());
    }
    if by_row.is_empty() {
        return Ok(None);
    }
    let mut search = map.headers.clone();
    let mut replace = map.headers.clone();
    for (y, changes) in &by_row {
        let row = map.rows.get(y).unwrap();
        let mut s_cells = vec![WILDCARD.to_string(); map.xs.len()];
        let mut r_cells = vec![WILDCARD.to_string(); map.xs.len()];
        for (x, tok) in changes {
            let i = map.xs.iter().position(|v| v == x).unwrap();
            // 目标格也用通配符：补丁与复制人位置无关
            //   游戏在跑时复制人移动会让精确 token 补丁 SEARCH 失配（落地失败）。
            s_cells[i] = WILDCARD.to_string();
            r_cells[i] = tok.clone();
        }
        search.push(render_row(*y, &s_cells, &map.xs));
        replace.push(render_row(*y, &r_cells, &map.xs));
    }
    Ok(Some((search.join("\n"), replace.join("\n"))))
}

/// 落地建造：dry-run → apply。返回 (是否成功, 输出文本)。
pub fn apply_build(
    session: &McpSession,
    placements: &BTreeMap<(i32, i32), String>,
    dry_run: bool,
) -> Result<(bool, String), String> {
    let map = read_map(session)?;
    let Some((search, replace)) = patch_wild(&map, placements)? else {
        return Ok((true, "目标格 token 已是目标值，无需编辑".into()));
    };
    let block = format!("<<<<<<< SEARCH\n{search}\n=======\n{replace}\n>>>>>>> REPLACE");
    let out = session
        .call_text(
            "world_editor",
            serde_json::json!({
                "command": "edit", "path": MAP_PATH, "content": block,
                "dryRun": dry_run, "confirm": !dry_run,
                "task": "循环: 地图 token 编辑（通配符补丁）"
            }),
        )
        .map_err(|e| e.to_string())?;
    // 判定成功的依据：mod 成功时返回 `ok: true`（首行），失败时首行是 `ok: false`。
    //   旧写法 `!out.contains("ok: false")` 会把**正文里任何**提到 ok:false 的
    //   字样误判成失败（例如 details 里嵌套的子结果）。改成看首行 + applied。
    let first = out.lines().next().unwrap_or("").trim();
    let ok = first.starts_with("ok: true")
        || (first.is_empty() && !out.contains("failed: 1") && !out.contains("ok: false"));
    Ok((ok, out))
}
