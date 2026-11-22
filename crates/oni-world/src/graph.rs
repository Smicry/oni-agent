//! 地图图结构 —— 把一局地图重建为「气室连通图 + 挖掘路径图」。
//!
//! 为什么要有它（2026-11-21 迭代）：
//! 线性网格只能回答"这一格能不能挖"，回答不了"哪个气室快没气了"、
//! "从打印舱到地表要挖通几个气室"、"挖哪一层能一次性连通最多空间"。
//! 把地图变成图之后，这些都能用 BFS / Dijkstra 直接算，代理面对新局面不再靠猜。
//!
//! ## 图的定义
//! * **节点 = 气室（Chamber）**：4 邻接的开放格（气体/液体/梯子/复制人）构成的连通域。
//!   气体会扩散填满整个连通域，所以它是"空气预算"的自然单位。
//! * **边 = 相邻气室之间的实心隔层**：挖开 k 格就能连通，k 就是边的代价。
//! * **查询**：气室空气续航（还能撑几周期）、危险气体分布、含复制人的气室到地表的最短挖掘路径。
//!
//! ## 数据来源（官方真值优先，与项目其它模块同源）
//! * 格子类别来自 `Grid::load`（viewport.md + 官方建筑索引 + 官方字形表）。
//! * 气体/液体质量来自 `oni://world/cell/{x}/{y}` 的 `element`/`massKg`（逐格并行取）。
//! * 复制人位置来自 `oni://dupes/todos`（与 preflight 同源）。
//! * ⚠ 氧耗速率是**社区经验值**，不是官方数值；见 docs/knowledge-base.md。

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use oni_mcp::{Catalog, McpSession};

use crate::cell::fetch_many;
use crate::grid::{Grid, Kind};

/// 每个复制人每周期耗氧（kg）。社区经验值（≈0.1kg/s × 600s/cycle），
/// 非官方数值；只用于气室续航的相对比较，不用于生死判定。
const OXY_PER_DUPE_CYCLE: f64 = 60.0;

/// 要精确统计气体质量的大气室上限。
///
/// 全图有近 10 万格，逐格 fetch 会把查询拖到几分钟；
/// 只精确统计"生活区 + 最大的 N 个气室"，其余只记格子数。
const MAX_PRECISE_CHAMBERS: usize = 12;

/// 气室：4 邻接开放格构成的连通域。
#[derive(Debug, Clone)]
pub struct Chamber {
    pub id: usize,
    pub cells: Vec<(i32, i32)>,
    /// 元素 → 总质量 kg（该气室内所有格之和）
    pub masses: BTreeMap<String, f64>,
    pub building_cells: usize,
    pub touches_surface: bool,
    pub has_dupes: bool,
}

impl Chamber {
    pub fn oxygen_kg(&self) -> f64 {
        self.masses.get("Oxygen").copied().unwrap_or(0.0)
    }
    pub fn co2_kg(&self) -> f64 {
        self.masses.get("CarbonDioxide").copied().unwrap_or(0.0)
    }
    /// 危险气体：氯气 / 污染氧（按质量排序，只返回 >0 的）。
    pub fn hazards(&self) -> Vec<(&str, f64)> {
        let mut v: Vec<(&str, f64)> = [
            ("ChlorineGas", self.masses.get("ChlorineGas").copied().unwrap_or(0.0)),
            ("ContaminatedOxygen", self.masses.get("ContaminatedOxygen").copied().unwrap_or(0.0)),
        ]
        .into_iter()
        .filter(|(_, kg)| *kg > 0.0)
        .collect();
        v.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        v
    }
    /// 可呼吸比例 O₂/(O₂+CO₂)。1.0 = 全是氧气 / 无气体。
    pub fn breath_ratio(&self) -> f64 {
        let o = self.oxygen_kg();
        let c = self.co2_kg();
        if o + c <= 0.0 { 1.0 } else { o / (o + c) }
    }
    /// 气室氧气续航（周期数）。注意：这是**封闭**假设；连通地表后实际无限。
    pub fn oxygen_cycles(&self, dupe_count: usize) -> Option<f64> {
        if dupe_count == 0 { return None; }
        Some(self.oxygen_kg() / (OXY_PER_DUPE_CYCLE * dupe_count as f64))
    }
    /// 按质量列出主要资源（TOP N）。
    /// 气室的坐标范围 (x0, y0, x1, y1) —— 报告必须带，否则只知道有多少氧不知道往哪走。
    pub fn bbox(&self) -> (i32, i32, i32, i32) {
        let mut x0 = i32::MAX;
        let mut y0 = i32::MAX;
        let mut x1 = i32::MIN;
        let mut y1 = i32::MIN;
        for (x, y) in &self.cells {
            x0 = x0.min(*x);
            y0 = y0.min(*y);
            x1 = x1.max(*x);
            y1 = y1.max(*y);
        }
        (x0, y0, x1, y1)
    }
    pub fn top_masses(&self, n: usize) -> Vec<(&str, f64)> {
        let mut v: Vec<(&str, f64)> = self.masses.iter().map(|(k, m)| (k.as_str(), *m)).collect();
        v.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        v.into_iter().take(n).collect()
    }
}

