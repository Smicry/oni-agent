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
    /// 该格上的标记（挖掘 DigPlacer / 取消 / 清扫…）。
    ///
    /// 用于优先挑没标记的格子，避免每轮对同一格重复下单。
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

/// 单格气体达到这个质量（kg/格）才把 CO2 判为危险格。
///
/// 校准：约 1.15kg/格 的 CO2 里复制人呼吸仍为 100。
/// 约束：不能一刀切排除 CO2，否则发电选址全被否决、造不出发电机。
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

    /// 该格是否适合人长时间待在里面（比 `is_hazardous` 严格得多）。
    ///
    /// 约束：人要待在里面的落点（床/厕所/发电机/治疗设施）必须是纯氧区——
    /// 约 1 kg/格 的 CO2 也会让呼吸持续下降，所以 CO2 即使浓度很低也不算数；
    /// 「路过/挖掘工作」的格子用宽松判据（`is_hazardous`）即可。
    pub fn is_safer_for_occupied(&self) -> bool {
        if self.is_solid() {
            return false; // 实心岩住不了人
        }
        // 液体格一律不住人：mod 的元素字段在"有薄层污水"时仍报主导气体为 Oxygen，
        // 只看元素名会把厕所/床建在污染水里（入口被淹，复制人不使用）。人要待的格子必须干燥。
        if self.is_liquid() {
            return false;
        }
        let el = self.element.as_str();
        // element 为空 = 读不到数据（batch fetch 漏格），
        //   不能当成"安全"—— AGENTS.md §9：读不到数据 ≠ 数据是极值。
        if el.is_empty() {
            return false;
        }
        if matches!(el, "Oxygen" | "氧气" | "OxygenGas" | "PureOxygen") {
            // 氧气还要够呼吸（质量 ≥0.3kg）：近真空格元素名也是 Oxygen，人爬上去窒息。
            return self.has_breathable_mass();
        }
        if matches!(el, "CarbonDioxide" | "二氧化碳") {
            return self.mass_kg < 0.05; // 人待在里面会慢慢掉呼吸 -> 一律不行
        }
        false
    }

    /// 氧气质量检查：区分"元素名是氧气"与"氧气够呼吸"。
    /// 阈值 0.3kg：正常氧气格 1~3kg，近真空格 <0.1kg（元素名也是 Oxygen，人爬上去窒息）。
    /// 读不到数据一律 false（保守）。
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

    /// CO₂ 主导的格子哪怕只有 0.25 kg/格，人待在里面呼吸也会掉到 0 → 不能当"落脚点/居住落点"。
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

    /// 液体格不住人，哪怕主导气体是氧气（mod 元素字段在薄层污水上仍报 Oxygen）。
    #[test]
    fn liquid_cell_is_never_safe_for_occupied_even_if_oxygen_dominant() {
        let mut c = cell("Oxygen", 1.6);
        c.state = "liquid".into();
        assert!(!c.is_safer_for_occupied(), "有液体的格子不能建厕所/床");
        // 近真空的氧气格也不能住人（元素名对但质量不够）
        assert!(!cell("Oxygen", 0.05).is_safer_for_occupied());
    }
}
