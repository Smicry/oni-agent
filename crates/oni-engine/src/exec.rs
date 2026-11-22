//! EXEC —— 执行动作。**预检不过 = 绝不执行**（这是三次“指令 ok 但没人干活”事故的拦截器）。

use std::collections::BTreeMap;

use oni_mcp::{Catalog, McpSession};
use oni_world::preflight::{preflight_build, preflight_dig};
use oni_world::rules::parse_points;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Action {
    Dig {
        rect: String,
        #[serde(default)]
        why: String,
        /// **锁定目标格**：语义动作指定的具体格子；选前沿格时优先它。
        /// 跨轮锁定避免"每轮重选最近目标→多列轮换→梯子浪费"。
        #[serde(default)]
        focus: Option<(i32, i32)>,
        /// 自动梯子列：目标是"挖头顶"且正下方不可达时，先铺梯子再挖。
        /// 这是 ONI 挖竖井的标准做法（人爬梯子列，一格格往上挖）。
        #[serde(default)]
        auto_ladder: bool,
    },
    Build {
        anchor: String,
        prefab: String,
        #[serde(default)]
        size: String,
        #[serde(default)]
        material: String,
        #[serde(default)]
        why: String,
    },
    Order {
        cmd: String,
        #[serde(default)]
        why: String,
    },
    /// **语义选点放置**：在给定区域内自动找一个合法落点再放建筑。
    ///
    /// 为什么需要：计划里写死坐标，换个存档就全废；
    /// 而"在走廊里找个能放厕所的地方"这种决策，本来就该由代理自己做。
    AutoBuild {
        prefab: String,
        /// 搜索区域 "x1,y1,x2,y2"
        region: String,
        #[serde(default)]
        material: String,
        #[serde(default)]
        why: String,
    },
    /// **接线**：把两个电力端点连起来。
    ///
    /// 为什么需要：
    ///   造出发电机**不等于**有电：电网需要导线连接两端。
    ///   发电机建成并运转，但
    ///   `consumerUsedWatts=0` —— 耗电建筑**没接上** → 研究站不工作
    ///   烤箱永远不解锁 → 食物耗尽
    ///   接线用 mod 的 `building_control domain=planning action=auto_connect`。
    AutoConnect {
        /// 电源端点名（如 "人力发电机"）
        from: String,
        /// 耗电端点名（如 "研究站"）
        to: String,
        #[serde(default)]
        why: String,
    },
    /// **语义化动作**：只说**意图**，坐标由运行时用图模块算出来。
    ///
    /// 为什么需要：plan.json 里写死坐标
    ///   换个存档就全废 —— 同一份计划不能跨存档复用。
    ///   `goal` 取值：`surface`（挖到地表）/ `element:Algae`（挖到含该元素的格）/
    ///   `mop_dirty`（排掉污染水/污染氧）。
    Auto {
        /// 目标：surface / element:XXX / mop_dirty
        goal: String,
        #[serde(default)]
        why: String,
    },
    /// **开局资源侦察**：探测关键资源分布并落盘（不改世界）。
    ///
    /// 为什么需要：
    ///   不知道铁矿在 20 格外就深挖 → 挖开地下 CO₂ → 三人。
    ///   先侦察再决定挖哪里，是唯一能避免这种灾难的顺序。
    Scout {
        #[serde(default)]
        why: String,
    },
    /// **直接调用任意 MCP 工具**：让依赖图能表达"领养打印好的新人"这类
    /// 不属于挖/建的动作。args 是 JSON 对象字符串。
    Call {
        tool: String,
        #[serde(default)]
        args: String,
        #[serde(default)]
        why: String,
    },
}

#[derive(Debug, Clone)]
pub struct ActionResult {
    pub kind: String,
    pub why: String,
    pub ok: bool,
    pub detail: String,
}

#[derive(Debug, Clone)]
pub struct ExecOutcome {
    pub results: Vec<ActionResult>,
    pub ok: bool,
}

impl ExecOutcome {
    fn stop(kind: &str, why: &str, detail: String) -> Self {
        ExecOutcome {
            results: vec![ActionResult { kind: kind.into(), why: why.into(), ok: false, detail }],
            ok: false,
        }
    }
}

/// **正在不可呼吸环境里的复制人所在格**。
///
/// 供挖掘管线的**所有**分支共用：
///   避人规则原先只写在"非梯子"分支里，而代理实际走的是梯子分支（挖竖井）
///   → 规则形同虚设 → 三人泡在污染水里，代理还在他们脚下开井。
/// 该目标格的**落脚点**是否是人能待的（可呼吸）环境。
///
/// （根因）：旧逻辑只检查"复制人当前所在格危不危险"，
///   **不检查落脚点本身** → 代理把人派进 CO₂ 主导的格子（只有 0.25~1.4 kg/格，
///   按旧的"CO₂ > 5kg 才致命"判据看着"不致命"）→ 人在那里呼吸一路掉到 0
///   → 倒地 → move_to 需要寻路、倒地的走不动 → 无人能救 → 三人。
///
/// 判据与"人要待在里面"的建筑落点一致（cell::is_safer_for_occupied）：
///   **主导元素必须是氧气**。mod 不提供 O₂ 分量（multicomponent 不可用），
///   所以只能用主导元素 —— 这是可得的最严判据。
///
/// 读不到格子数据 → 保守判"不可呼吸"，
///   并重试一次，避免偶发漏读误杀合法目标。
fn work_position_breathable(
    session: &McpSession,
    pf: &oni_world::preflight::DigPreflight,
    cell: (i32, i32),
) -> bool {
    let Some(wp) = pf
        .targets
        .iter()
        .find(|t| (t.x, t.y) == cell)
        .and_then(|t| t.work_position)
    else {
        return false;
    };
    let mut cells = oni_world::cell::fetch_many(session, &[wp]);
    if !cells.contains_key(&wp) {
        cells = oni_world::cell::fetch_many(session, &[wp]);
    }
    match cells.get(&wp) {
        Some(c) => c.is_safer_for_occupied(),
        None => false,
    }
}

pub fn danger_cells(session: &McpSession) -> Vec<(i32, i32)> {
    let dupes = oni_world::preflight::dupe_positions(session);
    if dupes.is_empty() {
        return Vec::new();
    }
    let cells = oni_world::cell::fetch_many(session, &dupes);
    dupes
        .into_iter()
        .filter(|c| {
            cells
                .get(c)
                .map(|cd| {
                    !cd.is_solid()
                        && !cd.is_empty()
                        && matches!(
                            cd.element.as_str(),
                            "DirtyWater"
                                | "CarbonDioxide"
                                | "ChlorineGas"
                                | "ContaminatedOxygen"
                                | "Vacuum"
                                | "污染水"
                                | "二氧化碳"
                                | "氯气"
                                | "污染氧"
                                | "真空"
                        )
                })
                .unwrap_or(false)
        })
        .collect()
}

/// 候选格是否**紧邻正在不可呼吸环境里的复制人**。
pub fn near_danger_cell(danger: &[(i32, i32)], c: (i32, i32)) -> bool {
    danger
        .iter()
        .any(|d| (d.0 - c.0).abs() <= 2 && (d.1 - c.1).abs() <= 2)
}

/// **死亡格**：挖开它会释放致命液体/气体（污染水/污染氧/真空）的格。
///
/// 为什么需要：
///   代理沿 Y=191 往西挖竖井，穿过污染层 → 竖井底整排变成污染水
///   基地生活区被 CO₂ 淹没 → 三人走进污染区 → 呼吸 0 → 倒地
///   根因：**挖开一格会释放那格的液体/气体**，而代理只检查"目标格本身"。
///   正解：挖之前先看**目标格正下方 1~2 格**是什么；
///   若是污染水/污染氧/真空 → 这格是死亡格 → 排除（换方向或先清理）。
/// **复制人脚下格**：目标格正下方 1~2 格是复制人当前所在格。
///
/// ：
///   旧规则只保护"正在不可呼吸环境里"的复制人（danger_cells），
///   注释还写着"复制人处在**正常**空气里时，在附近挖是安全的，不避" —— **这是错的**：
///   挖开复制人**脚下**的格子 → 人掉下去 → 掉进坑 / 水 / CO₂ 区 → 死亡。
///   代理挖地表竖井时把泡泡脚下掏空，泡泡随即消失。
///   规则：目标格正下方 1~2 格若是**任意**复制人所在格 → 排除该格。
pub fn dupe_below_cells(
    session: &McpSession,
    targets: &[oni_world::preflight::TargetCheck],
) -> Vec<(i32, i32)> {
    let dupes = oni_world::preflight::dupe_positions(session);
    let ts: Vec<(i32, i32)> = targets.iter().map(|t| (t.x, t.y)).collect();
    dupe_below_of(&dupes, &ts)
}

/// 纯逻辑版：目标格正下方 1~2 格有复制人 → 该目标格属于"脚下有人"。
///
/// 抽成纯函数是为了能单测（session 依赖没法在测试里造）。
pub fn dupe_below_of(dupes: &[(i32, i32)], targets: &[(i32, i32)]) -> Vec<(i32, i32)> {
    let mut below: Vec<(i32, i32)> = Vec::new();
    for t in targets {
        for dy in 1..=2i32 {
            let c = (t.0, t.1 - dy);
            if dupes.contains(&c) {
                below.push(c);
            }
        }
    }
    below.sort_unstable();
    below.dedup();
    below
}

/// 危险格判据（实现在 oni-world，便于 preflight 的建筑选点共用）。
///
/// ：代理把床/厕所建在 CO2 聚集区 -> 复制人睡在 CO2 里 -> 。
pub fn cell_is_hazardous(cd: &oni_world::cell::Cell) -> bool {
    cd.is_hazardous()
}