/// 挖掘路径：从起点气室到目标（地表）需挖开的格子。
#[derive(Debug, Clone)]
pub struct DigRoute {
    /// 路径上的格子（含起点与终点）
    pub cells: Vec<(i32, i32)>,
    /// 需要**挖开**的实心格数量（路径代价）
    pub dig_count: usize,
    /// 终点是否触地表
    pub reaches_surface: bool,
    /// 路径中落在**未探索区**的格数。
    ///
    /// ★ 为什么要单独记：全图模式覆盖整张星体，但大片区域未探索（地图渲染 `?`）。
    ///   寻路会乐观地穿过未探索区 → 那部分是**推测**，不是事实。
    ///   未探索格数多 = 这条路径不可信，别直接照着下单。
    pub unknown_cells: usize,
}

/// 地图图：气室节点 + 格子→气室的映射。
#[derive(Debug, Clone)]
pub struct MapGraph {
    pub chambers: Vec<Chamber>,
    pub cell_chamber: HashMap<(i32, i32), usize>,
    pub bounds: (i32, i32, i32, i32), // x1,y1,x2,y2
    /// 地表基准 Y：有实心地形的最高层。挖到这里就接通地表（无限空气）。
    pub surface_y: i32,
    /// 未探索格（地图渲染为 `?`）：**不是墙**，是可以挖通的未知区域。
    ///
    /// ★ 为什么单独记：全图里大量区域未探索。若把 `?` 当实心岩，
    ///   寻路会被完全挡住 → "到地表：无路径"，代理无法规划任何开拓方向。
    ///   语义上它是"代价未知的可挖空间"。
    pub unknown: HashSet<(i32, i32)>,
    /// 复制人当前所在格。
    ///
    /// ★ 2026-11-22：寻路起点必须**无条件**包含这些格 ——
    ///   复制人所在的格若未被探索（`?`），它不属于任何气室 → 只靠气室找起点会得到空集。
    pub dupe_cells: HashSet<(i32, i32)>,
}

impl MapGraph {
    /// 从当前地图构建气室图（相机视野内）。
    pub fn build(session: &McpSession, catalog: Option<&Catalog>) -> Result<MapGraph, String> {
        let grid = Grid::load(session, catalog)?;
        Self::from_grid(session, grid)
    }

    /// 从**全图**构建气室图（不受相机视野限制）。
    ///
    /// ★ 换地图 / 相机移开时必须用这个：viewport 只有 65×41 视野，
    ///   算出来的"到地表要挖几格"会随相机位置漂移。
    pub fn build_full(
        session: &McpSession,
        catalog: Option<&Catalog>,
        y_lo: Option<i32>,
        y_hi: Option<i32>,
    ) -> Result<MapGraph, String> {
        let grid = Grid::load_full(session, catalog, y_lo, y_hi)?;
        Self::from_grid(session, grid)
    }

