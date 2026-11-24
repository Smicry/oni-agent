//! 世界网格 + **可达性 BFS**。
//!
//! 只判断"几何上能不能站"不够：几何成立但复制人走不到的格子，会变成
//! "指令 ok、标记在、没人干活"的死订单。
//! 这里把整张地图读进内存，做 4 邻接 + 可走对角线的 BFS，
//! 只允许把"复制人真正走得到"的格子当作落脚点。

use std::collections::{HashMap, HashSet};

use oni_mcp::{Catalog, McpSession};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Solid,     // 实心天然岩
    Gas,       // 气体（可站）
    Liquid,    // 液体（不可站）
    Building,  // 建筑（实心）
    Ladder,    // 梯子（可站，且给上一格当地板）
    Protected, // 关键建筑保护区（打印舱/发电机：不能被压、不能被挖）
    Dupe,      // 复制人
    Unknown,
}

/// 关键建筑保护区：这些建筑被压/被拆会直接断电或断人。
/// 打印舱是唯一的新人来源，人力发电机是唯一的电源。
pub const PROTECTED: &[&str] = &[
    "Headquarters", "ManualGenerator", "Electrolyzer", "Battery", "BatteryMedium", "BatterySmart",
];

#[derive(Debug, Clone, Default)]
pub struct Grid {
    pub cells: HashMap<(i32, i32), Kind>,
    /// 地板砖（Tile）占格 —— `OnFloor` 建筑的人工支撑（天然岩石不算），见 `is_floor_tile()`。
    pub floor_tiles: HashSet<(i32, i32)>,
    /// 幻影建筑格：读不到实时 cell、退回容错锚点时多标出来、实际并不存在的建筑占格。
    ///
    /// 约束：建筑索引里的 Position 是判定点，不一定等于实际占格锚点（AGENTS.md §3.5）。
    /// 幻影格只做占位保护（别往这儿盖东西），不能承重——当成地板会让悬空格看起来能站，派工过去游戏拒收。
    pub phantom: HashSet<(i32, i32)>,
}

/// 官方字形表里所有"建筑类"单字符（从 `/active/symbols/glyphs.md` 读）。
///
/// 为什么不用硬编码：电线是「线」、发电机是「轮」、存储箱是「存」……
/// 硬编码漏一个就会把建筑格当成实心岩 → 建造直接压在电线上 → 电网断。
/// 官方表是生成的、覆盖全。
pub fn building_glyphs(session: &McpSession) -> HashSet<String> {
    let mut set = HashSet::new();
    let Ok(text) = session.call_text(
        "world_editor",
        serde_json::json!({
            "command": "read", "path": "/active/symbols/glyphs.md",
            "confirm": true, "task": "世界模型: 官方字形表"
        }),
    ) else {
        return set;
    };
    for line in text.lines() {
        // | `线` | Building | 电线 | `Wire` |
        let cols: Vec<&str> = line.split('|').map(|s| s.trim()).collect();
        if cols.len() < 3 || cols[2] != "Building" {
            continue;
        }
        let g = cols[1].trim_matches('`').trim();
        if !g.is_empty() {
            set.insert(g.to_string());
        }
    }
    set
}

/// 图例里的单字符 token → 类别
fn kind_of(token: &str, building_glyphs: &HashSet<String>) -> Kind {
    let t = token.trim();
    if t.is_empty() {
        return Kind::Unknown;
    }
    // 带 @(x,y) 的是建筑锚点渲染
    if t.contains('@') {
        if t.starts_with("Ladder") {
            return Kind::Ladder;
        }
        if t.starts_with('人') || t.starts_with('物') {
            return Kind::Dupe;   // 复制人站的那格 → 可站（它脚下是实的）
        }
        return Kind::Building;
    }
    match t {
        "梯" => Kind::Ladder,
        "人" | "物" => Kind::Dupe,
        "x" => Kind::Gas,           // 氧气
        "碳" => Kind::Gas,          // 二氧化碳
        "污" => Kind::Gas,          // 污染氧
        "染" => Kind::Liquid,       // 污染水
        "离" => Kind::Liquid,       // 水
        "氯" => Kind::Gas,          // 氯气
        "." | "?" => Kind::Unknown,
        // 建筑的非锚点渲染符号：用官方字形表判定（覆盖全部建筑）
        _ if building_glyphs.contains(t) => Kind::Building,
        // 天然实心：铁矿/氧石/磷矿/铜矿/泥土/砂岩/沙子/火成岩/煤炭/花岗岩/藻类/肥料/铅…
        _ => Kind::Solid,
    }
}