pub fn deadly_below_cells(
    session: &McpSession,
    targets: &[oni_world::preflight::TargetCheck],
) -> Vec<(i32, i32)> {
    // （）：只查**下方**是不够的 ——
    //   挖穿污染层的**顶部**会把污染水放下来，淹没目标格 → 跟着工单走过去干活的
    //   复制人直接淹死。
    //   ⇒ 上下 1~2 格都要查：挖任何一格都可能把相邻的污染层"打开"。
    let mut around: Vec<(i32, i32)> = Vec::new();
    for t in targets {
        for dy in -2..=2i32 {
            if dy == 0 {
                continue;
            }
            around.push((t.x, t.y + dy));
        }
    }
    around.sort_unstable();
    around.dedup();
    let cells = oni_world::cell::fetch_many(session, &around);
    around
        .into_iter()
        .filter(|c| {
            cells.get(c).map(|cd| {
                // 统一用共享判据（CO₂ 按**浓度**判，不按元素名一刀切）
                !cd.is_empty() && cell_is_hazardous(cd)
            }).unwrap_or(false)
        })
        .collect()
}

/// 自动腾空间：找一块 W×H 的候选地，返回**该挖的那一格**。
///
/// 为什么需要：
///   开局的打印舱占了 4×4，房间里只剩十几格 → 2×3 的厕所**根本没有现成落点**。
///   旧逻辑直接报「找不到落点」→ 节点暂缓 → 厕所永远建不出来，
///   而上一局正是「没厕所 → 膀胱爆 → 随地排泄 → 污染水 → 复制人溺死」。
///   ⇒ 找不到落点时应该**先挖出空间**，而不是放弃。
///
/// 规则：
///   - 占格只能是「气体」（可建）或「实心岩」（要先挖）；建筑/梯子/液体一律排除
///   - 底边必须有实心支撑（OnFloor 要求脚下有东西）
///   - 离复制人近的候选优先
///   - 挖哪一格要过**统一挖掘预检**（避人 / 死亡格 / 落脚点）→ 不自己拼工单
fn dig_space_for(
    session: &McpSession,
    catalog: Option<&Catalog>,
    region: (i32, i32, i32, i32),
    size: (i32, i32),
    dupes: &[(i32, i32)],
    building: &str,
) -> Option<((i32, i32), String)> {
    let reach = oni_world::preflight::load_reach(session, catalog)?;
    let (w, h) = size;
    let (rx1, ry1, rx2, ry2) = region;
    let mut cands: Vec<((i32, i32), i32)> = Vec::new();
    for x in rx1..=rx2 {
        for y in ry1..=ry2 {
            // 底边支撑：OnFloor 建筑脚下必须有实心或建筑
            let below = (x, y - 1);
            if !matches!(
                reach.grid.kind(below),
                oni_world::grid::Kind::Solid | oni_world::grid::Kind::Building
            ) {
                continue;
            }
            let mut need_dig: Vec<(i32, i32)> = Vec::new();
            let mut ok = true;
            for dy in 0..h {
                for dx in 0..w {
                    let c = (x + dx, y + dy);
                    match reach.grid.kind(c) {
                        oni_world::grid::Kind::Gas => {}
                        oni_world::grid::Kind::Solid => need_dig.push(c),
                        _ => {
                            ok = false;
                            break;
                        }
                    }
                }
                if !ok {
                    break;
                }
            }
            if !ok || need_dig.is_empty() {
                continue;
            }
            let dist = dupes
                .iter()
                .map(|(dx, dy)| (dx - x).abs() + (dy - y).abs())
                .min()
                .unwrap_or(i32::MAX);
            cands.push(((x, y), dist));
        }
    }
    cands.sort_by_key(|(_, d)| *d);
    for (anchor, _) in cands {
        // 占格里的实心岩：优先挖**最低**那格（复制人站在同一层就能挖，不用梯子）
        let mut solids: Vec<(i32, i32)> = Vec::new();
        for dy in 0..h {
            for dx in 0..w {
                let c = (anchor.0 + dx, anchor.1 + dy);
                if reach.grid.kind(c) == oni_world::grid::Kind::Solid {
                    solids.push(c);
                }
            }
        }
        solids.sort_by_key(|c| (c.1, c.0));
        // ★ 用**实时 cell 数据**复核候选格：reach.grid 是缓存网格，可能认为某格还是 Solid，
        //   但真实世界已经挖成气体（或已下挖掘工单）→ 旧逻辑反复对同一格下工单 → 厕所永远建不成。
        let live = oni_world::cell::fetch_many(session, &solids);
        for cell in solids {
            if live.get(&cell).map(|cd| cd.is_solid()).unwrap_or(true) == false {
                continue; // 实时已是气体/液体，不再挖
            }
            // 过统一挖掘预检（避人 / 死亡格 / 落脚点）→ 复用统一管线，不自己拼工单
            let pf = preflight_dig(session, &[cell], catalog);
            if !pf.ok || !pf.ready.contains(&cell) {
                continue;
            }
            return Some((
                cell,
                format!("为 {} 腾出空间：落点 ({},{}) 还差实心岩，先挖 ({},{})",
                    building, anchor.0, anchor.1, cell.0, cell.1
                ),
            ));
        }
    }
    None
}