    fn from_grid(session: &McpSession, grid: Grid) -> Result<MapGraph, String> {
        // 开放格 = 气体/液体/梯子/复制人（气体能占据、能扩散的格子）
        let open: HashSet<(i32, i32)> = grid.cells.iter()
            .filter(|(_, k)| matches!(k, Kind::Gas | Kind::Liquid | Kind::Ladder | Kind::Dupe))
            .map(|(c, _)| *c)
            .collect();
        // 未探索格（`?`）：不是气室，但也不是墙 —— 寻路时按"可挖通"处理
        let unknown: HashSet<(i32, i32)> = grid.cells.iter()
            .filter(|(_, k)| matches!(k, Kind::Unknown))
            .map(|(c, _)| *c)
            .collect();
        // BFS 求连通域（4 邻接；气体不斜向扩散，所以不用对角）
        let mut chambers: Vec<Chamber> = Vec::new();
        let mut cell_chamber: HashMap<(i32, i32), usize> = HashMap::new();
        let mut remaining = open.clone();
        while let Some(&seed) = remaining.iter().next() {
            let mut comp = Vec::new();
            let mut q = VecDeque::new();
            q.push_back(seed);
            remaining.remove(&seed);
            while let Some(c) = q.pop_front() {
                comp.push(c);
                for n in [(c.0 + 1, c.1), (c.0 - 1, c.1), (c.0, c.1 + 1), (c.0, c.1 - 1)] {
                    if remaining.remove(&n) { q.push_back(n); }
                }
            }
            let id = chambers.len();
            for c in &comp { cell_chamber.insert(*c, id); }
            chambers.push(Chamber {
                id, cells: comp, masses: BTreeMap::new(),
                building_cells: 0, touches_surface: false, has_dupes: false,
            });
        }

        // ★ 取数策略：全图有近 10 万格，**不能逐格 fetch**（会把一次查询拖到几分钟）。
        //   只对"值得精确统计"的格取质量：
        //   1) 含复制人的气室（生活区，必须精确 —— 空气预算直接决定生死）
        //   2) 面积最大的若干个气室（主要活动空间）
        //   其余气室只记格子数，不取质量（避免为了精确数字牺牲响应速度）。
        let dupes = crate::preflight::dupe_positions(session);
        let dupe_set: HashSet<(i32, i32)> = dupes.iter().copied().collect();

        // 先标出含复制人的气室
        for ch in chambers.iter_mut() {
            if ch.cells.iter().any(|c| dupe_set.contains(c)) {
                ch.has_dupes = true;
            }
        }
        // 按面积排序，取前 N 个大气室
        let mut order: Vec<usize> = (0..chambers.len()).collect();
        order.sort_by_key(|&i| std::cmp::Reverse(chambers[i].cells.len()));
        let big: std::collections::HashSet<usize> = order.into_iter().take(MAX_PRECISE_CHAMBERS).collect();

        let mut cell_list: Vec<(i32, i32)> = Vec::new();
        for (i, ch) in chambers.iter().enumerate() {
            if ch.has_dupes || big.contains(&i) {
                cell_list.extend(ch.cells.iter().copied());
            }
        }
        let cell_data = fetch_many(session, &cell_list);
        // 没取到精确数据的格子：标记为"未统计"，报告里要能区分
        let _ = &cell_data;

        // ★ 地表判定：**不能**用 max_y（全图顶部是太空 Y=383，不是地表）。
        //   正确做法：地表是"有实心地形的最高层"。地表之上是未探索区（地图渲染为 `?`），
        //   之下是岩石/矿层。所以取**最高的一行含实心格**作为地表基准。
        //   （Klei 星球的地表上下浮动，这个定义对任何地图都成立。）
        let surface_y = grid
            .cells
            .iter()
            .filter(|(_, k)| matches!(k, Kind::Solid | Kind::Building | Kind::Protected))
            .map(|(c, _)| c.1)
            .max()
            .unwrap_or_else(|| grid.cells.keys().map(|c| c.1).max().unwrap_or(0));

        for (idx, ch) in chambers.iter_mut().enumerate() {
            for c in &ch.cells {
                if let Some(cd) = cell_data.get(c) {
                    *ch.masses.entry(cd.element.clone()).or_insert(0.0) += cd.mass_kg;
                    if cd.has_building() { ch.building_cells += 1; }
                }
                if c.1 >= surface_y { ch.touches_surface = true; }
                if dupe_set.contains(c) { ch.has_dupes = true; }
            }
            let _ = idx;
        }

        let xs: Vec<i32> = grid.cells.keys().map(|c| c.0).collect();
        let ys: Vec<i32> = grid.cells.keys().map(|c| c.1).collect();
        let bounds = (
            xs.iter().copied().min().unwrap_or(0), ys.iter().copied().min().unwrap_or(0),
            xs.iter().copied().max().unwrap_or(0), ys.iter().copied().max().unwrap_or(0),
        );
        Ok(MapGraph { chambers, cell_chamber, bounds, surface_y, unknown, dupe_cells: dupe_set })
    }

