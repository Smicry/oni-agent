//! 坐标、占格与可达性的硬规则。纯函数，无 IO —— 供预检与规划热路径调用。

/// mod 的 Y 轴向上递增（Y=219 是地表，Y=179 最深）。
pub const Y_AXIS_UP: bool = true;

/// 锚点是左下角（Y 最小的那一行）；建筑从锚点向上生长。
pub const ANCHOR_IS_LOWER_LEFT: bool = true;

pub type Cell = (i32, i32);

/// 建筑占格：`(x..x+w-1, y..y+h-1)`。
pub fn footprint(x: i32, y: i32, w: i32, h: i32) -> Vec<Cell> {
    let mut v = Vec::with_capacity((w * h) as usize);
    for dy in 0..h {
        for dx in 0..w {
            v.push((x + dx, y + dy));
        }
    }
    v
}

/// 支撑格：锚点正下方 `y-1`（物理下方）。
pub fn support_cell(x: i32, y: i32) -> Cell {
    (x, y - 1)
}

/// 一个格子的 8 邻居（挖掘/建造时的落脚点候选）。
pub fn neighbors(x: i32, y: i32) -> Vec<Cell> {
    let mut v = Vec::with_capacity(8);
    for dy in -1..=1 {
        for dx in -1..=1 {
            if dx == 0 && dy == 0 {
                continue;
            }
            v.push((x + dx, y + dy));
        }
    }
    v
}

/// 矩形内的所有格（闭区间）。
pub fn rect_cells(x1: i32, y1: i32, x2: i32, y2: i32) -> Vec<Cell> {
    let (lo_x, hi_x) = if x1 <= x2 { (x1, x2) } else { (x2, x1) };
    let (lo_y, hi_y) = if y1 <= y2 { (y1, y2) } else { (y2, y1) };
    let mut v = Vec::new();
    for y in lo_y..=hi_y {
        for x in lo_x..=hi_x {
            v.push((x, y));
        }
    }
    v
}

/// 解析 "x,y" 或 "x1,y1,x2,y2"。
pub fn parse_points(s: &str) -> Result<Vec<Cell>, String> {
    let nums: Vec<i32> = s
        .split(|c: char| c == ',' || c == ' ' || c == ':')
        .filter(|t| !t.is_empty())
        .map(|t| t.parse::<i32>().map_err(|_| format!("坏坐标: {t}")))
        .collect::<Result<Vec<_>, _>>()?;
    match nums.len() {
        2 => Ok(vec![(nums[0], nums[1])]),
        4 => Ok(rect_cells(nums[0], nums[1], nums[2], nums[3])),
        _ => Err("坐标格式应为 x,y 或 x1,y1,x2,y2".into()),
    }
}

/// 难挖的岩层（会显著拖慢挖掘，但依然可挖）。
pub const HARD_ROCKS: &[&str] = &["IgneousRock", "Granite", "Obsidian", "Abyssalite", "MoltenRock"];

/// 可用于建造的建材（Raw 类）。
pub const BUILDABLE_RAW: &[&str] = &[
    "SandStone", "Dirt", "Clay", "Algae", "Carbon", "Cuprite", "IronOre", "GoldAmalgam",
    "Steel", "IgneousRock", "Granite", "Salt", "Sulfur", "Fossil", "Molybdenum", "Talc",
    "Phosphorite", "Snow", "Ice", "PollutedDirt", "Scoria", "Obsidian", "Glowstone",
];