pub fn execute(session: &McpSession, catalog: &Catalog, actions: &[Action]) -> ExecOutcome {
    let mut results = Vec::new();
    for a in actions {
        match a {
            Action::Dig { rect, why, auto_ladder, focus } => {
                let cells = match parse_points(rect) {
                    Ok(c) => c,
                    Err(e) => return ExecOutcome::stop("dig", why, e),
                };
                let pf = preflight_dig(session, &cells, Some(catalog));
                if !pf.ok {
                    let detail = pf
                        .targets
                        .iter()
                        .map(|t| format!("({},{}) {}{}", t.x, t.y, t.element,
                            if t.reasons.is_empty() { String::new() } else { format!(" — {}", t.reasons.join("; ")) }))
                        .collect::<Vec<_>>()
                        .join(" | ");
                    results.push(ActionResult { kind: "dig".into(), why: why.clone(), ok: false,
                        detail: format!("预检未通过，未执行: {detail}") });
                    return ExecOutcome { results, ok: false };
                }
                // 自动梯子列：目标是"挖头顶"但正下方不可达 → 先在可达的那一格铺梯子，
                //   复制人爬上去才能继续往上挖。没有这一步就永远挖不到头顶的矿层。
                // 缺梯子材料时的降级说明（不阻断横向挖掘）
                let mut ladder_note: Option<String> = None;
                // 锁定列走不通时**整轮不下单**（绝不横向换列）
                let mut locked_out = false;
                if *auto_ladder {
                    // 必须遍历**所有**需要梯子的前沿格，挑第一个"梯子落点确实可达"的。
                    //   旧代码用 find() 只取第一个：它的落点若不可达（ladder_step 返回 None），
                    //   整个 auto_ladder 分支就放弃 —— 哪怕后面还有能铺梯子的目标。
                    //   症状：日志一直显示"前沿 N 格，本轮挖 1 格"（走了非梯子分支），
                    //   梯子永远不铺，竖井永远挖不动。
                    let reach = oni_world::preflight::load_reach(session, Some(catalog));
                    let mut chosen: Option<((i32, i32), (i32, i32))> = None;
                    if let Some(r) = &reach {
                        // 收集所有"落点可达"的梯子候选，再按 focus 距离排序：
                        //   旧代码按 pf.targets 顺序取第一个 → 锁定的目标列被跳过
                        //   代理在 123/125/132/134 列之间乱挖，surface 锁形同虚设
                        //   → 换气节点永远打不通。
                        //   现在：离 focus 越近越优先；focus 列挖不动才降级到别的列。
                        let mut candidates: Vec<((i32, i32), (i32, i32))> = Vec::new();
                        // 避人规则在梯子分支**同样生效**：
                        //   旧代码只在非梯子分支避人 → 代理在三人泡着的污染水区
                        //   继续开井 → 呼吸掉到 87.5 → 差点重演/9。
                        let danger = danger_cells(session);
                        // 死亡格（）：梯子分支同样要排除
                        let mut deadly = deadly_below_cells(session, &pf.targets);
                        // ：复制人**脚下**也不能挖
                        deadly.extend(dupe_below_cells(session, &pf.targets));
                        // P0-69 低氧竖井拦截：批量读梯子候选格 + 其上方格的氧气质量。
                        //   实事故：梯子一路铺到 (127,215)，该处 Oxygen 仅 0.046kg/格（近真空），
                        //   复制人爬上去"寻找安全区域"、呼吸掉到 90 → 必须禁止引导人去低氧格。
                        //   读不到 → 保守跳过。批量 fetch 控制 MCP 压力。
                        let ladder_cells: Vec<(i32, i32)> = pf.targets.iter()
                            .filter(|t| t.needs_ladder && pf.ready.contains(&(t.x, t.y)))
                            .flat_map(|t| vec![(t.x, t.y), (t.x, t.y + 1)])
                            .collect();
                        let ladder_air = oni_world::cell::fetch_many(session, &ladder_cells);
                        for t in pf.targets.iter().filter(|t| t.needs_ladder && pf.ready.contains(&(t.x, t.y))) {
                            if near_danger_cell(&danger, (t.x, t.y)) {
                                continue;
                            }
                            if deadly.iter().any(|d| *d == (t.x, t.y)) {
                                continue;
                            }
                            // 目标格或其上方无足够氧气 → 不把人往低氧竖井顶引
                            let air_ok = ladder_air.get(&(t.x, t.y))
                                .or_else(|| ladder_air.get(&(t.x, t.y + 1)))
                                .map(|cd| cd.has_breathable_mass())
                                .unwrap_or(false);
                            if !air_ok {
                                continue;
                            }
                            if let Some(lp) = oni_world::preflight::ladder_step(r, t.x, t.y) {
                                candidates.push((lp, (t.x, t.y)));
                            }
                        }
                        if let Some(f) = focus {
                            // 同列硬锁：surface 目标锁定的是**列**，
                            //   不是某一格。旧逻辑按曼哈顿距离排序 → 选到"离焦点近但列不同"的
                            //   前沿格 → 代理在 115/116/118 列之间来回漂移，梯子成倍浪费，
                            //   竖井永远挖不到地表。
                            //   现在：只保留 focus 所在列的候选；该列完全走不通才降级到别的列。
                            let same_col: Vec<((i32, i32), (i32, i32))> = candidates
                                .iter()
                                .filter(|(_, t)| t.0 == f.0)
                                .cloned()
                                .collect();
                            if !same_col.is_empty() {
                                // 同列内：先挖低的（离基地近的），保证竖井自下而上连续
                                candidates = same_col;
                                candidates.sort_by_key(|(_, t)| t.1);
                            } else if !candidates.is_empty() {
                                // 修正（）：**有候选但都不在锁定列**
                                //   → 不换列（换列=梯子白铺）→ 本轮不下单。
                                //
                                // 但如果**本来就没有**需要梯子的候选（candidates 为空），
                                //   说明这一列的前沿格**不需要梯子**（正下方就是岩肩/实地，
                                //   例如沿实心岩肩的斜向阶梯）→ 应该交给下面的「横向挖前沿」分支，
                                //   绝不能整轮不下单（氧石支线因此一格没挖）。
                                candidates.clear();
                                locked_out = true;
                                ladder_note = Some(format!(
                                    "目标列 {} 暂时走不通 → 本轮不换列（等梯子/复制人消化）",
                                    f.0
                                ));
                            }
                            // candidates 为空 → 说明这一列的前沿格**不需要梯子**
                            //   （正下方就是岩肩/实地，例如沿实心岩肩的斜向阶梯）
                            //   → 交给下面的「横向挖前沿」分支，**绝不能整轮不下单**。
                            //   
                        }
                        if let Some(c) = candidates.first().copied() {
                            chosen = Some(c);
                        }
                    }
                    if let Some((lp, target)) = chosen {
                        let t = pf.targets.iter().find(|t| (t.x, t.y) == target).cloned();
                        let t = t.unwrap();
                        // 梯子材料：缺材料**不能阻断横向挖一格**。
                        //   梯子是"往上挖"的手段，不是"挖建材"的前提。
                        //   旧代码在这里直接失败 → 开局永远拿不到第一块砂岩 → 节点被误熔断。
                        let (mats, reason) = mod_available_materials(session, "Ladder");
                        if let Some(mat) = mats.first().cloned() {
                            // 逐级往上试：
                            //   内存网格是**快照**，可能滞后于真实地图 —— 那一格已经
                            //   有梯子了，落地必然失败。旧逻辑一失败就中止整个动作
                            //   代理冻回暂停 → 殖民地停摆。
                            //   正确做法：失败就**往上一级**继续试（梯子链本来就该连续），
                            //   全失败也**不阻断横向挖**（梯子只是"往上挖"的手段）。
                            let mut attempt = 0usize;
                            let mut rung = lp;
                            while attempt < 3 && rung.1 < t.y - 1 {
                                attempt += 1;
                                let token = crate::mapedit::build_token("Ladder", &mat);
                                let mut placements = BTreeMap::new();
                                placements.insert(rung, token);
                                let dry = match crate::mapedit::apply_build(session, &placements, true) {
                                    Ok(v) => v,
                                    Err(e) => {
                                        ladder_note = Some(format!("梯子 dry-run 失败: {}", e));
                                        break;
                                    }
                                };
                                if !dry.0 {
                                    ladder_note = Some(format!(
                                        "梯子 dry-run 未通过: {}",
                                        first_line(&dry.1).unwrap_or_default()
                                    ));
                                    rung = (rung.0, rung.1 + 1);
                                    continue;
                                }
                                match crate::mapedit::apply_build(session, &placements, false) {
                                    Ok((true, _out)) => {
                                        results.push(ActionResult { kind: "dig".into(), why: why.clone(), ok: true,
                                            detail: format!("竖井：先在 ({},{}) 铺梯子，再挖 ({},{}) 的头顶", rung.0, rung.1, t.x, t.y) });
                                        return ExecOutcome { results, ok: true };
                                    }
                                    Ok((false, out)) => {
                                        // 这一级放不下（很可能已经有一格梯子）→ 往上一级继续
                                        ladder_note = Some(format!(
                                            "梯子落地失败 @({},{}): {}",
                                            rung.0, rung.1, compact_error(&out)
                                        ));
                                        rung = (rung.0, rung.1 + 1);
                                    }
                                    Err(e) => {
                                        ladder_note = Some(format!("梯子落地失败 @({},{}): {}", rung.0, rung.1, e));
                                        rung = (rung.0, rung.1 + 1);
                                    }
                                }
                            }
                        } else {
                            // 缺梯子材料 → 记下来，继续走"挖前沿"分支（横向挖不需要梯子）
                            ladder_note = Some(format!(
                                "铺梯子缺建材（{}）→ 改走横向挖掘",
                                reason.as_deref().unwrap_or("无可用材料")
                            ));
                        }
                    }
                }
                // 锁定列走不通 → **整轮不下任何单**。
                //   旧逻辑会落到下面的"横向挖前沿"分支 → 换列挖 → 梯子白铺
                //   。
                if locked_out {
                    // 修正（）：**不要在这里提前返回**。
                    //   locked_out 的本意是「梯子分支不换列」，不是「整轮不下单」。
                    //   旧代码直接 return → 连下面的「横向挖前沿」分支也被禁掉
                    //   代理整轮空转（地表节点每轮都「锁定列走不通」，复制人全空闲）。
                    //   横向分支本身就有 **focus 优先级**（挖锁定格）→ 不会换列 ✓。
                    results.push(ActionResult {
                        kind: "dig".into(),
                        why: why.clone(),
                        ok: true,
                        detail: format!(
                            "锁定列没有需要梯子的前沿格 → 交给横向分支（仍挖锁定格）{}",
                            ladder_note.clone().map(|n| format!("：{n}")).unwrap_or_default()
                        ),
                    });
                }
                // 只下**前沿**（当前就有落脚点的格子）——阶梯必须一格一格长。
                // 绝不按 bbox 下单：bbox 会把没有落脚点的格子也标上，
                //   那些死标记会让 ONI 建不出差事，复制人就一直"有活但干不了"。
                let ready = pf.ready.clone();
                let mut issued = 0usize;
                let mut already = 0usize;
                // 优先挑**还没标记**的前沿格：
                //   旧逻辑每轮都取第一个前沿格 → 每轮重复对同一格下单，
                //   既浪费调用，又掩盖真实进展。
                //   全都有标记 = 已经下过单了，本轮就等复制人消化，不再重复下单。
                let marked: std::collections::HashSet<(i32, i32)> = pf
                    .targets
                    .iter()
                    .filter(|t| t.has_marker)
                    .map(|t| (t.x, t.y))
                    .collect();
                let fresh: Vec<(i32, i32)> = ready
                    .iter()
                    .copied()
                    .filter(|c| !marked.contains(c))
                    // 排除"需要梯子"的格：直上直下的竖井没有梯子爬不上去，
                    //   下去单只会变成死工单（ONI 立刻取消）。
                    .filter(|c| !pf.targets.iter().any(|t| (t.x, t.y) == *c && t.needs_ladder))
                    // （根因）：**工位的落脚点必须是人能呼吸的格**。
                    //   旧逻辑只看"复制人当前所在格危不危险"，不看落脚点本身
                    //   代理把人派进 CO₂ 主导的格子
                    //   人站在那里呼吸一路掉到 0 → 倒地 → 搬不动
                    //   判据与"人要待在里面"的建筑落点一致：主导元素必须是氧气。
                    //   （mod 不给 O₂ 分量：multicomponent 不可用 ⇒ 只能用主导元素。）
                    .filter(|c| work_position_breathable(session, &pf, *c))
                    .collect();
                // 避人规则：
                //   **不要在"身处不可呼吸环境的人"旁边开井**。
                //   ：代理沿竖井挖 195~201 层 → 地下 CO₂ 涌入低层走廊
                //   三个在 (121~132,192~193) 作业的复制人全部倒地、其中一人死亡。
                //   规则：复制人所在格若不可呼吸（污染水/CO₂/氯气/真空），
                //   则排除距他 2 格内的候选格 —— 换一列挖，别在人脚下开井。
                //   （复制人处在**正常**空气里时，**脚下**依然不能挖 —— 见 dupe_below_cells。
                // ：旧假设"正常空气里在附近挖是安全的"是错的，
                //    挖开脚下 → 人掉进坑/水 → 死亡。就是这么少了一个人。）
                // 避人规则：
                //   不要在"身处不可呼吸环境的人"旁边开井。
                //   ：代理沿竖井挖 195~201 层 → 地下 CO2 涌入低层走廊
                //   三个在 (121~132,192~193) 作业的复制人全部倒地、其中一人死亡。
                //   规则：复制人所在格若不可呼吸，
                //   则排除距他 2 格内的候选格 —— 换一列挖，别在人脚下开井。
                //
                // 新坑：这条规则原来**只作用于非梯子分支**，
                //   而代理实际走的正是梯子分支（挖竖井）→ 规则完全没生效：
                //   三人已泡在污染水里，代理还在他们脚下开井。
                //   现在抽成共享函数 danger_cells / near_danger_cell，
                //   **梯子分支与非梯子分支都要用**。
                let danger_cells = danger_cells(session);
                let near_danger = |c: (i32, i32)| -> bool {
                    near_danger_cell(&danger_cells, c)
                };
                // 死亡格规则：
                //   挖开一格会**释放该格周围的液体/气体**。
                //   代理沿 Y=191 往西挖竖井 → 挖穿污染层
                //   竖井底 (108~122,191) 全部变成污染水 → 基地生活区被 CO₂ 淹没
                //   三人先后走进污染区 → 呼吸 0 → 倒地
                //   规则：**目标格正下方 1~2 格若是污染/真空 → 排除该格**
        //   （挖开它就是打开一个死亡空间）。
                let mut deadly_below = deadly_below_cells(session, &pf.targets);
                // ：复制人**脚下**也不能挖
                deadly_below.extend(dupe_below_cells(session, &pf.targets));
                let near_deadly = |c: (i32, i32)| -> bool {
                    deadly_below.iter().any(|d| *d == c)
                };
                // 建材优先：优先挖能**补充建材**的格子（砂岩/泥土/藻类）。
                //   梯子约 100kg/格 —— 长竖井要十几格梯子，材料必须自给自足。
                //   挖开 1 格砂岩 = +700kg 地上建材 > 铺 1 格梯子的 -100kg → 净增。
                //   挖煤/铜则只出不进 → 梯子材料很快耗尽，竖井卡死。
                const REFILL: &[&str] = &["SandStone", "Dirt", "Algae", "Clay"];
                let mut best_idx: Option<usize> = None;
                let mut best_refill = false;
                let mut best_focus = false;
                for (i, c) in fresh.iter().enumerate() {
                    // 避人（）：跳过"身处不可呼吸环境的人"附近的格
                    if near_danger(*c) {
                        continue;
                    }
                    // 死亡格（，）：目标格下方是污染/真空 → 挖开即致命
                    if near_deadly(*c) {
                        continue;
                    }
                    let el = pf
                        .targets
                        .iter()
                        .find(|t| (t.x, t.y) == *c)
                        .map(|t| t.element.as_str())
                        .unwrap_or("");
                    let is_refill = REFILL.contains(&el);
                    // focus 优先：锁定目标格时只挖它，避免多列轮换
                    let is_focus = focus.map(|f| f == *c).unwrap_or(false);
                    let better = match best_idx {
                        None => true,
                        Some(_) => (is_focus && !best_focus)
                            || (is_focus == best_focus && is_refill && !best_refill),
                    };
                    if better {
                        best_idx = Some(i);
                        best_refill = is_refill;
                        best_focus = is_focus;
                    }
                }
                let pick = best_idx.map(|i| fresh[i]);
                // 每轮只挖**一格**：挖掉一格会抽走下一格的支撑（竖井就没地板了），
                //   一次挖一整批会把阶梯挖成无地板的竖井 → 没人能站 → 下一轮又挖不动。
                //   一次一格，支撑链才会自动长成**斜向阶梯**。
                for cell in pick.iter() {
                    let cmd = format!("挖 x1={} y1={} x2={} y2={} priority=7 confirm=true", cell.0, cell.1, cell.0, cell.1);
                    match run_order_raw(session, &cmd, false) {
                        Ok(out) => {
                            if out.contains("marked=0") {
                                already += 1;
                            } else {
                                issued += 1;
                            }
                        }
                        Err(e) => {
                            results.push(ActionResult { kind: "dig".into(), why: why.clone(), ok: false, detail: e });
                            return ExecOutcome { results, ok: false };
                        }
                    }
                }
                let note_prefix = ladder_note.map(|n| format!("[{}] ", n)).unwrap_or_default();
                // 把「视野内还有多少格挂着挖掘标记」写进结果：
                //   mod 下发的挖掘工单**不出现在 step_facts 的 remainingTrackedOrders 里**
                //
                //   代理只按 pending_orders 判"有没有活"→ 误判空跑 → 把世界冻回去
                //   → 那 5 张工单永远不会被执行。必须把"还有活"显式带出来。
                let outstanding = pf.targets.iter().filter(|t| t.has_marker).count();
                let out_note = if outstanding > 0 {
                    format!("；视野内还有 {} 格待完成", outstanding)
                } else {
                    String::new()
                };
                let detail = if pick.is_some() {
                    format!("{}{}前沿 {} 格（新 {} / 已有标记 {}），本轮挖 1 格：新标记 {}，已在位 {}",
                        note_prefix, out_note, ready.len(), fresh.len(), marked.len(), issued, already)
                } else {
                    format!("{}{}前沿 {} 格无可下单曲（全部需梯子或已有标记）→ 等梯子/复制人消化，本轮不下新单",
                        note_prefix, out_note, ready.len())
                };
                results.push(ActionResult { kind: "dig".into(), why: why.clone(), ok: true, detail });
            }
            Action::Build { anchor, prefab, size, material, why } => {
                let def = match catalog.find(prefab) {
                    Some(d) => d,
                    None => {
                        results.push(ActionResult { kind: "build".into(), why: why.clone(), ok: false,
                            detail: format!("目录里找不到 {prefab}（注意：目录必须 includeUnavailable 全量）") });
                        return ExecOutcome { results, ok: false };
                    }
                };
                let (w, h) = if size.is_empty() {
                    (def.width, def.height)
                } else {
                    let parts: Vec<i32> = size.split(|c: char| c == 'x' || c == ',' || c == '*')
                        .filter_map(|t| t.trim().parse().ok()).collect();
                    if parts.len() != 2 {
                        results.push(ActionResult { kind: "build".into(), why: why.clone(), ok: false,
                            detail: format!("尺寸格式错误: {size}") });
                        return ExecOutcome { results, ok: false };
                    }
                    (parts[0], parts[1])
                };
                let pts = match parse_points(anchor) {
                    Ok(p) => p,
                    Err(e) => return ExecOutcome::stop("build", why, e),
                };
                let a = pts[0];
                // 材料选择：目录的 availableMaterials（地上已有）→ defaultMaterials（推荐）。
                //   旧代码只用 available_materials.first()，开局它是**空数组**
                //   生成无材料 token → mod 报 phase: failed（建造被游戏拒绝）。
                // 材料来自 mod 权威查询：它按地上实际库存给出可用材料。
                //   目录的 defaultMaterials 只是首选一种；猜错了游戏会静默拒绝建造。
                let (mats, mat_reason) = mod_available_materials(session, &def.prefab_id);
                let mat = if !material.is_empty() {
                    material.clone()
                } else {
                    match mats.first() {
                        Some(m) => m.clone(),
                        None => {
                            let reason = mat_reason.as_deref().unwrap_or("无可用材料");
                            // 研究锁 vs 缺材料是两种完全不同的失败：
                            //   locked_by_research = 这个建筑**根本造不出来**（要研究解锁），
                            //   往计划里排它就是死路 → 必须换目标，而不是等材料。
                            let detail = if reason.contains("locked_by_research") {
                                format!(
                                    "✖ {} **未解锁**（mod 报告: {}）→ 这个建筑造不出来，需要研究站或科技；换目标",
                                    def.prefab_id, reason
                                )
                            } else {
                                format!(
                                    "✖ 造 {} 缺建材（mod 报告: {}）→ 先采建材再建造",
                                    def.prefab_id, reason
                                )
                            };
                            results.push(ActionResult { kind: "build".into(), why: why.clone(), ok: false, detail });
                            return ExecOutcome { results, ok: false };
                        }
                    }
                };
                let found = def.available_materials.iter().find(|m| m.tag == mat);
                let real_kg = loose_material_kg(session, &mat);
                let mat_ok = Some(real_kg > 0.0);
                let mat_note = Some(format!("{} 地上真实可用 {:.0}kg", found.map(|m| m.name.clone()).unwrap_or_else(|| mat.clone()), real_kg));
                let pf = preflight_build(session, a, (w, h), mat_note.clone(), mat_ok, Some(catalog), &def.prefab_id);
                if !pf.ok {
                    results.push(ActionResult { kind: "build".into(), why: why.clone(), ok: false,
                        detail: format!("预检未通过，未执行: {}", pf.reasons.join("; ")) });
                    return ExecOutcome { results, ok: false };
                }
                let token = crate::mapedit::build_token(&def.prefab_id, &mat);
                let mut placements = BTreeMap::new();
                placements.insert(a, token);
                match crate::mapedit::apply_build(session, &placements, true) {
                    Ok((true, _dry)) => match crate::mapedit::apply_build(session, &placements, false) {
                        Ok((true, out)) => results.push(ActionResult { kind: "build".into(), why: why.clone(), ok: true,
                            detail: format!("已放置 {} @({},{}) {}", def.prefab_id, a.0, a.1,
                                first_line(&out).unwrap_or_default()) }),
                        Ok((false, out)) => {
                            results.push(ActionResult { kind: "build".into(), why: why.clone(), ok: false,
                                detail: format!("落地失败: {}", first_line(&out).unwrap_or_default()) });
                            return ExecOutcome { results, ok: false };
                        }
                        Err(e) => {
                            results.push(ActionResult { kind: "build".into(), why: why.clone(), ok: false, detail: e });
                            return ExecOutcome { results, ok: false };
                        }
                    },
                    Ok((false, d)) => {
                        results.push(ActionResult { kind: "build".into(), why: why.clone(), ok: false,
                            detail: format!("dry-run 未通过: {}", first_line(&d).unwrap_or_default()) });
                        return ExecOutcome { results, ok: false };
                    }
                    Err(e) => {
                        results.push(ActionResult { kind: "build".into(), why: why.clone(), ok: false, detail: e });
                        return ExecOutcome { results, ok: false };
                    }
                }
            }
            Action::AutoBuild { prefab, region, material, why } => {
                // region == "auto"：**不写死坐标**，在复制人可达区附近自动找落点。
                //   这是同一份计划跨存档复用的关键 —— 换地图不用改计划。
                let def = match catalog.find(prefab) {
                    Some(d) => d,
                    None => {
                        results.push(ActionResult { kind: "build".into(), why: why.clone(), ok: false,
                            detail: format!("目录里找不到 {prefab}（目录必须 includeUnavailable 全量）") });
                        return ExecOutcome { results, ok: false };
                    }
                };
                let (w, h) = (def.width, def.height);
                // 先判**能不能造**（研究锁 + 材料），再造。
                //   旧顺序是"先查材料 → 造 → 落点失败" → 被研究锁的建筑
                //   会反复走到"找不到落点"这一步 → 反复失败 → 反复冻回。
                let mat = match buildability(session, &def.prefab_id) {
                    Buildability::Ready(m) => {
                        if !material.is_empty() {
                            material.clone()
                        } else {
                            m
                        }
                    }
                    Buildability::ResearchLocked(why) => {
                        results.push(ActionResult { kind: "build".into(), why: why.clone(), ok: false,
                            detail: format!(
                                "⛔ {} 被研究锁（{}）→ 造不了；换方案（研究完成 / 选无锁替代）",
                                def.prefab_id, why
                            ) });
                        return ExecOutcome { results, ok: false };
                    }
                    Buildability::MissingMaterial(why) => {
                        results.push(ActionResult { kind: "build".into(), why: why.clone(), ok: false,
                            detail: format!(
                                "✖ 造 {} 缺建材（mod 报告: {}）→ 先采建材再建造",
                                def.prefab_id, why
                            ) });
                        return ExecOutcome { results, ok: false };
                    }
                };
                let found = def.available_materials.iter().find(|m| m.tag == mat);
                let real_kg = loose_material_kg(session, &mat);
                let mat_ok = Some(real_kg > 0.0);
                let mat_note = Some(format!("{} 地上真实可用 {:.0}kg", found.map(|m| m.name.clone()).unwrap_or_else(|| mat.clone()), real_kg.max(0.0)));
                if let Some(false) = mat_ok {
                    results.push(ActionResult { kind: "build".into(), why: why.clone(), ok: false,
                        detail: format!("材料 {mat} 不可用（{mat_note:?}）") });
                    return ExecOutcome { results, ok: false };
                }
                // region == "auto"：不写死坐标，在复制人可达区附近自动找落点。
                let rect = if region.trim() == "auto" {
                    match auto_region(session, Some(catalog)) {
                        Some(r) => r,
                        None => {
                            results.push(ActionResult { kind: "build".into(), why: why.clone(), ok: false,
                                detail: "找不到复制人附近的建造区域".into() });
                            return ExecOutcome { results, ok: false };
                        }
                    }
                } else {
                    let cells = match parse_points(region) {
                        Ok(c) => c,
                        Err(e) => return ExecOutcome::stop("build", why, e),
                    };
                    bounds(&cells)
                };
                let Some((anchor, pf)) =
                    oni_world::preflight::find_build_site(session, rect, (w, h), mat_ok, Some(catalog), &prefab)
                else {
                    // 自动腾空间：
                    //   找不到落点**不等于**「没地方」 —— 常常只是**空间不够**（占格是实心岩）。
                    //   旧逻辑直接失败 → 节点被暂缓 → 厕所/床永远建不成；
                    //   而上一局就是「没有厕所 → 膀胱爆 → 随地排泄 → 污染水 → 溺死」。
                    //   新逻辑：找一块 W×H 的候选地（占格只能气体或实心岩、底边要有实心支撑），
                    //   把其中**一格实心岩**挖掉（过统一挖掘预检：避人/死亡格/落脚点），
                    //   下一轮再回来建造 → 空间自己长出来。
                    let dupes = oni_world::preflight::dupe_positions(session);
                    if let Some((cell, note)) =
                        dig_space_for(session, Some(catalog), rect, (w, h), &dupes, &def.name)
                    {
                        let cmd = format!(
                            "挖 x1={} y1={} x2={} y2={} priority=7 confirm=true",
                            cell.0, cell.1, cell.0, cell.1
                        );
                        match run_order_raw(session, &cmd, false) {
                            Ok(out) => {
                                results.push(ActionResult { kind: "build".into(), why: why.clone(), ok: true,
                                    detail: format!("{note} → 已下挖掘工单 @({},{})（{}）", cell.0, cell.1,
                                        first_line(&out).unwrap_or_default()) });
                                return ExecOutcome { results, ok: true };
                            }
                            Err(e) => {
                                results.push(ActionResult { kind: "build".into(), why: why.clone(), ok: false, detail: e });
                                return ExecOutcome { results, ok: false };
                            }
                        }
                    }
                    results.push(ActionResult { kind: "build".into(), why: why.clone(), ok: false,
                        detail: format!("{region} 内找不到能放 {} 的落点（占格/支撑/落脚点都不满足）", def.prefab_id) });
                    return ExecOutcome { results, ok: false };
                };
                let token = crate::mapedit::build_token(&def.prefab_id, &mat);
                let mut placements = BTreeMap::new();
                placements.insert(anchor, token.clone());
                match crate::mapedit::apply_build(session, &placements, true) {
                    Ok((true, _dry)) => match crate::mapedit::apply_build(session, &placements, false) {
                        Ok((true, out)) => results.push(ActionResult { kind: "build".into(), why: why.clone(), ok: true,
                            detail: format!("已在 ({},{}) 放置 {}（{}，材料 {}）{}", anchor.0, anchor.1,
                                def.prefab_id, def.name, mat,
                                first_line(&out).unwrap_or_default()) }),
                        Ok((false, out)) => {
                            results.push(ActionResult { kind: "build".into(), why: why.clone(), ok: false,
                                detail: format!("落地失败: {}", first_line(&out).unwrap_or_default()) });
                            return ExecOutcome { results, ok: false };
                        }
                        Err(e) => {
                            results.push(ActionResult { kind: "build".into(), why: why.clone(), ok: false, detail: e });
                            return ExecOutcome { results, ok: false };
                        }
                    },
                    Ok((false, d)) => {
                        results.push(ActionResult { kind: "build".into(), why: why.clone(), ok: false,
                            detail: format!("dry-run 未通过: {}", first_line(&d).unwrap_or_default()) });
                        return ExecOutcome { results, ok: false };
                    }
                    Err(e) => {
                        results.push(ActionResult { kind: "build".into(), why: why.clone(), ok: false, detail: e });
                        return ExecOutcome { results, ok: false };
                    }
                }
            }
            // 语义化动作：只说意图，坐标运行时算（跨存档复用的关键）
            Action::AutoConnect { from, to, why } => {
                // 接线：找两个端点的坐标 → auto_connect
                let (fx, fy) = match find_building_pos(session, &from) {
                    Some(p) => p,
                    None => {
                        results.push(ActionResult { kind: "wire".into(), why: why.clone(), ok: false,
                            detail: format!("找不到电力端点「{from}」") });
                        return ExecOutcome { results, ok: false };
                    }
                };
                let (tx, ty) = match find_building_pos(session, &to) {
                    Some(p) => p,
                    None => {
                        results.push(ActionResult { kind: "wire".into(), why: why.clone(), ok: false,
                            detail: format!("找不到电力端点「{to}」") });
                        return ExecOutcome { results, ok: false };
                    }
                };
                let out = session.call_text(
                    "building_control",
                    serde_json::json!({
                        "domain": "planning", "action": "auto_connect",
                        "fromX": fx, "fromY": fy, "toX": tx, "toY": ty,
                        "confirm": true,
                        "task": format!("代理: 接电线 {} → {}", from, to)
                    }),
                );
                match out {
                    Ok(txt) => {
                        let ok = txt.contains("\"ok\": true") || txt.contains("committed: true");
                        results.push(ActionResult { kind: "wire".into(), why: why.clone(), ok,
                            detail: format!("接线 {} ({},{}) → {} ({},{}): {}", from, fx, fy, to, tx, ty,
                                if ok { "已提交" } else { "失败" }) });
                        if !ok {
                            return ExecOutcome { results, ok: false };
                        }
                    }
                    Err(e) => {
                        results.push(ActionResult { kind: "wire".into(), why: why.clone(), ok: false, detail: e.to_string() });
                        return ExecOutcome { results, ok: false };
                    }
                }
            }
            Action::Auto { goal, why } => {
                let Some(g) = crate::autoact::parse_goal(goal) else {
                    results.push(ActionResult { kind: "auto".into(), why: why.clone(), ok: false,
                        detail: format!("无法解析的 goal: {goal}") });
                    return ExecOutcome { results, ok: false };
                };
                // 语义动作只负责"圈定目标区"，挖掘交给**统一执行管线**
                //   。
                //   旧版语义动作自己拼 raw 工单，绕过了全部这些规则
                //   死工单 + 重复下单 + 把好节点拖进熔断。
                let Some(action) = crate::autoact::compile(session, Some(catalog), g, why) else {
                    results.push(ActionResult { kind: "auto".into(), why: why.clone(), ok: false,
                        detail: format!("语义目标在当前地图上找不到（{}）→ 换方案", goal) });
                    return ExecOutcome { results, ok: false };
                };
                let sub = execute(session, catalog, &[action]);
                results.extend(sub.results);
                if !sub.ok {
                    return ExecOutcome { results, ok: false };
                }
            }
            Action::Scout { why } => {
                // 开局侦察：探测关键资源分布（不改世界），报告落盘 scout.json
                let report = crate::autoact::scout_resources(session, Some(catalog));
                results.push(ActionResult { kind: "scout".into(), why: why.clone(), ok: true, detail: report });
            }
            Action::Call { tool, args, why } => {
                let parsed: serde_json::Value = match serde_json::from_str(args) {
                    Ok(v) => v,
                    Err(e) => {
                        results.push(ActionResult { kind: "call".into(), why: why.clone(), ok: false,
                            detail: format!("args 不是合法 JSON: {e}") });
                        return ExecOutcome { results, ok: false };
                    }
                };
                let mut payload = parsed.as_object().cloned().unwrap_or_default();
                payload.entry("task").or_insert_with(|| serde_json::Value::String(format!("计划: {why}")));
                match session.call_text(tool, serde_json::Value::Object(payload)) {
                    Ok(out) => results.push(ActionResult { kind: "call".into(), why: why.clone(), ok: true,
                        detail: format!("{tool} → {}", first_line(&out).unwrap_or_default()) }),
                    Err(e) => {
                        results.push(ActionResult { kind: "call".into(), why: why.clone(), ok: false, detail: e.to_string() });
                        return ExecOutcome { results, ok: false };
                    }
                }
            }
            Action::Order { cmd, why } => {
                // 必须 dry-run → apply 两步都做（历史 bug：只做了 dry-run，指令从没落地）
                if let Err(e) = run_order(session, cmd, true) {
                    results.push(ActionResult { kind: "order".into(), why: why.clone(), ok: false, detail: format!("dry-run 失败: {e}") });
                    return ExecOutcome { results, ok: false };
                }
                match run_order(session, cmd, false) {
                Ok(d) => results.push(ActionResult { kind: "order".into(), why: why.clone(), ok: true, detail: d }),
                Err(e) => {
                    results.push(ActionResult { kind: "order".into(), why: why.clone(), ok: false, detail: e });
                    return ExecOutcome { results, ok: false };
                }
                }
            }
        }
    }
    ExecOutcome { results, ok: true }
}