/// 解析一份地图文本（viewport.md 或 layers/layer_*.md 通用）。
///
/// 两种来源格式一致：三个 X 表头（百位/十位/个位）+ `Y=<row>: tok tok ┆ tok ...` 行。
/// 展开一行的 token 列表（处理 RLE 压缩）。
///
/// 分层地图文件（`/active/map/layers/*.md`）用 RLE：`?x5` = 5 个 `?`，
///   `Ox3` = 3 个 `O`。**不解 RLE 会让所有坐标错位**。
///   viewport.md 在 compact=false 下不用 RLE，但同一套解析器能兼容两者。
fn expand_row_tokens(body: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(256);
    for tok in body.split_whitespace() {
        if tok == "┆" {
            continue;
        }
        // RLE：后缀 "x<数字>" → 重复 <数字> 次
        if let Some(pos) = tok.rfind('x') {
            let (head, tail) = tok.split_at(pos);
            let count: usize = tail[1..].parse().ok().unwrap_or(0);
            if count > 0 && !head.is_empty() && head.chars().all(|c| !c.is_ascii_digit()) {
                for _ in 0..count {
                    out.push(head.to_string());
                }
                continue;
            }
        }
        out.push(tok.to_string());
    }
    out
}

fn parse_map_text(text: &str, building_glyphs: &HashSet<String>) -> HashMap<(i32, i32), Kind> {
    let mut cells = HashMap::new();
    // X 由表头决定：百位/十位/个位
    let mut cols: Vec<Vec<i32>> = Vec::new();
    for hl in text.lines() {
        if hl.starts_with("百位X:") || hl.starts_with("十位X:") || hl.starts_with("个位X:") {
            let nums: Vec<i32> = hl
                .split_once(':')
                .map(|(_, r)| r)
                .unwrap_or("")
                .split_whitespace()
                .filter_map(|t| t.parse().ok())
                .collect();
            cols.push(nums);
        }
    }
    if cols.len() != 3 {
        return cells;
    }
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("Y=") else { continue };
        let Some((ys, body)) = rest.split_once(':') else { continue };
        let Ok(y) = ys.trim().parse::<i32>() else { continue };
        let toks = expand_row_tokens(body);
        for (i, tok) in toks.iter().enumerate() {
            if i >= cols[0].len() {
                break;
            }
            let x = cols[0][i] * 100 + cols[1][i] * 10 + cols[2][i];
            cells.insert((x, y), kind_of(tok, building_glyphs));
        }
    }
    cells
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RLE 展开必须正确：`?x5` = 5 个 `?`，`Ox3` = 3 个 `O`。
    #[test]
    fn rle_expands() {
        assert_eq!(expand_row_tokens("?x5 ┆ Ox3 子").len(), 9);
        assert_eq!(expand_row_tokens("?x5 ┆ Ox3 子"), vec!["?", "?", "?", "?", "?", "O", "O", "O", "子"]);
        // 不带 RLE 的普通行也要对
        assert_eq!(expand_row_tokens("x x 子").len(), 3);
        // 建筑注释带 @ 和括号，不能被当成 RLE
        assert_eq!(expand_row_tokens("Tile@(126,193) 子").len(), 2);
    }

    /// 可站性 = 可通行 ∧ 紧邻下一格实心（与游戏 navmesh 对齐，AGENTS.md §3.5）。
    #[test]
    fn walkable_requires_immediate_floor() {
        let mut cells = HashMap::new();
        cells.insert((5, 10), Kind::Solid);
        cells.insert((5, 11), Kind::Gas);
        cells.insert((5, 12), Kind::Gas);
        cells.insert((5, 20), Kind::Ladder);
        cells.insert((5, 21), Kind::Gas);
        let g = Grid { cells, floor_tiles: HashSet::new(), phantom: HashSet::new() };
        assert!(g.walkable((5, 11)), "脚下就是岩石 → 可站");
        assert!(!g.walkable((5, 12)), "脚下悬空 → 不可站（游戏也不认）");
        assert!(g.walkable((5, 20)), "梯子格永远可站");
        assert!(g.walkable((5, 21)), "脚下是梯子 → 承重");
    }

    /// 幻影建筑格不能当地板：当成地板会让悬空格看起来能站，派工过去游戏拒收。
    #[test]
    fn phantom_building_is_not_a_floor() {
        let mut cells = HashMap::new();
        cells.insert((5, 10), Kind::Building);
        cells.insert((5, 11), Kind::Gas);
        let mut phantom = HashSet::new();
        phantom.insert((5, 10));
        let g = Grid { cells, floor_tiles: HashSet::new(), phantom };
        assert!(!g.is_support((5, 10)), "幻影建筑格不能承重");
        assert!(!g.walkable((5, 11)), "悬在幻影墙上的格子不能站");
    }

    /// 坐标对齐：X 由表头决定，RLE 展开后必须一格对一格。
    /// 用 6 列的小表头验证：X = 0,1,2,3,4,5。
    #[test]
    fn parse_aligns_coordinates() {
        let text = "百位X: 0 0 0 0 0 0\n十位X: 0 0 0 0 0 0\n个位X: 0 1 2 3 4 5\nY=10: ?x2 x 子";
        let glyphs: HashSet<String> = HashSet::new();
        let cells = parse_map_text(text, &glyphs);
        // X=0,1 是 ?（隐藏）→ Unknown；X=2 是 x（氧气）→ Gas；X=3 是子 → Solid
        assert_eq!(cells.get(&(0, 10)), Some(&Kind::Unknown));
        assert_eq!(cells.get(&(1, 10)), Some(&Kind::Unknown));
        assert_eq!(cells.get(&(2, 10)), Some(&Kind::Gas));
        assert_eq!(cells.get(&(3, 10)), Some(&Kind::Solid));
        assert_eq!(cells.len(), 4);
    }
}
/// 世界纵向分层：每层 32 行。mod 的 `/active/map/layers/` 按这个粒度切。
const LAYER_HEIGHT: i32 = 32;
/// 层文件数量上限。
const MAX_LAYERS: usize = 12;

