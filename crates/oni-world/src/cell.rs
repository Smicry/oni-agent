use std::collections::HashMap;

use oni_mcp::McpSession;
use rayon::prelude::*;
use serde::Deserialize;

/// 单格信息（来自 `oni://world/cell/{x}/{y}`）。
#[derive(Debug, Clone, Deserialize)]
pub struct Cell {
    pub x: i32,
    pub y: i32,
    #[serde(default)]
    pub element: String,
    #[serde(default)]
    pub element_name: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub solid: bool,
    #[serde(default, rename = "massKg")]
    pub mass_kg: f64,
    #[serde(default)]
    pub buildings: Vec<CellBuilding>,
    /// 该格上的**标记**（挖掘 DigPlacer / 取消 / 清扫…）。
    ///
    /// 为什么需要：代理每轮取第一个前沿格 → 每轮重复对同一格下单，
    ///   既浪费又掩盖真实进展。
    #[serde(default)]
    pub designations: Vec<CellDesignation>,
}

/// 单个标记。
#[derive(Debug, Clone, Deserialize)]
pub struct CellDesignation {
    #[serde(default, rename = "prefabId")]
    pub prefab_id: String,
    #[serde(default, rename = "name")]
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CellBuilding {
    #[serde(default, rename = "prefabId")]
    pub prefab_id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default, rename = "anchorX")]
    pub anchor_x: i32,
    #[serde(default, rename = "anchorY")]
    pub anchor_y: i32,
    #[serde(default, rename = "operational")]
    pub operational: Option<bool>,
}

/// 单格气体达到这个质量（kg/格）才判 CO2 致命。
///
/// 校准：基地 Y=189~196 的 CO2 只有 ~1.15kg/格，
///   复制人站在里面**呼吸仍是 100**。
///   一刀切排除 CO2 会让发电选址全否 → 永远造不出发电机 → 基地永远没氧气 → 死锁。
pub const CO2_DANGER_KG_PER_CELL: f64 = 5.0;

impl Cell {
    /// 该格是否是"人待不得 / 挖不得"的危险格。
    ///
    /// 三类硬危险（与浓度无关）：
    ///   - 液体（污染水/污水/水）：淹死 + 污染扩散
    ///   - 真空：无法生存
    ///   - 污染氧 / 氯气：即使浓度低也有毒
    /// CO2：按**浓度**判。
    pub fn is_hazardous(&self) -> bool {
        if self.is_solid() {
            return false; // 实心岩是要挖的料，不是待不得的环境
        }
        let el = self.element.as_str();
        if matches!(
            el,
            "DirtyWater" | "污染水" | "SewageWater" | "污水" | "Water" | "水"
        ) {
            return true;
        }
        if matches!(el, "Vacuum" | "真空") {
            return true;
        }
        if matches!(el, "ContaminatedOxygen" | "污染氧" | "ChlorineGas" | "氯气") {
            return true;
        }
        if matches!(el, "CarbonDioxide" | "二氧化碳") {
            return self.mass_kg > CO2_DANGER_KG_PER_CELL;
        }
        false
    }

    /// 该格是否是**适合人长时间待在里面的**格子（比 is_hazardous 严格得多）。
    ///
    /// ：
    ///   基地 Y=189 的 CO2 只有 0.86~1.42 kg/格 —— 远低于致命阈值，
    ///   但**人待在里面呼吸会一路降到 77~93**（吉恩在发电机房里反复出现这个值），
    ///   长期待着 -> 归零 -> 死亡。
    ///   所以「人要待在里面」的落点（床/厕所/发电机/治疗设施）必须基本是纯氧区：
    ///   CO2 即使只有 1 kg/格也不算数。
    /// 而「路过/挖掘工作」的格子用宽松判据（is_hazardous）即可。
    pub fn is_safer_for_occupied(&self) -> bool {
        if self.is_solid() {
            return false; // 实心岩住不了人
        }
        let el = self.element.as_str();
        // element 为空 = **读不到数据**（batch fetch 漏格），
        //   不能当成"安全"—— AGENTS.md §13.6：读不到数据 ≠ 数据是极值。
        //   代理把床建在 (131,189)正是因为那格没读到元素，
        //   element 空 -> 判"安全" -> 严格筛选失效 -> 复制人睡在 CO2 里 -> 。
        if el.is_empty() {
            return false;
        }
        if matches!(el, "Oxygen" | "氧气" | "OxygenGas" | "PureOxygen") {
            return true;
        }
        if matches!(el, "CarbonDioxide" | "二氧化碳") {
            return self.mass_kg < 0.05; // 人待在里面会慢慢掉呼吸 -> 一律不行
        }
        false
    }