/// **真实可达材料**：读 `oni://resources/inventory`。
///
/// 为什么不能用目录里的 `availableKg`：那是全局/陈旧值。
/// 开局砂岩目录显示 11782kg，实际地上 **0kg**，
/// 于是代理反复下"材料不够"的建造单 → 每轮都失败 → 看着像在跑、其实啥也没干。
pub fn loose_material_kg(session: &McpSession, element: &str) -> f64 {
    let Ok(v) = session.read_json("oni://resources/inventory") else {
        return 0.0;
    };
    v.get("items")
        .and_then(|i| i.as_array())
        .map(|a| {
            a.iter()
                .filter(|it| it.get("elementId").and_then(|x| x.as_str()) == Some(element))
                .filter_map(|it| it.get("totalMassKg").and_then(|m| m.as_f64()))
                .sum()
        })
        .unwrap_or(0.0)
}

/// 自动建造区域：以复制人可达区为中心，取一个覆盖它的矩形。
///
/// 为什么需要：plan.json 写死 `region: "124,194,137,197"` 换个存档就废。
///   这里从**当前复制人位置**实时算出一个合理区域 → 同一份计划跨存档复用。
fn auto_region(
    session: &McpSession,
    catalog: Option<&Catalog>,
) -> Option<(i32, i32, i32, i32)> {
    let reach = oni_world::preflight::load_reach(session, catalog)?;
    let mut x0 = i32::MAX;
    let mut y0 = i32::MAX;
    let mut x1 = i32::MIN;
    let mut y1 = i32::MIN;
    for (x, y) in &reach.dupes {
        x0 = x0.min(*x);
        y0 = y0.min(*y);
        x1 = x1.max(*x);
        y1 = y1.max(*y);
    }
    if x0 > x1 {
        return None;
    }
    // 往外扩 8 格，给建筑留落点空间
    Some((x0 - 8, y0 - 4, x1 + 8, y1 + 4))
}
fn bounds(cells: &[(i32, i32)]) -> (i32, i32, i32, i32) {
    let xs: Vec<i32> = cells.iter().map(|c| c.0).collect();
    let ys: Vec<i32> = cells.iter().map(|c| c.1).collect();
    (*xs.iter().min().unwrap(), *ys.iter().min().unwrap(), *xs.iter().max().unwrap(), *ys.iter().max().unwrap())
}