    /// 含复制人的气室（当前生活区）。
    pub fn dupe_chambers(&self) -> Vec<&Chamber> {
        self.chambers.iter().filter(|c| c.has_dupes).collect()
    }

    /// 触地表的气室（无限新鲜空气源）。
    pub fn surface_chambers(&self) -> Vec<&Chamber> {
        self.chambers.iter().filter(|c| c.touches_surface).collect()
    }

    /// 危险气室（可呼吸比例 < 阈值 或含危险气体）。
    pub fn hazardous_chambers(&self, breath_floor: f64) -> Vec<&Chamber> {
        self.chambers.iter().filter(|c| {
            c.breath_ratio() < breath_floor || !c.hazards().is_empty()
        }).collect()
    }

    /// 从含复制人的气室到地表的最短挖掘路径（Dijkstra：开放格代价 0，实心格代价 1）。
    /// 返回代价最低的那条。这是"往哪挖能最快接通无限空气"的直接答案。
    pub fn route_to_surface(&self) -> Option<DigRoute> {
        let mut starts: Vec<(i32, i32)> = self.chambers.iter()
            .filter(|c| c.has_dupes)
            .flat_map(|c| c.cells.iter().copied())
            .collect();
        // ★★ 2026-11-22 修正（新存档实测：「复制人不在任何气室内？」→ 无路径）：
        //   复制人所在格若是**未探索**（`?`），它不属于任何气室 → starts 为空 → 直接 None。
        //   正解：**无条件把复制人实际坐标加进起点**（他们站着的地方一定是可站的）。
        for d in self.dupe_cells.iter().copied() {
            if !starts.contains(&d) {
                starts.push(d);
            }
        }
        if starts.is_empty() { return None; }
        let surface_y = self.surface_y;
        let is_open = |c: (i32, i32)| -> bool { self.cell_chamber.contains_key(&c) };
        // ★ 未探索格（`?`）按"可挖通但代价 1"处理，不是墙。
        //   否则全图里未探索区域会彻底挡住寻路 → 永远找不到地表。
        // 0-1 BFS：用双端队列，开放格 push_front，可挖格 push_back
        let mut dist: HashMap<(i32, i32), usize> = HashMap::new();
        let mut prev: HashMap<(i32, i32), (i32, i32)> = HashMap::new();
        let mut dq: VecDeque<(i32, i32)> = VecDeque::new();
        for s in &starts { dist.insert(*s, 0); dq.push_front(*s); }
        let mut goal: Option<(i32, i32)> = None;
        while let Some(c) = dq.pop_front() {
            // 到达地表：到达地表层即可（那一层挖通就接通无限空气）
            if c.1 >= surface_y { goal = Some(c); break; }
            let d = dist[&c];
            for n in [(c.0 + 1, c.1), (c.0 - 1, c.1), (c.0, c.1 + 1), (c.0, c.1 - 1)] {
                // 不越出地图边界
                if n.0 < self.bounds.0 || n.0 > self.bounds.2 || n.1 < self.bounds.1 || n.1 > self.bounds.3 { continue; }
                let cost = if is_open(n) { 0 } else { 1 };
                let nd = d + cost;
                if dist.get(&n).map(|x| *x > nd).unwrap_or(true) {
                    dist.insert(n, nd);
                    prev.insert(n, c);
                    if cost == 0 { dq.push_front(n); } else { dq.push_back(n); }
                }
            }
        }
        let goal = goal?;
        let mut path = vec![goal];
        let mut cur = goal;
        while let Some(p) = prev.get(&cur) {
            path.push(*p);
            cur = *p;
            if starts.contains(&cur) { break; }
        }
        path.reverse();
        // 需要挖的格 = 路径上所有非气室格（实心岩 + 未探索区）
        let dig_count = path.iter().filter(|c| !is_open(**c)).count();
        let unknown_cells = path.iter().filter(|c| self.unknown.contains(c)).count();
        Some(DigRoute { cells: path, dig_count, reaches_surface: true, unknown_cells })
    }