    /// P0-69：氧气质量检查 —— 区分"元素名是氧气"与"氧气够呼吸"。
    ///   竖井顶端 (127,215) 元素=Oxygen 但只有 0.046kg/格（近真空），
    ///   is_safer_for_occupied 只看元素名会误判安全 → 人爬上去窒息。
    ///   阈值 0.3kg：正常氧气格 1~3kg，低氧竖井 <0.1kg。读不到数据一律 false（保守）。
    pub fn has_breathable_mass(&self) -> bool {
        if self.element.is_empty() {
            return false;
        }
        if matches!(self.element.as_str(), "Oxygen" | "氧气" | "OxygenGas" | "PureOxygen") {
            return self.mass_kg >= 0.3;
        }
        false
    }

    pub fn is_solid(&self) -> bool {
        self.solid || self.state == "solid"
    }
    pub fn is_empty(&self) -> bool {
        !self.is_solid() && self.buildings.is_empty()
    }
    pub fn is_liquid(&self) -> bool {
        self.state == "liquid"
    }
    pub fn has_building(&self) -> bool {
        !self.buildings.is_empty()
    }
    /// 该格是否已有**挖掘标记**。
    pub fn has_dig_marker(&self) -> bool {
        self.designations
            .iter()
            .any(|d| d.prefab_id == "DigPlacer" || d.name.contains("挖掘"))
    }
}

/// 批量取格（并行）。这是预检的主要 IO 来源。
pub fn fetch_many(session: &McpSession, cells: &[(i32, i32)]) -> HashMap<(i32, i32), Cell> {
    cells
        .par_iter()
        .filter_map(|(x, y)| {
            let uri = format!("oni://world/cell/{x}/{y}");
            match session.read_json(&uri) {
                Ok(v) => match serde_json::from_value::<Cell>(v) {
                    Ok(c) => Some((*x, *y, c)),
                    Err(e) => {
                        eprintln!("[cell {x},{y}] 解析失败: {e}");
                        None
                    }
                },
                Err(e) => {
                    eprintln!("[cell {x},{y}] 读取失败: {e}");
                    None
                }
            }
        })
        .map(|(x, y, c)| ((x, y), c))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(element: &str, mass: f64) -> Cell {
        Cell {
            x: 0,
            y: 0,
            element: element.to_string(),
            element_name: String::new(),
            state: "gas".to_string(),
            solid: false,
            mass_kg: mass,
            buildings: Vec::new(),
            designations: Vec::new(),
        }
    }

    /// 的校准：**CO₂ 主导的格子，哪怕只有 0.25 kg/格，
    ///   人待在里面呼吸也会一路掉到 0** → 绝不能当"落脚点/居住落点"。
    ///   旧的 is_hazardous 按 5 kg/格 才算致命 → 漏放 → 代理把人派进去
    #[test]
    fn co2_dominant_cell_is_never_safe_to_stand_in() {
        let c = cell("CarbonDioxide", 0.25);
        assert!(
            !c.is_safer_for_occupied(),
            "CO₂ 主导的格哪怕只有 0.25kg/格，人待着也会掉呼吸 → 不能当落脚点"
        );
        // 宽松判据（"旁边能不能挖"）仍只看浓度 —— 但绝不能拿它当落脚点用
        assert!(
            !c.is_hazardous(),
            "低浓度 CO₂ 不算'挖不得'，但也绝不能当落脚点"
        );
    }

    #[test]
    fn oxygen_dominant_cell_is_safe_to_stand_in() {
        assert!(cell("Oxygen", 1.6).is_safer_for_occupied());
    }

    /// 读不到元素 → 保守判"不安全"。
    #[test]
    fn unknown_element_is_not_safe() {
        assert!(!cell("", 0.0).is_safer_for_occupied());
    }
}