/// **研究 id 的优先级表**（ONI 基础研究树，按对殖民地的重要性排序）。
///
/// 为什么用固定表而不是动态查询：
///   mod **没有**暴露"可用研究列表"接口，
///   `management/research.md?format=json` 只有 active/target/queue 三项。
///   → 唯一可行办法是**按 id 逐个尝试**（不存在的 id 会回 "Research tech not found"）。
///   这张表就是"试哪些、按什么顺序"的知识沉淀。
///
/// 顺序含义：**先食物、再产氧、再基建**。
pub const RESEARCH_PRIORITY: &[&str] = &[
    // —— 食物链 ——
    "FarmingTech",      // 基础耕种 -> 藻类箱/种植箱/口粮箱/堆肥堆
    "FineDining",       // 食物制备 -> 电动烤箱
    "Agriculture",      // 农业 -> 农业站
    "RationBox",        // 口粮箱
    // —— 产氧 ——
    "ImprovedOxygen",   // 空气系统 -> 电解器
    "LiquidPiping",     // 水管（电解器的前置）
    "LiquidConduit",
    "AtmosphericAnalysis",
    // —— 基建/电力 ——
    "BasicPower",
    "PowerManagement",
    "AdvancedPower",
    "Batteries",
    // —— 居住/健康 ——
    "BasicFarming",
    "Hygiene",
    "BasicConstruction",
    "FineDining2",
];