impl Grid {
    /// 读相机视野内的地图（一次 MCP 读，快）。
    ///
    /// 预检用它：只关心"目标格周围 + 复制人可达区"，视野足够。
    pub fn load(session: &McpSession, catalog: Option<&Catalog>) -> Result<Grid, String> {
        let building_glyphs = building_glyphs(session);
        let text = session
            .call_text(
                "world_editor",
                serde_json::json!({
                    "command": "read", "path": "/active/map/viewport.md",
                    "format": "edit", "compact": false,
                    "confirm": true, "task": "世界模型: 读取整张地图"
                }),
            )
            .map_err(|e| e.to_string())?;
        let cells = parse_map_text(&text, &building_glyphs);
        let mut grid = Grid { cells, floor_tiles: HashSet::new(), phantom: HashSet::new() };
        grid.mark_protected(session);
        // 必须同时标记全部建筑的占格：否则建筑内部格子在网格里仍是 Gas，预检会选中它，落地报 occupied_by_building。
        grid.mark_official_buildings(session, catalog);
        Ok(grid)
    }

    /// 读**全图**（不受相机视野限制）。
    ///
    /// 为什么需要：`viewport.md` 只有相机看得见的 65×41 —— 换一张大地图、
    ///   或者把相机移开，"到地表要挖几格""哪个气室在憋 CO₂"就全算错了。
    ///   mod 提供 `/active/map/layers/layer_<y0>_<y1>.md`，
    ///   逐层读就能拿到整张星体的地形。
    ///
    /// 层是**按需读**的：只读覆盖 [y_lo, y_hi] 的层，其余跳过。
    pub fn load_full(
        session: &McpSession,
        catalog: Option<&Catalog>,
        y_lo: Option<i32>,
        y_hi: Option<i32>,
    ) -> Result<Grid, String> {
        let building_glyphs = building_glyphs(session);
        let mut cells: HashMap<(i32, i32), Kind> = HashMap::new();
        let mut read_any = false;
        let mut last_err = String::new();
        for i in 0..MAX_LAYERS {
            let y0 = i as i32 * LAYER_HEIGHT;
            let y1 = y0 + LAYER_HEIGHT - 1;
            // 只读需要的层（None = 全读）
            if let Some(lo) = y_lo {
                if y1 < lo { continue; }
            }
            if let Some(hi) = y_hi {
                if y0 > hi { continue; }
            }
            let path = format!("/active/map/layers/layer_{y0}_{y1}.md");
            match session.call_text(
                "world_editor",
                serde_json::json!({
                    "command": "read", "path": path,
                    "format": "edit", "compact": false,
                    "confirm": true, "task": "世界模型: 读取全图分层"
                }),
            ) {
                Ok(text) => {
                    read_any = true;
                    for (c, k) in parse_map_text(&text, &building_glyphs) {
                        cells.insert(c, k);
                    }
                }
                Err(e) => {
                    last_err = e.to_string();
                }
            }
        }
        if !read_any {
            return Err(format!("读不到全图分层: {last_err}"));
        }
        let mut grid = Grid { cells, floor_tiles: HashSet::new(), phantom: HashSet::new() };
        grid.mark_protected(session);
        // 全图加载路径也要标记全部建筑占格与地板砖：两种加载模式必须给出一致的占格判定。
        grid.mark_official_buildings(session, catalog);
        Ok(grid)
    }