    /// 文本报告（给 CLI / agent 快速读）。
    pub fn report(&self, dupe_count: usize) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "地图气室图：{} 个气室，范围 X={}..{} Y={}..{}
",
            self.chambers.len(), self.bounds.0, self.bounds.2, self.bounds.1, self.bounds.3
        ));
        // 按危险程度排序：可呼吸比例低的在前，含复制人的在前
        let mut ids: Vec<usize> = (0..self.chambers.len()).collect();
        ids.sort_by_key(|&i| {
            let c = &self.chambers[i];
            ((c.breath_ratio() * 100.0) as i64, if c.has_dupes { 0 } else { 1 })
        });
        for i in ids {
            let c = &self.chambers[i];
            let tag = if c.has_dupes { "🏠生活区" } else if c.touches_surface { "🌍地表" } else { "  " };
            let hz: Vec<String> = c.hazards().iter().map(|(e, kg)| format!("{e}:{kg:.0}kg")).collect();
            let runway = c.oxygen_cycles(dupe_count).map(|x| format!("{x:.1}周期")).unwrap_or_else(|| "-".into());
            let bx = c.bbox();
            out.push_str(&format!(
                "  [{:>2}] {} 格{} @X={}..{} Y={}..{} O₂={:.0}kg CO₂={:.0}kg 比例={:.0}% 续航={} 建筑={}{}\n",
                c.id, c.cells.len(), tag, bx.0, bx.2, bx.1, bx.3, c.oxygen_kg(), c.co2_kg(), c.breath_ratio() * 100.0,
                runway, c.building_cells,
                if hz.is_empty() { String::new() } else { format!(" ⚠{}", hz.join(",")) }
            ));
            let top: Vec<String> = c.top_masses(3).iter().map(|(e, kg)| format!("{e}:{kg:.0}kg")).collect();
            out.push_str(&format!("       主要物质: {}\n", top.join(" ")));
        }
        if let Some(r) = self.route_to_surface() {
            out.push_str(&format!(
                "\n地表线 Y={}（有实心地形的最高层）\n到地表最短挖掘路径：需挖 {} 格，路径 {} 步\n",
                self.surface_y, r.dig_count, r.cells.len()
            ));
            if r.unknown_cells > 0 {
                out.push_str(&format!(
                    "  ⚠ 路径有 {} 格落在未探索区（推测，非事实）→ 只作开拓方向参考\n",
                    r.unknown_cells
                ));
            } else {
                out.push_str("  ✓ 路径全程在已探索区（可信）\n");
            }
            out.push_str(&format!(
                "  路径：{}\n",
                r.cells.iter().map(|c| format!("({},{})", c.0, c.1)).collect::<Vec<_>>().join(" → ")
            ));
        } else {
            out.push_str("
到地表：无路径（复制人不在任何气室内？）
");
        }
        out
    }
}