/// 自动选一项研究（返回 research id）。
///
/// 为什么需要：研究是**唯一**能解锁关键设备的途径
///   （电动烤箱/电解器/农业站/藻类箱全部研究锁）。
///   手工点研究要重读文件 + 试探 id —— 应该由代理自己按优先级选。
///
/// 做法：按 `RESEARCH_PRIORITY` 顺序**逐个尝试**，第一个不报
/// "Research tech not found" 的就是它（失败原因会是"研究点不足"等）。
pub fn pick_research(session: &McpSession) -> Option<String> {
    // 已经 active / 已在队列里的 -> 不重复选
    if let Ok(v) = session.read_json("oni://research/status") {
        let active = v
            .get("active")
            .and_then(|a| a.get("id"))
            .and_then(|i| i.as_str())
            .unwrap_or("")
            .to_string();
        let queued: Vec<String> = v
            .get("queue")
            .and_then(|q| q.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.get("id").and_then(|i| i.as_str()).map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        if !active.is_empty() || !queued.is_empty() {
            return None; // 已有研究在跑
        }
    }
    for id in RESEARCH_PRIORITY {
        if research_id_exists(session, id) {
            return Some((*id).to_string());
        }
    }
    None
}

/// 某个 research id 在当前存档里是否存在（试写一次，读回是否成功）。
fn research_id_exists(session: &McpSession, id: &str) -> bool {
    let line = format!("research id=\"{}\" confirm=true", id);
    let res = session.call_text(
        "world_editor",
        serde_json::json!({
            "command": "edit", "path": "/active/management/research.md",
            "content": format!(
                "<<<<<<< SEARCH\n# research id=\"ImprovedOxygen\" confirm=true\n=======\n{}\n>>>>>>> REPLACE",
                line
            ),
            "confirm": true,
            "task": format!("代理: 探测研究 {id}")
        }),
    );
    let Ok(text) = res else { return false };
    // "Research tech not found" -> 这个 id 不存在
    !text.contains("Research tech not found")
}

/// 找已建成建筑的位置（按中文名）。走官方建筑索引。
pub fn find_building_pos(session: &McpSession, name: &str) -> Option<(i32, i32)> {
    let text = session.call_text(
        "world_editor",
        serde_json::json!({
            "command": "read", "path": "/active/buildings/index.md",
            "confirm": true, "task": format!("代理: 查建筑位置 {name}")
        }),
    ).ok()?;
    for line in text.lines() {
        let cols: Vec<&str> = line.split('|').map(|s| s.trim()).collect();
        if cols.len() < 4 {
            continue;
        }
        if cols[1] != name {
            continue;
        }
        let Some((xs, ys)) = cols[3].trim_start_matches('(').split_once(',') else {
            continue;
        };
        let (Ok(x), Ok(y)) = (xs.trim().parse::<i32>(), ys.trim().trim_end_matches(')').parse::<i32>()) else {
            continue;
        };
        return Some((x, y));
    }
    None
}

/// 建筑是否**已经接上电线**。
///
/// 为什么需要：mod 的电力诊断会逐个报
///   `ports: [{hasWire: false}]` → 没接线的耗电建筑**不会耗电**，
///   发电机却在发电→ 看起来一切正常，实际什么都没跑。
pub fn building_has_wire(session: &McpSession, name: &str) -> bool {
    let Ok(text) = session.read_json("oni://power/summary").map(|v| v.to_string()) else {
        return false;
    };
    // diagnostics.items[].name == 目标名 → 看它的 ports[].hasWire
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
        return false;
    };
    let items = v
        .get("diagnostics")
        .and_then(|d| d.get("items"))
        .and_then(|i| i.as_array())
        .cloned()
        .unwrap_or_default();
    for it in items {
        if it.get("name").and_then(|n| n.as_str()) != Some(name) {
            continue;
        }
        let ports = it.get("ports").and_then(|p| p.as_array()).cloned().unwrap_or_default();
        // mod 的诊断**只列有问题的建筑**：出现在这里且 hasWire=false → 没接上。
        //   没出现在诊断里 → 接线正常。
        if ports.iter().all(|p| p.get("hasWire").and_then(|w| w.as_bool()) != Some(true)) {
            return false;
        }
        return true;
    }
    // 不在诊断里 → 没被 mod 列为问题 → 视为已接（或不存在）
    true
}

/// 救援：把倒地的复制人移到**有氧气的落脚点**。
///
/// 为什么需要：
///   旧结论"mod 无 dupes_control、downed 不可救"**是错的**—— mod 有
///   `dupes_control domain=command action=move_to`，只是：
///     - **只能移到「对象名」**（打印舱/研究站/厕所/床铺…），不能传坐标；
///     - 用 `人@名字` 语义快捷键（`dupeName=` 传中文名会报 Duplicant not found）。
///   倒地的复制人若待在污染区会**必定死亡**；把他挪到干净的建筑旁边就能活。
///
/// **这个复制人是否需要救援**（倒地 **或** 在不可呼吸环境里呼吸持续下降）。
///
/// 这是 修复的**完整形态**。
///   之前只在 exec.rs 里扩宽了 rescue_dupes 的内部条件，但 agent.rs 的**调用门槛**
///   仍然是 `downed_count() > 0` → "呼吸在掉但没倒地"的人**根本进不了救援逻辑**，
///   扩展形同虚设。
///   ⇒ 判据必须**只有一份**，调用方与执行方共用。
pub fn dupe_needs_rescue(d: &crate::sense::Dupe) -> bool {
    let breath = d.breath.unwrap_or(0.0);
    d.is_downed() || (d.in_unbreathable() && breath < 95.0)
}

/// 返回 true = 已发出移动指令。
/// S3 执行后复核：**「已下令」不等于「已生效」**。
///
/// 倒地的复制人无法自行寻路 → 移动工单永远不会被执行 → 旧日志写「已挪动」是谎报，
/// 会让人以为救到了而不去建床 / 清毒气。
///
/// 做法：下达移动命令时**记下移动前的位置**；下一轮**回读当前位置**对比：
///   位置变了 → 真的挪到了；没变 → 如实报告「未生效（目标不可达 / 需要一张床）」。
#[derive(Debug, Clone)]
struct MoveAttempt {
    from: (i32, i32),
    to: String,
}

static MOVE_ATTEMPTS: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<String, MoveAttempt>>,
> = std::sync::OnceLock::new();

fn move_attempts() -> &'static std::sync::Mutex<std::collections::HashMap<String, MoveAttempt>> {
    MOVE_ATTEMPTS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// 读某个复制人当前所在格。