    /// 标出关键建筑及其 1 格缓冲。
    fn mark_protected(&mut self, session: &McpSession) {
        let Ok(text) = session.call_text(
            "world_editor",
            serde_json::json!({
                "command": "read", "path": "/active/buildings/index.md",
                "confirm": true, "task": "世界模型: 关键建筑保护区"
            }),
        ) else {
            return;
        };
        for line in text.lines() {
            let cols: Vec<&str> = line.split('|').map(|s| s.trim()).collect();
            if cols.len() < 5 {
                continue;
            }
            let prefab = cols[2];
            if !PROTECTED.contains(&prefab) {
                continue;
            }
            // 建筑索引表格式：prefab 在 cols[2]，坐标在 cols[4]。
            let Some((xs, ys)) = cols[4].trim_start_matches('(').split_once(',') else {
                continue;
            };
            let (Ok(x), Ok(y)) = (xs.trim().parse::<i32>(), ys.trim().trim_end_matches(')').parse::<i32>()) else {
                continue;
            };
            // 4x4 打印舱 / 2x2 发电机：按 4x4 铺（够覆盖），再加 1 格缓冲
            // 缓冲只包正交邻格，不含对角：对角保护会挡住贴着建筑角落的通道（如竖井入口）。
            for dy in -1..=4 {
                for dx in -1..=4 {
                    let corner = (dx == -1 || dx == 4) && (dy == -1 || dy == 4);
                    if corner {
                        continue;
                    }
                    self.cells.insert((x + dx, y + dy), Kind::Protected);
                }
            }
        }
    }

