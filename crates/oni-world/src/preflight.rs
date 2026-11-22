//! 下单前预检 —— 拦下"指令 ok 但没人能干活"的订单。
//!
//! 三道关：
//! 1. **目标可挖/可建**（实心岩 / 空格子）
//! 2. **支撑成立**（OnFloor 建筑要 y-1 行是实的）
//! 3. **落脚点在复制人真正走得到的连通域里**（世界模型 BFS，不是"几何上能站"）
//!
//! 第 3 条是血泪教训：几何成立但走不到 → 死标记 → 复制人一直空闲 → 空跑。

use std::collections::{HashMap, HashSet};

use oni_mcp::{Catalog, McpSession};

use crate::cell::fetch_many;
use crate::grid::{Grid, Kind};
use crate::rules::{footprint, neighbors, support_cell, HARD_ROCKS};

#[derive(Debug, Clone)]
pub struct TargetCheck {
    pub x: i32,
    pub y: i32,
    pub element: String,
    pub diggable: bool,
    /// 一个复制人**真正走得到**的落脚点
    pub work_position: Option<(i32, i32)>,
    /// 是不是复制人**脚下正踩着**的地板（挖了会塌层/掏空地基）
    pub is_floor: bool,
    /// 是否属于"竖井"：正下方是空的且不可达 → 需要先铺梯子才能往上挖
    pub needs_ladder: bool,
    pub hard_rock: bool,
    /// 该格**已有挖掘标记**（之前已经下过单）。
    ///
    /// ★ 为什么需要：代理每轮都取第一个前沿格，结果每轮重复对同一格下单，
    ///   既浪费又掩盖真实进展（标记已下但复制人在睡觉 → 看着像没进展）。
    ///   知道有没有标记，就能优先挑**没标记**的格子推进。
    pub has_marker: bool,
    /// ★ 液体陷阱：目标格正上方有大质量液体（>500kg），挖穿会被淹没。
    ///   本局实测：基地正上方是 5 万吨水袋，挖穿会淹死全殖民地 → 必须硬拦截。
    pub flood_risk: bool,
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct DigPreflight {
    pub targets: Vec<TargetCheck>,
    /// 前沿：当前就能开挖的格子（阶梯一格格长）
    pub ready: Vec<(i32, i32)>,
    pub ok: bool,
    /// 复制人当前可达的格子数（诊断用）
    pub reachable_count: usize,
}

#[derive(Debug, Clone)]
pub struct BuildPreflight {
    pub anchor: (i32, i32),
    pub size: (i32, i32),
    pub empty_cells: bool,
    pub supported: bool,
    pub work_position: Option<(i32, i32)>,
    pub material_ok: Option<bool>,
    pub material_note: Option<String>,
    pub reasons: Vec<String>,
    pub ok: bool,
}

/// 读取复制人当前位置（挖掘落脚点的可达性起点）
pub fn dupe_positions(session: &McpSession) -> Vec<(i32, i32)> {
    let Ok(todos) = session.read_json("oni://dupes/todos") else {
        return Vec::new();
    };
    todos
        .get("dupes")
        .and_then(|d| d.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|d| {
                    let x = d.get("x")?.as_i64()? as i32;
                    let y = d.get("y")?.as_i64()? as i32;
                    Some((x, y))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// 世界模型 + 复制人可达连通域
pub struct Reach {
    pub grid: Grid,
    pub set: HashSet<(i32, i32)>,
    /// **爬梯可达集** = 官方可达集 + 顺着梯子链往上能爬到的格。
    ///
    /// ★ 为什么必须单独算（2026-11-21 实测钉死）：
    ///   官方 reachability 查询**不反映"梯子可爬的格"** ——
    ///   连续铺好梯子、复制人真的踩着梯子挖开了头顶之后，
    ///   可达区查询仍然只显示原来的走廊那几格。
    ///   预检只信官方集 → 梯子井上方所有格都被判成"没有复制人站得到的落脚格"
    ///   → 竖井挖到一半就卡死（实测：挖开了 (125,196) 之后再也挖不动 (125,197)）。
    pub climbable: HashSet<(i32, i32)>,
    /// 官方导航的真实可达格（samples）——落脚点的权威依据
    pub official: HashSet<(i32, i32)>,
    /// 官方包围盒（建筑豁免用）
    pub bbox: HashSet<(i32, i32)>,
    /// 复制人当前所在格（一定是可达的）
    pub dupes: Vec<(i32, i32)>,
    /// 复制人当前所在格的 1 步邻域（人类玩家的"就挖我旁边"）
    pub dupe_hood: HashSet<(i32, i32)>,
}

pub fn load_reach(session: &McpSession, catalog: Option<&Catalog>) -> Option<Reach> {
    let grid = Grid::load(session, catalog).ok()?;
    let dupes = dupe_positions(session);
    let bbox = official_reach(session);
    let official = official_samples(session);
    let mut r = reach_from_grid(grid, dupes.clone(), bbox.clone(), official.clone());

    // ★★★ S1（2026-11-22 新存档实测）：**视野不够就自动升级全图**。
    //   viewport.md 只有**相机视野**（65×41）→ 相机一移开基地，
    //   预检的世界模型里就没有基地 → 所有「人能不能到那」的判断全错
    //   → 合法目标被判「没有落脚点」→ 代理反复失败/冻回。
    //   判据：**官方可达样本里出现了本地网格看不到（或标成未知）的格**
    //   → 说明本地视野没覆盖复制人真正能活动的区域 → 用全图层重算。
    if !view_covers_world(&r) {
        if let Some(full) = full_grid(session, catalog) {
            r = reach_from_grid(full, dupes.clone(), bbox.clone(), official.clone());
        }
    }
    Some(r)
}

/// 本地（相机视野）网格是否覆盖了复制人真正能活动的区域。
fn view_covers_world(r: &Reach) -> bool {
    if r.official.is_empty() {
        return true; // 没有官方样本可比 → 不做无谓的全图重算
    }
    !r.official
        .iter()
        .any(|c| matches!(r.grid.kind(*c), Kind::Unknown))
}

/// **全图网格缓存**（进程内，TTL 30s）。
static FULL_GRID: std::sync::OnceLock<
    std::sync::Mutex<Option<(Grid, std::time::Instant)>>,
> = std::sync::OnceLock::new();

fn full_grid(session: &McpSession, catalog: Option<&Catalog>) -> Option<Grid> {
    let slot = FULL_GRID.get_or_init(|| std::sync::Mutex::new(None));
    let mut guard = slot.lock().ok()?;
    let fresh = guard
        .as_ref()
        .map(|(_, at)| at.elapsed() < std::time::Duration::from_secs(30))
        .unwrap_or(false);
    if fresh {
        return guard.as_ref().map(|(g, _)| g.clone());
    }
    let grid = Grid::load_full(session, catalog, None, None).ok()?;
    *guard = Some((grid.clone(), std::time::Instant::now()));
    Some(grid)
}

/// 从网格构建可达集（视野/全图共用同一套规则）。
fn reach_from_grid(
    grid: Grid,
    dupes: Vec<(i32, i32)>,
    bbox: HashSet<(i32, i32)>,
    official: HashSet<(i32, i32)>,
) -> Reach {
    let set = if bbox.is_empty() {
        grid.reachable(&dupes)
    } else {
        grid.reachable_with(&dupes, &bbox)
    };
    let mut dupe_hood: HashSet<(i32, i32)> = HashSet::new();
    for d in &dupes {
        dupe_hood.insert(*d);
        for (dx, dy) in [(-1, 0), (1, 0), (0, -1), (0, 1), (-1, -1), (-1, 1), (1, -1), (1, 1)] {
            dupe_hood.insert((d.0 + dx, d.1 + dy));
        }
    }
    // ★★★ 2026-11-22 修正（长竖井卡死的**根因**）：
    //   爬梯可达集必须从**网格 BFS 的可达集**（r.set）出发，而不是从**官方稀疏样本**。
    //   官方 samples 只有十几个格（且是"离复制人近"的抽样），
    //   竖井顶端的梯子格根本不在里面 → 梯子链的**根部缺失** → 整条链断
    //   → 复制人爬不上去 → 上方的挖掘工单永远挂着 → 代理看到"已有标记"就不下新单
    //   → 双方互等（实测：竖井挖到 Y=195 就再也没动过）。
    //   r.set 是网格 BFS（含梯子规则）算出来的，**根部一定正确**。
    let climbable = compute_climbable(&grid, &set);
    Reach { grid, set, climbable, official, bbox, dupes, dupe_hood }
}

/// **爬梯可达集**：从官方可达集出发，顺着梯子链一格格往上扩。
///
/// 规则（实测定论）：站在**梯子格**上的复制人可以再往上爬一格。
/// 所以：某格是梯子且在可达集里 → 它的正上方（若为空）也可达。
/// 反复扩展直到不动点（梯子链可以有多级）。
pub fn compute_climbable(grid: &Grid, official: &HashSet<(i32, i32)>) -> HashSet<(i32, i32)> {
    let mut set = official.clone();
    loop {
        let mut added = false;
        let snapshot: Vec<(i32, i32)> = set.iter().copied().collect();
        for c in snapshot {
            if grid.kind(c) != Kind::Ladder {
                continue;
            }
            let up = (c.0, c.1 + 1);
            if set.contains(&up) {
                continue;
            }
            // 上面必须是空的（气体/液体），实心要等它先被挖开
            if matches!(grid.kind(up), Kind::Gas | Kind::Liquid | Kind::Dupe) {
                set.insert(up);
                added = true;
            }
        }
        if !added {
            break;
        }
    }
    set
}

/// 读取**游戏官方**的复制人可达区（navigator 算出的每人可达包围盒）。
///
/// 为什么不用我们自己从渲染 token 猜的：打印舱门廊、口粮箱这类
/// "占格但实际能走"的建筑会被判成墙 → 17 格走廊被算成 8 格 →
/// 所有工位都判"没人走得到"→ 预检全灭 → 一格也挖不动（真 bug）。
/// 官方 navigator 的包围盒才是真值。
/// 官方导航的**真实可达格**（`/active/dupes/reachability.md` 的 samples 数组）。
///
/// ★ 关键：samples 才是真值，bounds 只是包围盒。
///   用包围盒会把"框内但走不到"的格子也算成可达 → 下了没人能执行的工单
///   （实测：包围盒 15 格 vs ONI 真实 12 格，差出来的 3 格全是死工单）。
pub fn official_samples(session: &McpSession) -> HashSet<(i32, i32)> {
    let mut set = HashSet::new();
    let Ok(text) = session.call_text(
        "world_editor",
        serde_json::json!({
            "command": "read", "path": "/active/dupes/reachability.md",
            "radius": 16, "sampleLimit": 200, "confirm": true,
            "task": "世界模型: 官方可达区（真实 samples）"
        }),
    ) else {
        return set;
    };
    // ★ 只取 ```json 代码块里的那段 JSON：
    //   文件正文里还有 `payload={radius:14,...}` 这类说明文字，
    //   从第一个 '{' 截到最后一个 '}' 会把说明也包进来 → 解析失败 → 空集 →
    //   退化成"乐观模式" → 又下了没人能执行的死工单。
    let Some(fence) = text.rfind("```json") else { return set; };
    let Some(rel) = text[fence + 7..].find('{') else { return set; };
    let start = fence + 7 + rel;
    let Some(end) = text[start..].rfind('}') else { return set; };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text[start..=start + end + 1]) else {
        return set;
    };
    if let Some(items) = v.get("items").and_then(|i| i.as_array()) {
        for item in items {
            if let Some(samples) = item.get("samples").and_then(|s| s.as_array()) {
                for s in samples {
                    let x = s.get("x").and_then(|v| v.as_i64()).unwrap_or(-1);
                    let y = s.get("y").and_then(|v| v.as_i64()).unwrap_or(-1);
                    if x >= 0 && y >= 0 {
                        set.insert((x as i32, y as i32));
                    }
                }
            }
        }
    }
    set
}

/// 官方包围盒（用来判断"占格但能走"的建筑格是否可通行）。
pub fn official_reach(session: &McpSession) -> HashSet<(i32, i32)> {
    let mut out = HashSet::new();
    let Ok(text) = session.call_text(
        "world_editor",
        serde_json::json!({
            "command": "read", "path": "/active/dupes/reachability.md",
            "radius": 14, "sampleLimit": 14, "confirm": true,
            "task": "世界模型: 官方可达区"
        }),
    ) else {
        return out;
    };
    let key = "\"bounds\":[";
    let mut rest = text.as_str();
    while let Some(i) = rest.find(key) {
        let tail = &rest[i + key.len()..];
        let nums: Vec<i32> = tail
            .split(|c: char| !c.is_ascii_digit() && c != '-')
            .filter(|t| !t.is_empty())
            .take(4)
            .filter_map(|t| t.parse().ok())
            .collect();
        if nums.len() == 4 {
            let (x1, y1, x2, y2) = (nums[0], nums[1], nums[2], nums[3]);
            for x in x1.min(x2)..=x1.max(x2) {
                for y in y1.min(y2)..=y1.max(y2) {
                    out.insert((x, y));
                }
            }
        }
        rest = &tail[tail.find(']').map(|p| p + 1).unwrap_or(tail.len())..];
    }
    out
}

/// 挖掘预检。
pub fn preflight_dig(session: &McpSession, targets: &[(i32, i32)], catalog: Option<&Catalog>) -> DigPreflight {
    let reach = load_reach(session, catalog);
    let Some(r) = &reach else {
        return DigPreflight { targets: Vec::new(), ready: Vec::new(), ok: false, reachable_count: 0 };
    };
    let (grid, set) = (&r.grid, &r.set);

    let mut checks = Vec::new();
    let dupe_cells = dupe_positions(session);
    // 读目标格的标记状态（哪些格已经下过单）
    let marker_cells = fetch_many(session, targets);
    // ★ P0-65 液体陷阱：批量读目标格正上方 3 格，挖穿大质量液体会淹没殖民地。
    //   本局实测基地正上方是 5 万吨水袋 —— 必须在预检层硬拦截，不能靠运气。
    const FLOOD_MASS_KG: f64 = 500.0;
    let mut above_cells: Vec<(i32, i32)> = Vec::new();
    for (x, y) in targets {
        for dy in 1..=3 {
            above_cells.push((*x, *y + dy));
        }
    }
    let above_live = fetch_many(session, &above_cells);
    for (x, y) in targets {
        let c = (*x, *y);
        let kind = grid.kind(c);
        // ★ 正上方若有 >500kg 液体 → 挖穿会灌水淹没 → 硬标记，绝不进 ready
        let flood_risk = (1..=3).any(|dy| {
            above_live.get(&(*x, *y + dy))
                .map(|cd| cd.is_liquid() && cd.mass_kg > FLOOD_MASS_KG)
                .unwrap_or(false)
        });
        let mut reasons = Vec::new();
        if flood_risk {
            reasons.push(format!("液体陷阱：正上方有大质量液体（>{FLOOD_MASS_KG:.0}kg），挖穿会被淹没"));
        }
        let diggable = kind == Kind::Solid;
        if kind != Kind::Solid {
            reasons.push(format!("不是实心（{kind:?}），挖它没有意义"));
        }
        let hard = HARD_ROCKS.contains(&element_name(kind).as_str());
        if hard {
            reasons.push("硬岩，耗时长".to_string());
        }
        // 落脚点：邻居必须 walkable **且**在复制人的可达连通域里
        // ★ 关键：**不能从正上方往下挖**（挖完人会掉进洞里 → 挖出爬不回来的坑）。
        //   正下方（挖头顶）是安全的；同层（横挖）也安全。
        let mut wp_below = None;   // 正下方 → 挖头顶
        let mut wp_side = None;    // 同层 → 横挖
        let mut wp_above = None;   // 正上方 → 往下挖（仅当目标正下方实心）
        for n in neighbors(*x, *y) {
            if !grid.walkable(n) {
                continue;
            }
            // ★ 落脚点必须是**复制人真的走得到**的格（或复制人当前就站在那）。
            //
            // ★★ 2026-11-22 修正（小星局实测）：
            //   /active/dupes/reachability.md 的 samples 是**稀疏子集**
            //   （每个复制人只列 12 个样本，而实际可达 19~24 格）。
            //   只认 samples → 合法落脚点被判"没有复制人走得到的落脚格"
            //   → 预检不过 → 代理每轮失败 → 反复"冻回暂停" → 殖民地全程没活干。
            //   （实测：采煤/采铜节点整条被拒，人力发电机蓝图 20 分钟无人施工。）
            //
            // 正确判据 = 网格 BFS ∩ 官方包围盒（r.set，乐观但有界）
            //           ∪ 爬梯可达集（r.climbable：官方集 + 梯子链）
            //           ∪ 复制人当前所在格。
            //   梯子链必须留着：官方集不含"踩着梯子能爬到的格"，
            //   只用官方集会把梯子井上方误判成没有落脚点 → 竖井卡死（事故 7）。
            if !r.set.contains(&n)
                && !r.climbable.contains(&n)
                && !dupe_cells.iter().any(|d| *d == n)
            {
                continue;
            }
            match n.1.cmp(y) {
                std::cmp::Ordering::Less => wp_below = Some(n),
                std::cmp::Ordering::Equal => wp_side = Some(n),
                std::cmp::Ordering::Greater => wp_above = Some(n),
            }
        }
        let below = (*x, *y - 1);
        let below_safe = grid.is_support(below) || grid.kind(below) == Kind::Ladder;
        let wp = wp_below.or(wp_side).or(if below_safe { wp_above } else { None });
        // 正下方是空的 → 竖井：要么能直接站在下面挖（wp_below），要么需要梯子列
        let below_open = matches!(grid.kind(below), Kind::Gas | Kind::Liquid | Kind::Dupe);
        let needs_ladder = below_open && wp_below.is_none() && wp_side.is_none() && below_safe == false;
        if wp.is_none() && !needs_ladder {
            reasons.push("✖ 没有复制人走得到的落脚格 → 没人能过去挖（工位不可达）".into());
        }
        // ★ 不许挖"唯一支撑"：挖掉这格 → 它上方那个可达站立格失去地板 →
        //   人掉下去 → 可能掉进没地板的洞里爬不回来 → CO₂ 沉底 → 全员窒息。
        //   ★ 修正（2026-11-21）：旧判定只看"正上方有可达格"，把**头顶空腔**也误判成地板，
        //   导致"往上挖"永远被拦（挖 (136,193) 时 (136,194) 是刚挖开的空腔 → 误判）。
        //   正确条件：目标格是上方站立格的**唯一**支撑（上方格下方 8 格内只有目标格是实的）。
        let mut is_floor = false;
        for n in neighbors(*x, *y) {
            if n.1 == y + 1 && grid.walkable(n) && set.contains(&n) {
                // 上方格 (n.x, n.y) 的支撑：它下方 8 格里有多少个实的？
                let mut supports = 0;
                for k in 1..=8 {
                    if grid.is_support((n.0, n.1 - k)) {
                        supports += 1;
                    }
                }
                if supports <= 1 {
                    is_floor = true;   // 唯一支撑 → 挖了必塌
                }
            }
        }
        if is_floor {
            reasons.push("是复制人站立格的唯一支撑，挖了会塌层（不作为挖掘目标）".into());
        }
        // ★ 挖掘目标的**正下方必须有可站立的落脚点**（实心岩或梯子）。
        //   否则这一格是"悬空下挖"：挖完复制人就掉进没有地板的洞里，
        //   再挖下一层就更深 → 爬不回来 → CO₂ 沉底 → 全员窒息。
        //   本局就是这么把三个人封进竖井里全灭的。
        //   （往上挖靠梯子列：梯子格的正下方是梯子 → 允许。）
        let below = (*x, *y - 1);
        let below_ok = grid.is_support(below) || grid.kind(below) == Kind::Ladder;
        if !below_ok {
            reasons.push("正下方悬空 → 挖了会掉进没地板的洞（要往下挖先铺梯子/平台）".into());
        }
        checks.push(TargetCheck {
            x: *x, y: *y, element: element_name(kind), diggable, work_position: wp,
            is_floor, needs_ladder, hard_rock: hard,
            has_marker: marker_cells.get(&c).map(|cd| cd.has_dig_marker()).unwrap_or(false),
            flood_risk,
            reasons,
        });
    }
    let ready: Vec<(i32, i32)> = checks
        .iter()
        // ★ P0-65：液体陷阱格硬排除（挖穿会淹没殖民地）
        .filter(|c| c.diggable && (c.work_position.is_some() || c.needs_ladder) && !c.is_floor && !c.flood_risk)
        .map(|c| (c.x, c.y))
        .collect();
    let ok = !ready.is_empty();
    DigPreflight { targets: checks, ready, ok, reachable_count: set.len() }
}

/// 建造预检：占格空？支撑实心？落脚点可达？材料合法？
pub fn preflight_build(
    session: &McpSession,
    anchor: (i32, i32),
    size: (i32, i32),
    material_note: Option<String>,
    material_ok: Option<bool>,
    catalog: Option<&Catalog>,
    prefab_id: &str,
) -> BuildPreflight {
    let Some(r) = load_reach(session, catalog) else {
        return BuildPreflight {
            anchor, size, empty_cells: false, supported: false, work_position: None,
            material_ok, material_note, reasons: vec!["读不到世界模型".into()], ok: false,
        };
    };
    // ★★ 2026-11-22：**占格判定用实时 cell**，不用渲染网格。
    //   渲染网格（viewport.md）是**陈旧快照**：刚拆掉的口粮箱仍显示为 Building
    //   → 预检判「占格不是空的」→ 永远铺不上砖 → 厕所/发电机永远建不成。
    //   只查这 2~4 个占格 → 代价可忽略。
    let fp_cells = footprint(anchor.0, anchor.1, size.0, size.1);
    let live = crate::cell::fetch_many(session, &fp_cells);
    preflight_build_in(&r, anchor, size, material_note, material_ok, prefab_id, Some(&live))
}

/// 同一套预检，但复用**已经读好的**世界模型。
///
/// 为什么必须有这个版本：区域选点要扫几十上百个候选锚点，
/// 每次都重新读整张地图 + 官方可达区会把一次扫描拖到十几秒。
pub fn preflight_build_in(
    r: &Reach,
    anchor: (i32, i32),
    size: (i32, i32),
    material_note: Option<String>,
    material_ok: Option<bool>,
    prefab_id: &str,
    live: Option<&HashMap<(i32, i32), crate::cell::Cell>>,
) -> BuildPreflight {
    let grid = &r.grid;
    let dupe_cells = r.dupes.clone();
    let fp = footprint(anchor.0, anchor.1, size.0, size.1);
    let sup = support_cell(anchor.0, anchor.1);

    let mut reasons = Vec::new();
    let mut empty = true;
    for c in &fp {
        // ★ 实时优先（渲染网格可能陈旧）；读不到才退回网格。
        let live_kind = live.and_then(|m| m.get(c)).map(|cd| {
            if cd.has_building() {
                Kind::Building
            } else if cd.is_solid() {
                Kind::Solid
            } else if cd.is_liquid() {
                Kind::Liquid
            } else {
                Kind::Gas
            }
        });
        let kind = live_kind.unwrap_or_else(|| grid.kind(*c));
        match kind {
            Kind::Gas => {}
            // ★ 液体可以建：ONI 会把液体挤开。
            //   之前这里判"不是空的"→ 预检全灭 → 走廊里什么都建不了
            //   （实测人力发电机就建在污染水走廊里，游戏照单全收）。
            Kind::Liquid => {}
            // ★ 关键建筑保护区：打印舱（唯一新人来源）/ 人力发电机（唯一电源）
            //   压上去 = 直接断电或断人。
            Kind::Protected => { empty = false; reasons.push(format!("占格 {c:?} 在关键建筑保护区内（打印舱/发电机不能被压）")); }
            Kind::Dupe => { empty = false; reasons.push(format!("占格 {c:?} 上有复制人")); }
            k => { empty = false; reasons.push(format!("占格 {c:?} 不是空的（{k:?}）")); }
        }
    }
    // ★ 支撑必须覆盖**整个底边**：2 格宽的建筑要 2 格都有地板。
    //   只查锚点那一格 → 会挑中"悬空一半"的落点 → 游戏报 missing_support。
    let supports: Vec<(i32, i32)> = (0..size.0.max(1)).map(|dx| (anchor.0 + dx, anchor.1 - 1)).collect();
    let unsupported: Vec<(i32, i32)> = supports.iter().copied().filter(|c| !grid.is_support(*c)).collect();
    let supported = unsupported.is_empty();
    if !supported {
        reasons.push(format!("支撑格 {unsupported:?} 不实心 → 会报 missing_support"));
    }
    // ★ OnFloor 建筑只认**地板砖**当支撑（天然岩石不算）。
    //   mod 原文：Unsupported OnFloor building: place floor/support tiles below。
    //   旧预检把天然岩石也当支撑 → 选中其实不行的落点 → 落地必失败。
    let no_tile_supports: Vec<(i32, i32)> = supports
        .iter()
        .copied()
        .filter(|c| !grid.is_floor_tile(*c))
        .collect();
    // ★★ 2026-11-22 修正：**地板砖自己不需要"支撑是地板砖"**。
    //   旧逻辑对所有建筑都要求"支撑格是地板砖" → 给 Tile（砖块）做预检时变成**自指循环**：
    //   要铺砖得先有砖 → 代理永远铺不上砖 → 厕所/发电机/研究站的底边永远准备不出来。
    //   正解：只有**非 Tile 的 OnFloor 建筑**才要求支撑是地板砖；
    //     Tile 只要支撑是实心（岩/砖都行）即可。
    if !no_tile_supports.is_empty() && prefab_id != "Tile" {
        reasons.push(format!("支撑格 {no_tile_supports:?} 没有地板砖 → OnFloor 建筑可能报 missing_support"));
    }
    let mut wp = None;
    'outer: for c in &fp {
        for n in neighbors(c.0, c.1) {
            // ★ 2026-11-22 修正：官方 samples 是**稀疏子集**（实测每个复制人只列 12 个样本，
            //   实际可达 19~24 格）→ 只认 samples 会把合法落脚点判成「没人能来施工」。
            //   与挖掘预检同一口径：网格 BFS ∩ 官方包围盒 ∪ 爬梯可达集 ∪ 复制人当前格。
            let ok = r.set.contains(&n)
                || r.climbable.contains(&n)
                || dupe_cells.iter().any(|d| *d == n);
            if grid.walkable(n) && ok {
                wp = Some(n);
                break 'outer;
            }
        }
    }
    if wp.is_none() {
        reasons.push("附近没有复制人走得到的落脚格 → 没人能来施工".into());
    }
    if let Some(false) = material_ok {
        reasons.push("材料不足或材料不合法".into());
    }
    let ok = empty && supported && wp.is_some() && material_ok != Some(false);
    BuildPreflight {
        anchor, size, empty_cells: empty, supported, work_position: wp,
        material_ok, material_note, reasons, ok,
    }
}

/// **语义选点**：在给定区域内找一个能放下的落点。
///
/// 为什么需要它：计划里写死坐标在换一个存档就全废；
/// 而"在走廊里找个能放厕所的地方"这种语义，代理必须能自己解。
/// 评分：离最近的复制人越近越好（施工快、不占用远端资源），其次优先靠下（省材料）。
pub fn find_build_site(
    session: &McpSession,
    region: (i32, i32, i32, i32),
    size: (i32, i32),
    material_ok: Option<bool>,
    catalog: Option<&Catalog>,
    prefab_id: &str,
) -> Option<((i32, i32), BuildPreflight)> {
    let reach = load_reach(session, catalog)?;
    let dupes = dupe_positions(session);
    // ★★★ 2026-11-22 事故 15（回滚后 95 秒内 2 人全灭）：
    //   代理把 4 张床 + 2 个厕所建在 Y=189 的 CO2 聚集区 -> 复制人睡在 CO2 里
    //   -> 呼吸归零 -> 全灭（native step_facts 也报 population=0）。
    //   => **所有**自动建造的落点都必须可呼吸，不只是发电机。
    let cands = find_build_site_all(&reach, &dupes, region, size, material_ok, prefab_id, None);
    if cands.is_empty() {
        return None;
    }
    // 批量 fetch 所有候选的占格 -> 过滤掉危险落点（CO2 按浓度，液体/真空/毒气一律排除）
    let probes: Vec<(i32, i32)> = cands
        .iter()
        .flat_map(|((x, y), _, _)| footprint(*x, *y, size.0, size.1))
        .collect();
    let cells = fetch_many(session, &probes);
    let safe = |a: (i32, i32)| -> bool {
        footprint(a.0, a.1, size.0, size.1)
            .iter()
            // ★ None = 这一格**没读到** -> 保守排除（不能当安全）
            .all(|c| cells.get(c).map(|cd| !cd.is_safer_for_occupied()).unwrap_or(false))
    };
    // ★★ 2026-11-22：区域搜索的候选也必须用**实时**数据复核"占格为空"，
    //   否则网格说空、实际有建筑（刚拆的口粮箱仍显示为 Building）→ 选到坏落点。
    let live_cells = &cells;
    cands
        .into_iter()
        .find(|((x, y), _, _)| {
            let occupancy_ok = footprint(*x, *y, size.0, size.1).iter().all(|c| {
                match live_cells.get(c) {
                    Some(cd) => !cd.is_solid() && !cd.has_building(),
                    // 没读到 → 保守排除（宁可少一个候选，也不要建在不明物体上）
                    None => false,
                }
            });
            occupancy_ok && safe((*x, *y))
        })
        .map(|((x, y), _, pf)| ((x, y), pf))
}

pub fn find_build_site_in(
    r: &Reach,
    dupes: &[(i32, i32)],
    region: (i32, i32, i32, i32),
    size: (i32, i32),
    material_ok: Option<bool>,
    prefab_id: &str,
    live: Option<&HashMap<(i32, i32), crate::cell::Cell>>,
) -> Option<((i32, i32), BuildPreflight)> {
    find_build_site_all(r, dupes, region, size, material_ok, prefab_id, live)
        .into_iter()
        .next()
        .map(|((x, y), _, pf)| ((x, y), pf))
}

/// 列出区域内**所有**通过几何预检的落点，按评分从好到差排序。
///
/// ★ 2026-11-22：find_build_site 需要在候选里挑"可呼吸"的，
///   所以这里必须给出全部候选而不是单个最优。
pub fn find_build_site_all(
    r: &Reach,
    dupes: &[(i32, i32)],
    region: (i32, i32, i32, i32),
    size: (i32, i32),
    material_ok: Option<bool>,
    prefab_id: &str,
    live: Option<&HashMap<(i32, i32), crate::cell::Cell>>,
) -> Vec<((i32, i32), (i32, i64), BuildPreflight)> {
    let (x1, y1, x2, y2) = region;
    let mut all: Vec<((i32, i32), (i32, i64), BuildPreflight)> = Vec::new();
    for y in y1.min(y2)..=y1.max(y2) {
        for x in x1.min(x2)..=x1.max(x2) {
            let pf = preflight_build_in(r, (x, y), size, None, material_ok, prefab_id, live);
            if !pf.ok {
                continue;
            }
            let dist: i64 = dupes
                .iter()
                .map(|d| ((d.0 - x).abs() + (d.1 - y).abs()) as i64)
                .min()
                .unwrap_or(999);
            // 底边每格都有地板砖 → 0（优先）；否则 1
            // ★ OnFloor 建筑（发电机/旱厕/洗手盆）只认地板砖支撑，天然岩石不算。
            //   实测发电机连撞 10+ 次全失败：选点只按"离复制人近"排 →
            //   总挑到天然岩石支撑的位置 → 落地必失败 → 无限重试。
            let has_tile = (0..size.0.max(1))
                .all(|dx| r.grid.is_floor_tile((x + dx, y - 1)));
            let key = (if has_tile { 0i32 } else { 1i32 }, dist);
            all.push(((x, y), key, pf));
        }
    }
    all.sort_by_key(|(_, k, _)| *k);
    all
}

/// 这一列里复制人**真正站得到**的最低格（官方可达集）。
///
/// ★ 为什么必须用 official 而不是自算的 set：
///   自算 BFS 是**乐观**的（忽略"不能凭空往上爬"的规则），
///   会把够不到的格算成可达 —— 旧版梯子逻辑就栽在这：
///   梯子修在 (126,196)（自算可达、官方不可达）→ 复制人根本爬不上去 → 竖井永远挖不动。
pub fn column_base(reach: &Reach, x: i32, y: i32) -> Option<i32> {
    // ★★★ 2026-11-22 修正（本局实测：梯子永远铺不上 → 竖井一格都挖不动）：
    //   旧实现用 reach.climbable（= 官方可达集 ∪ 梯子链）找列基座，
    //   而**官方可达集是"半径受限的局部窗口"**
    //   （dupe 索引 radius=8、reachability.md radius=16），不是全局可达集
    //   → 基座常常不在窗口里 → ladder_step 返回 None → 梯子分支永不生效。
    //   正解：**用地图网格**（世界的真值）从目标正下方往下找：
    //     梯子 → 继续往下；空格且**脚下有地板** → 就是基座；实心岩 → 列到此为止。
    let mut base: Option<i32> = None;
    for yy in (y - 24..y).rev() {
        match reach.grid.kind((x, yy)) {
            Kind::Ladder => continue,
            Kind::Gas | Kind::Liquid | Kind::Dupe => {
                let below = (x, yy - 1);
                if reach.grid.is_support(below) || reach.grid.kind(below) == Kind::Ladder {
                    // 继续往下找 → 最终留下的是**最低**的站立格（列基座）
                    base = Some(yy);
                }
            }
            _ => break, // 实心岩 → 这一列到此为止
        }
    }
    base
}

/// **梯子列**：为了让目标 (x,y) 的正下方 (x,y-1) 可达，该把梯子铺在哪。
///
/// ★ ONI 的竖井标准做法（2026-11-21 实测钉死）：
///   梯子必须**从复制人站得到的那一格开始，连续往上铺**：
///     梯子(125,194) → 爬到 (125,195) → 梯子(125,195) → 爬到 (125,196) → 站上去挖头顶。
///   实测证据：连续铺 (125,194)+(125,195) 后，复制人成功踩梯子挖开了 (125,196)。
///
/// ★ 旧逻辑返回 (x, y-2)（目标的支撑格）—— 那一格复制人**根本站不到**
///   → 梯子修好了也没人能爬 → 竖井永远挖不动（实测：梯子修在 (126,196)，
///   而官方可达区只有 row 193..194）。
///
/// ★ 另一个实测事实：**官方 reachability 查询不反映"梯子可爬的格"**
///   （钉死：连续铺好梯子后，可达区仍显示 row 193..194）。
///   所以"能不能爬"只能靠**梯子链是否连续**判断，不能信可达集。
pub fn ladder_step(reach: &Reach, x: i32, y: i32) -> Option<(i32, i32)> {
    // 目标 (x,y) 的正下方 (x,y-1) 是空的但**不可达** → 需要梯子。
    let below = (x, y - 1);
    if !matches!(reach.grid.kind(below), Kind::Gas | Kind::Liquid | Kind::Dupe) {
        return None; // 正下方是实心 → 直接挖就行，不需要梯子
    }
    // ★★ 2026-11-22 修正（本局实测：梯子分支永不生效 → 竖井一格都挖不动）：
    //   旧代码用 reach.climbable（官方可达集 ∪ 梯子链）判断「已经能站上去 → 不需要梯子」。
    //   而官方可达集是**半径受限的局部窗口**、且**不反映梯子**
    //   → 它会说「能站上去」（其实没梯子，复制人根本爬不上来）
    //   → 梯子分支被整体跳过 → 代理只会下「死工单」。
    //   正解：**能不能爬只看梯子链**（格子里到底有没有梯子）。
    if reach.grid.kind(below) == Kind::Ladder {
        return None; // 正下方本身是梯子格 → 复制人能从下面爬上来
    }
    // ★ 从站得到的那一格开始，连续往上铺。
    //   ★★ 2026-11-22：链要**自下而上连续** —— 找出基座往上的第一处「缺梯子」的格，
    //     补上它；全都有梯子时，补目标正下方那一格。
    let base = column_base(reach, x, y)?;
    for yy in base..(y - 1) {
        if reach.grid.kind((x, yy)) != Kind::Ladder {
            return Some((x, yy)); // 最下面缺梯子的那格 → 先补它
        }
    }
    Some((x, y - 1))
}

/// 同一列在 [y_low, y_high] 之间有没有梯子（可攀爬的退路）。
fn column_has_ladder(grid: &Grid, x: i32, y_low: i32, y_high: i32) -> bool {
    (y_low..=y_high).any(|y| grid.kind((x, y)) == Kind::Ladder)
}

fn element_name(k: Kind) -> String {
    match k {
        Kind::Solid => "实心岩",
        Kind::Gas => "气体",
        Kind::Liquid => "液体",
        Kind::Building => "建筑",
        Kind::Ladder => "梯子",
        Kind::Protected => "保护区",
        Kind::Dupe => "复制人",
        Kind::Unknown => "未知",
    }
    .to_string()
}

pub fn format_cell(c: &(i32, i32)) -> String {
    format!("({},{})", c.0, c.1)
}