pub fn dupe_position_now(session: &McpSession, name: &str) -> Option<(i32, i32)> {
    let text = session.call_text(
        "world_editor",
        serde_json::json!({
            "command": "read", "path": "/active/dupes/index.oni",
            "confirm": true, "task": format!("复核: {name} 的位置")
        }),
    ).ok()?;
    let f = text.rfind("```json")?;
    let rel = text[f + 7..].find('{').map(|i| f + 7 + i)?;
    let end = text[rel..].rfind('}')?;
    let v: serde_json::Value = serde_json::from_str(&text[rel..=rel + end]).ok()?;
    let items = v.get("items")?.as_array()?;
    for it in items {
        let n = it.get("dupe").and_then(|d| d.get("name")).and_then(|x| x.as_str()).unwrap_or("");
        if n == name {
            let p = it.get("position")?;
            let x = p.get("x")?.as_i64()? as i32;
            let y = p.get("y")?.as_i64()? as i32;
            return Some((x, y));
        }
    }
    None
}

/// 记录一次移动尝试（移动前的位置 + 目标）。
fn note_move_attempt(name: &str, from: Option<(i32, i32)>, to: &str) {
    let Some(from) = from else { return };
    if let Ok(mut g) = move_attempts().lock() {
        g.insert(name.to_string(), MoveAttempt { from, to: to.to_string() });
    }
}

/// **复核上一轮的移动是否真的生效**。
///
/// 返回给日志的报告条目（生效 / 未生效）。
pub fn verify_moves(session: &McpSession) -> Vec<String> {
    let Ok(mut g) = move_attempts().lock() else { return Vec::new() };
    if g.is_empty() {
        return Vec::new();
    }
    let pending: Vec<(String, MoveAttempt)> = g.drain().collect();
    let mut out = Vec::new();
    for (name, att) in pending {
        match dupe_position_now(session, &name) {
            Some(now) if now != att.from => {
                out.push(format!("✅ 复核：{name} 已挪到 ({},{})（目标 {}）", now.0, now.1, att.to));
            }
            Some(_) => {
                out.push(format!(
                    "⚠️ 复核：{name} 仍在 ({},{}) —— 移动**未生效**（目标 {} 不可达 / 倒地的需要一张床）",
                    att.from.0, att.from.1, att.to
                ));
            }
            None => {
                out.push(format!("⚠️ 复核：{name} 读不到位置（可能已死亡）"));
            }
        }
    }
    out
}

pub fn rescue_dupes(session: &McpSession, dupes: &[crate::sense::Dupe]) -> Vec<String> {
    // 候选落脚点：基地里已建成、有唯一名字的建筑（mod 只能按名字寻址）
    // 目标顺序按"落点更可能有氧气"排：研究站在高处的新区，
    //   扩散器在产氧点旁；基地低层是 CO₂ 聚集区，最后才选。
    // 修正：
    //   旧顺序把**基地低层**排在前面
    //   → 倒地的复制人被「救」进 CO₂ 区 → 呼吸归零 → 死亡（救援反而成了杀手）。
    //   正解：**高处/富氧区的建筑优先**；低层建筑排到最后（宁可不动，也别送进毒气）。
    let targets = [
        "研究站",
        "氧气扩散器",
        "人力发电机",
        "煤炭发电机",
        "电解器",
        "藻类箱",
        "医疗床",
        "床铺",
        "打印舱",
        "户外厕所",
        "口粮箱",
    ];
    let mut moved = Vec::new();
    for d in dupes {
        // （）：不能只救"倒地" ——
        //   卡塔莉娜当时状态是**为建造挖掘**（没倒地），但人在污染水层里干活，
        //   呼吸一路归零 → 死亡。旧规则直接 continue → 代理看着她死。
        //   现在：倒地 **或**都要救。
        if !dupe_needs_rescue(d) {
            continue;
        }
        // 移动前先记位置（供下一轮复核「是否真的挪动了」）
        let from_pos = dupe_position_now(session, &d.name);
        for t in targets {
            // 落点必须是**富氧格**：先查建筑位置，再看该格是否氧气主导。
            // 旧逻辑只看"建筑存在就移"→ 把窒息的人从氧气梯子列送进 CO₂ 坑的藻类箱
            // （2026-11-22 裴/麦克斯呼吸反复掉到16就是这么来的）。读不到气体 → 保守跳过。
            if let Some((bx, by)) = find_building_pos(session, t) {
                let cells = oni_world::cell::fetch_many(session, &[(bx, by)]);
                let safe = cells.get(&(bx, by)).map(|c| c.is_safer_for_occupied()).unwrap_or(false);
                if !safe {
                    continue; // 该建筑在 CO₂/污染区 → 不把人往毒气里送
                }
            } else {
                continue; // 建筑不存在 → 跳过（旧逻辑靠 mod 报错来筛，这里提前筛）
            }
            let line = format!("移 人@{} -> {} confirm=true", d.name, t);
            let res = session.call_text(
                "world_editor",
                serde_json::json!({
                    "command": "edit",
                    "path": "/active/ops/dupes.md",
                    "content": format!(
                        "<<<<<<< SEARCH\n# call domain=info action=status_check\n=======\n{}\n>>>>>>> REPLACE",
                        line
                    ),
                    "confirm": true,
                    "task": format!("代理救援: 把 {} 移到 {}", d.name, t)
                }),
            );
            let ok = res.as_deref().map(|s| s.contains("\"ok\": true") || s.contains("ok: true")).unwrap_or(false);
            if ok {
                // mod 返回 ok 只代表**工单已下达**，不代表人真的到了。
                //   倒地的复制人无法自己寻路 → 移动工单永远不会被执行
                //   旧日志写「已挪动」是**谎报**，会让人以为救到了而不去建床/清毒气。
                //   正解：只说「已下令」，并点明倒地的需要一张床（ONI 的抬人 chore）。
                note_move_attempt(&d.name, from_pos, t);
                moved.push(format!(
                    "{} → {}（已下令）{}",
                    d.name,
                    t,
                    if d.is_downed() {
                        "；倒地者无法自行寻路 → 需要在富氧处有床，由同伴抬起"
                    } else {
                        ""
                    }
                ));
                break;
            }
        }
    }
    moved
}

/// **mod 权威的可用建材查询**：让 mod 自己算这个 prefab 当前能用哪些材料。
///
/// 为什么必须问 mod：
///   1) 目录的 `availableMaterials` 开局是**空数组**（地上没材料）→ 拿不到任何材料线索；
///   2) `defaultMaterials` 只是**首选一种**（梯子 = Isoresin），不是唯一合法材料；
///      梯子的 `resolvedMaterialCategories` = [BuildableRaw, BuildingWood]，藻类/木头都合法；
///   3) 自己猜材料 → token 被游戏拒绝，报错还看不出原因。
///   mod 会在地上有货时直接返回可用材料列表；没有时返回 autoMaterialReason。
pub fn mod_available_materials(
    session: &McpSession,
    prefab_id: &str,
) -> (Vec<String>, Option<String>) {
    let uri = format!("oni://buildings/materials?prefabId={prefab_id}");
    let Ok(v) = session.read_json(&uri) else { return (Vec::new(), None) };
    let materials: Vec<String> = v
        .get("materials")
        .and_then(|m| m.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|m| {
                    m.get("tag").or_else(|| m.get("id")).and_then(|t| t.as_str()).map(|s| s.to_string())
                })
                .collect()
        })
        .unwrap_or_default();
    let reason = v
        .get("autoMaterialReason")
        .and_then(|r| r.as_str())
        .map(|s| s.to_string());
    (materials, reason)
}