    /// 从官方建筑索引读全部建筑的锚点，按目录尺寸铺开占格。
    ///
    /// 为什么不靠渲染字形猜：地图里电线是「线」、发电机是「轮」，
    /// 这些单字和元素字形（碳/藻/土）长得像，猜错就会把建筑格当成空地
    /// → 建造直接压在电线上 → 电网断开。
    fn mark_official_buildings(&mut self, session: &McpSession, catalog: Option<&Catalog>) {
        let Ok(text) = session.call_text(
            "world_editor",
            serde_json::json!({
                "command": "read",
                "path": "/active/buildings/index.md",
                "confirm": true,
                "task": "世界模型: 官方建筑索引"
            }),
        ) else {
            return;
        };
        // 先把索引里的建筑解析出来。
        let mut entries: Vec<(String, i32, i32)> = Vec::new();
        for line in text.lines() {
            let cols: Vec<&str> = line.split('|').map(|s| s.trim()).collect();
            if cols.len() < 5 {
                continue;
            }
            let prefab = cols[2];
            if prefab.is_empty() {
                continue;
            }
            // 建筑索引表格式：prefab 在 cols[2]，坐标在 cols[4]。
            let Some((xs, ys)) = cols[4].trim_start_matches('(').split_once(',') else {
                continue;
            };
            let (Ok(x), Ok(y)) = (xs.trim().parse::<i32>(), ys.trim().trim_end_matches(')').parse::<i32>()) else {
                continue;
            };
            entries.push((prefab.to_string(), x, y));
        }
        if entries.is_empty() {
            return;
        }
        // 取真实锚点：实时 cell 里每栋建筑都带 anchorX/anchorY（真值，AGENTS.md §3.5）。
        //   索引里的 Position 是判定点，与真实底边锚点可能差 1，不能用来猜占格。
        let probes: Vec<(i32, i32)> = entries.iter().map(|e| (e.1, e.2)).collect();
        let live = crate::cell::fetch_many(session, &probes);
        for (prefab, x, y) in entries {
            let (w, h) = catalog
                .and_then(|c| c.find(&prefab))
                .map(|d| (d.width.max(1) as i32, d.height.max(1) as i32))
                .unwrap_or((1, 1));
            let mut real: Vec<(i32, i32)> = Vec::new();
            if let Some(cd) = live.get(&(x, y)) {
                for b in &cd.buildings {
                    if b.prefab_id == prefab && !real.contains(&(b.anchor_x, b.anchor_y)) {
                        real.push((b.anchor_x, b.anchor_y));
                    }
                }
            }
            if !real.is_empty() {
                // 有真锚点 → 只标真占格，不造幻影。
                for (ax, ay) in &real {
                    for dy in 0..h {
                        for dx in 0..w {
                            self.cells.insert((ax + dx, ay + dy), Kind::Building);
                        }
                    }
                    if prefab == "Tile" {
                        self.floor_tiles.insert((*ax, *ay));
                    }
                }
                continue;
            }
            // 读不到实时数据 → 退回容错锚点，
            //   但多标出来的格子记成幻影：保护，但不承重。
            if prefab == "Tile" {
                self.floor_tiles.insert((x, y));
                self.floor_tiles.insert((x - 1, y));
            }
            for (ax, ay) in [(x, y), (x - 1, y), (x, y - 1)] {
                for dy in 0..h {
                    for dx in 0..w {
                        let c = (ax + dx, ay + dy);
                        if c == (x, y) {
                            self.cells.insert(c, Kind::Building);
                        } else if self.cells.get(&c) == Some(&Kind::Solid) {
                            // 底下本来就是岩石 → 保持岩石，别被幻影覆盖
                            continue;
                        } else {
                            self.cells.insert(c, Kind::Building);
                            self.phantom.insert(c);
                        }
                    }
                }
            }
        }
    }

    pub fn kind(&self, c: (i32, i32)) -> Kind {
        self.cells.get(&c).copied().unwrap_or(Kind::Unknown)
    }

    /// 能否当地板（实心岩 / 建筑顶 / 梯子）
    ///
    /// 幻影建筑格**不算地板**：它只是别盖这儿的占位，
    ///   当成地板会让悬空格看起来能站 → 死工单（见 phantom 字段注释）。
    pub fn is_support(&self, c: (i32, i32)) -> bool {
        if self.phantom.contains(&c) {
            return false;
        }
        matches!(self.kind(c), Kind::Solid | Kind::Building | Kind::Ladder)
    }

    /// 能否当 `OnFloor` 建筑的人工地板（地板砖 Tile）。
    ///
    /// ONI 的 `buildLocationRule=OnFloor` 建筑（人力发电机/旱厕/洗手盆）按规则要求下方是地板砖，
    /// 天然岩石不算；mod 报 `Unsupported OnFloor building: place floor/support tiles below` +
    /// `reasonCode=missing_support`。
    /// `Kind::Building` 里只有 Tile（砖块）算人工地板；其它建筑（打印舱/口粮箱/研究站…）不算。
    pub fn is_floor_tile(&self, c: (i32, i32)) -> bool {
        self.floor_tiles.contains(&c)
    }

    /// 某格能否站人：不是实心/建筑，且正下方就是地板。
    ///
    /// 可站性 = 可通行 ∧ 紧邻下一格实心，与官方可达集
    /// （`/active/dupes/reachability.md` 的 samples）及游戏 navmesh 逐格吻合（AGENTS.md §3.5）：
    /// 悬空格不可站；脚下是岩石/砖块的格可站；梯子格永远可站。
    /// 约束：不要放宽成"向下 N 格内有地板"——模型比游戏宽松会派工到没人能执行的格子。
    pub fn walkable(&self, c: (i32, i32)) -> bool {
        match self.kind(c) {
            Kind::Ladder => return true,
            Kind::Gas | Kind::Dupe | Kind::Liquid => {}
            _ => return false,
        }
        self.floor_within(c, 1)
    }

    /// 下方 8 格内能否找到地板（实心岩 / 建筑 / 梯子）。
    fn floor_within(&self, c: (i32, i32), depth: i32) -> bool {
        for k in 1..=depth {
            if self.is_support((c.0, c.1 - k)) {
                return true;
            }
        }
        false
    }

    /// 同 `walkable`，但官方可达区内的建筑格也算可站。
    ///
    /// 打印舱门廊、口粮箱这类"占格但实际能走"的建筑，靠渲染 token 猜会把走廊掐断，
    /// 所有工位被判"不可达"；官方 navigator 的可达包围盒（`/active/dupes/reachability.md`）才是真值。
    pub fn walkable_with(&self, c: (i32, i32), official: &HashSet<(i32, i32)>) -> bool {
        // 梯子必须直接进 open 集合：否则梯子格被当成墙，网格 BFS 的可达集不含梯子，爬梯可达集也扩不出来。
        let open = match self.kind(c) {
            Kind::Gas | Kind::Dupe | Kind::Liquid | Kind::Ladder => true,
            Kind::Building => official.contains(&c),
            _ => false,
        };
        if !open {
            return false;
        }
        // 梯子格永远可站（可攀爬）—— 已在 open 里，这里保留显式说明
        if self.kind(c) == Kind::Ladder {
            return true;
        }
        // 脚下紧邻一格就要有地板（与游戏 navmesh 对齐，见 `walkable`）：悬空格站不住，派工过去也是死工单。
        self.floor_within(c, 1)
    }

    /// 从种子出发做 BFS：4 邻接 + 可走对角（两个正交格都可走时）
    pub fn reachable(&self, seeds: &[(i32, i32)]) -> HashSet<(i32, i32)> {
        let mut seen: HashSet<(i32, i32)> = HashSet::new();
        let mut stack: Vec<(i32, i32)> = Vec::new();
        for s in seeds {
            if self.walkable(*s) && seen.insert(*s) {
                stack.push(*s);
            }
        }
        while let Some(c) = stack.pop() {
            let orth = [(c.0 + 1, c.1), (c.0 - 1, c.1), (c.0, c.1 + 1), (c.0, c.1 - 1)];
            for n in orth {
                if self.walkable(n) && seen.insert(n) {
                    stack.push(n);
                }
            }
            for (dx, dy) in [(1, 1), (1, -1), (-1, 1), (-1, -1)] {
                let d = (c.0 + dx, c.1 + dy);
                if self.walkable(d)
                    && seen.insert(d)
                    && (self.walkable((d.0, c.1)) || self.walkable((c.0, d.1)))
                {
                    stack.push(d);
                }
            }
        }
        seen
    }

    /// 带官方可达区豁免的 BFS：建筑格只要在官方包围盒内就当可走。
    pub fn reachable_with(
        &self,
        seeds: &[(i32, i32)],
        official: &HashSet<(i32, i32)>,
    ) -> HashSet<(i32, i32)> {
        let mut seen: HashSet<(i32, i32)> = HashSet::new();
        let mut stack: Vec<(i32, i32)> = Vec::new();
        for s in seeds {
            if self.walkable_with(*s, official) && seen.insert(*s) {
                stack.push(*s);
            }
        }
        while let Some(c) = stack.pop() {
            // 水平 + 向下（掉下去）：复制人**不能凭空往上爬**
            for n in [(c.0 + 1, c.1), (c.0 - 1, c.1), (c.0, c.1 - 1)] {
                if self.walkable_with(n, official) && seen.insert(n) {
                    stack.push(n);
                }
            }
            // 向上：必须有梯子（当前格或目标格）
            let up = (c.0, c.1 + 1);
            if self.walkable_with(up, official)
                && (self.kind(up) == Kind::Ladder || self.kind(c) == Kind::Ladder)
                && seen.insert(up)
            {
                stack.push(up);
            }
            // 对角：斜向上=上台阶（目标正下方必须实心）；斜向下/水平=不能穿角
            for (dx, dy) in [(1, 1), (1, -1), (-1, 1), (-1, -1)] {
                let d = (c.0 + dx, c.1 + dy);
                if !self.walkable_with(d, official) {
                    continue;
                }
                let ok = if dy == 1 {
                    self.is_support((d.0, d.1 - 1))
                } else {
                    self.walkable_with((d.0, c.1), official)
                        || self.walkable_with((c.0, d.1), official)
                };
                if ok && seen.insert(d) {
                    stack.push(d);
                }
            }
        }
        seen
    }
}