/// mod 报告的候选材料 + **地上真实可用量**。
///
/// 为什么不能只看 `materials[]` 的顺序：
///   mod 返回的 `materials[]` 是**目录顺序**，第一个往往地上 0kg
///   。
///   ⇒ 必须按 availableKg 排序，取第一个真有货的。
pub fn mod_materials_with_kg(
    session: &McpSession,
    prefab_id: &str,
) -> Vec<(String, f64)> {
    let uri = format!("oni://buildings/materials?prefabId={prefab_id}");
    let Ok(v) = session.read_json(&uri) else { return Vec::new() };
    v.get("materials")
        .and_then(|m| m.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|m| {
                    let tag = m
                        .get("tag")
                        .or_else(|| m.get("id"))
                        .and_then(|t| t.as_str())?
                        .to_string();
                    let kg = m.get("availableKg").and_then(|k| k.as_f64()).unwrap_or(0.0);
                    Some((tag, kg))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// **自举**：某个建筑造不了（缺材料）时，去挖它缺的那个材料。
///
/// 为什么必须有：
///   开局零铜零氧石 → 产氧节点每轮都失败 → 保命节点不暂缓 → **整轮空转**。
///   代理应该做的是"缺什么就去挖什么"，而不是反复尝试注定失败的建造。
///   ⇒ 返回 `element:<材料>` 语义目标，交给元素搜索去找最近的该元素格。
///
/// 只选**能挖到散装**的元素类材料（岩石/矿），不选需要特殊来源的。
pub fn bootstrap_material(
    session: &McpSession,
    prefab_id: &str,
    chinese_name: &str,
    near: Option<(i32, i32)>,
) -> Option<String> {
    const DIGGABLE: &[&str] = &[
        "OxyRock", "Cuprite", "IronOre", "Carbon", "Clay", "SandStone", "Dirt", "Algae", "Sand",
        "GoldAmalgam", "UraniumOre", "AluminumOre", "Cobalt", "Steel",
    ];
    let mats = mod_materials_with_kg(session, prefab_id);
    // 只考虑**地上真的没有**的——有货的不用挖
    let candidates: Vec<String> = mats
        .iter()
        .filter(|(_, kg)| *kg <= 0.0)
        .map(|(t, _)| t.clone())
        .filter(|t| DIGGABLE.contains(&t.as_str()))
        .collect();
    if candidates.is_empty() {
        return None;
    }
    // 修正：**按位置挑材料，不按目录顺序**。
    //   同一个建筑可能同时缺铜（深埋在下方，挖了会放出 CO₂/水袋）和
    //   铁矿（在地表附近，挖它顺带就通地表了）。
    //   正解：逐个搜最近位置，**优先选"在上方/浅"的**，下方重罚。
    let mut best: Option<(i32, String)> = None;
    for tag in &candidates {
        let found = oni_world::search::search_element(session, tag, near, 40);
        let Some(first) = found.first() else { continue };
        let (fx, fy) = *first;
        // 用户指令「别再往下挖了」→ **下方的材料直接排除**。
        //   下方 = 地下 CO₂ / 水袋 / 深坑。
        //   旧逻辑只是"重罚 1000 分"，但当**只有**下方材料时仍会去挖 → 违背指令。
        //   正解：下方材料不作为候选；若因此找不到材料 → 返回 None → 上层换方案。
        if let Some((_, ny)) = near {
            if fy < ny {
                eprintln!(
                    "[自举] {} 位于下方（y={} < 基准 {}）→ 按指令跳过，不挖",
                    tag, fy, ny
                );
                continue;
            }
        }
        let dist = match near {
            Some((nx, ny)) => (fx - nx).abs() + (fy - ny).abs(),
            None => 0,
        };
        let score = dist;
        if best.as_ref().map(|(s, _)| score < *s).unwrap_or(true) {
            best = Some((score, tag.clone()));
        }
    }
    let tag = best.map(|(_, t)| t).or_else(|| candidates.first().cloned())?;
    eprintln!("[自举] {} 造不了 → 去挖 {}", chinese_name, tag);
    Some(tag)
}

#[cfg(test)]
mod buildability_tests {
    use super::*;

    /// 研究优先级表：食物链必须排在产氧前面。
    #[test]
    fn research_priority_puts_food_first() {
        let p = RESEARCH_PRIORITY;
        let farming = p.iter().position(|x| *x == "FarmingTech").expect("应有 FarmingTech");
        let dining = p.iter().position(|x| *x == "FineDining").expect("应有 FineDining");
        let oxy = p.iter().position(|x| *x == "ImprovedOxygen").expect("应有 ImprovedOxygen");
        assert!(farming < dining, "基础耕种必须排在食物制备之前（否则烤箱解锁不了）");
        assert!(dining < oxy, "食物链必须排在产氧之前");
    }

    /// 被研究锁 vs 缺材料 —— 必须分开。
    ///   混为一谈 → 被研究锁的节点反复失败 → 反复冻回 → 殖民地反复停摆。
    #[test]
    fn research_lock_is_distinct_from_missing_material() {
        assert_eq!(
            classify_reason("building_locked_by_research"),
            Buildability::ResearchLocked("building_locked_by_research".into())
        );
        assert_eq!(
            classify_reason("no currently available material in this world/inventory"),
            Buildability::MissingMaterial(
                "no currently available material in this world/inventory".into()
            )
        );
        // 未知原因 → 当作缺材料（保守：可能只是暂时没货）
        assert!(matches!(
            classify_reason("something else"),
            Buildability::MissingMaterial(_)
        ));
    }
}

/// 建筑当前的**可建造性**判定。
///
/// 为什么需要：被研究锁的建筑（电动烤炉、电解器、
///   藻类箱…）即使材料齐全也造不出来 → 代理反复尝试 → 每次都失败
///   触发「动作失败 → 冻回暂停」→ 殖民地反复停摆。
///   必须在**动手前**分清"缺材料"（可去采）与"被研究锁"（只能等研究）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Buildability {
    /// 现在就能造（材料已就位）
    Ready(String),
    /// 缺建材 → 应该先去采材料
    MissingMaterial(String),
    /// 被研究锁 → 只能等研究完成（或换别的方案）
    ResearchLocked(String),
}

/// 查询建筑当前能不能造（材料 + 研究锁）。
///
/// 为什么锁定状态要单独查：
///   `oni://buildings/materials?prefabId=X` **不报告研究锁**——它照样返回
///   铜矿 1452kg（看起来材料齐全），但建筑其实被锁。
///   真正报锁定的是 `building_control domain=planning action=search_defs`：
///   它的 `autoMaterialReason` = `building_locked_by_research`、`unlocked: false`。
///   两个接口口径不一致 → 只看 materials 会误判"能造" → 反复失败。
pub fn buildability(session: &McpSession, prefab_id: &str) -> Buildability {
    // 1) 先查研究锁（权威口径）
    if let Some(lock) = research_lock_reason(session, prefab_id) {
        return Buildability::ResearchLocked(lock);
    }
    // 2) 再查材料
    let (mats, reason) = mod_available_materials(session, prefab_id);
    if let Some(m) = mats.first() {
        // 修正：mod 返回的「可用材料」是**目录顺序**，
        //   不是「地上真的有货」的顺序。建旱厕时取了目录第一个「火成岩」
        //   → 反复失败「材料不可用」→ 节点被暂缓。
        //   正解：按**地上真实可用量**排序，取第一个真有货的。
        let mut best: Option<(f64, String)> = None;
        for tag in &mats {
            let kg = loose_material_kg(session, tag);
            if kg > 0.0 && best.as_ref().map(|(k, _)| kg > *k).unwrap_or(true) {
                best = Some((kg, tag.clone()));
            }
        }
        return Buildability::Ready(best.map(|(_, t)| t).unwrap_or_else(|| m.clone()));
    }
    classify_reason(&reason.unwrap_or_default())
}

/// 查建筑是否被研究锁（返回锁定原因）。
///
/// 走 `building_control domain=planning action=search_defs`（按 prefabId 精确查），
/// 解析其中的 `autoMaterialReason` / `unlocked`。
pub fn research_lock_reason(session: &McpSession, prefab_id: &str) -> Option<String> {
    let text = session
        .call_text(
            "building_control",
            serde_json::json!({
                "domain": "planning", "action": "search_defs",
                "query": prefab_id, "includeUnavailable": true,
                "task": format!("代理: 查询 {prefab_id} 的研究锁状态")
            }),
        )
        .ok()?;
    // 必须**精确匹配 prefabId**，不能取第一个：
    //   search_defs 按 prefabId 查询时会返回**多个相似匹配**（前缀/包含关系），
    //   例如查 "ResearchCenter" 返回 AdvancedResearchCenter（研究锁）在前。
    //   旧代码取第一段 → 把未锁的研究站误判成"被研究锁" → 研究站永远建不成
    //   → 烤箱不解锁 → 食物链断裂 → 全局卡死。
    let want = format!("prefabId: {}", prefab_id);
    let mut seg: Option<&str> = None;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with(&want) {
            let rest = &text[text.find(line).unwrap_or(0)..];
            let end = rest[1..]
                .find("- prefabId:")
                .map(|i| i + 1)
                .unwrap_or(rest.len());
            seg = Some(&rest[..end]);
            break;
        }
    }
    let Some(seg) = seg else { return None };
    if seg.contains("autoMaterialReason: building_locked_by_research")
        || seg.contains("unlocked: false")
    {
        return Some("building_locked_by_research".to_string());
    }
    None
}

/// 把 mod 的 `autoMaterialReason` 分类成可建造性。
pub fn classify_reason(why: &str) -> Buildability {
    if why.contains("locked_by_research") || why.contains("research") {
        return Buildability::ResearchLocked(why.to_string());
    }
    Buildability::MissingMaterial(why.to_string())
}

/// 把 mod 的返回压成一行可读错误（保留 phase/error 字段，别只留首行）。
fn compact_error(out: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    for line in out.lines() {
        let l = line.trim();
        if l.is_empty() { continue; }
        if l.starts_with("phase:") || l.starts_with("error:") || l.starts_with("ok:")
            || l.starts_with("applied:") || l.starts_with("block:") {
            parts.push(l.to_string());
        }
        if parts.len() >= 4 { break; }
    }
    if parts.is_empty() { first_line(out).unwrap_or_default() } else { parts.join(" | ") }
}
pub fn first_line_pub(s: &str) -> Option<String> {
    first_line(s)
}

fn first_line(s: &str) -> Option<String> {
    s.lines().map(|l| l.trim()).find(|l| !l.is_empty()).map(|l| l.chars().take(120).collect())
}

/// 在 `/active/ops/orders.md` 的 Edit Commands 段里执行一条命令。
/// 规则：**一个编辑块只放一条可执行命令**；锚点用文件里的注释示例行。
pub fn run_order(session: &McpSession, cmd: &str, dry_run: bool) -> Result<String, String> {
    let full = run_order_raw(session, cmd, dry_run)?;
    Ok(first_line(&full).unwrap_or_else(|| "ok".into()))
}

/// 返回完整原始输出（调试用）。
pub fn run_order_raw(session: &McpSession, cmd: &str, dry_run: bool) -> Result<String, String> {
    let path = "/active/ops/orders.md";
    let text = session
        .call_text("world_editor", serde_json::json!({ "command": "read", "path": path, "confirm": true, "task": "循环: 读取运维文件" }))
        .map_err(|e| e.to_string())?;
    let sec = text.rfind("## Edit Commands").ok_or("运维文件里没有 Edit Commands 段")?;
    let rest = &text[sec..];
    let fence = rest.find("```text").ok_or("Edit Commands 段里没有 ```text 代码块")? + 7;
    let body = &rest[fence..];
    let end = body.find("```").ok_or("代码块没有闭合")?;
    let block = body[..end].trim_end();
    let first_word = cmd.split_whitespace().next().unwrap_or("").to_string();
    let anchor = block.lines()
        .find(|l| l.trim_start().starts_with('#') && l.contains(&first_word))
        .unwrap_or(block).to_string();
    let patch = format!("<<<<<<< SEARCH\n{anchor}\n=======\n{cmd}\n>>>>>>> REPLACE");
    let out = session
        .call_text("world_editor", serde_json::json!({
            "command": "edit", "path": path, "content": patch,
            "dryRun": dry_run, "confirm": !dry_run,
            "task": format!("循环: 执行订单 {cmd}")
        }))
        .map_err(|e| e.to_string())?;
    if out.contains("ok: false") {
        return Err(format!("订单执行失败: {}", first_line(&out).unwrap_or_default()));
    }
    Ok(out)
}
