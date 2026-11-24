use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::session::McpSession;

/// 建筑定义（来自 `oni://buildings/defs`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildingDef {
    #[serde(rename = "prefabId")]
    pub prefab_id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub width: i32,
    #[serde(default)]
    pub height: i32,
    #[serde(default, rename = "buildLocationRule")]
    pub rule: String,
    #[serde(default, rename = "availableMaterials")]
    pub available_materials: Vec<MaterialDef>,
    /// 建造目录给出的**首选材料**（即使当前地上没有该材料也会列出）。
    ///
    /// 开局地上零建材时 `availableMaterials` 是空数组，无材料的 `Prefab:7` token 会被 mod 报 `phase: failed`；
    /// 所以用 `defaultMaterials`（例：梯子 = Isoresin 异构树液）作为目标材料，
    /// 再单独检查地上有没有它 —— 有材料才能造，没有就换方案。
    #[serde(default, rename = "defaultMaterials")]
    pub default_materials: Vec<String>,
    /// 该建筑接受的材料**类别**（mod 的 `materialCategories` 解析结果）。
    ///
    /// 例：梯子 = `["BuildableRaw", "BuildingWood"]` → 藻类/木头/砂岩等都属于合法材料。
    /// `defaultMaterials` 只是**首选**一种，实际可用的材料要看类别。
    #[serde(default, rename = "materialCategories")]
    pub material_categories: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MaterialDef {
    #[serde(default)]
    pub tag: String,
    #[serde(default)]
    pub name: String,
    #[serde(default, rename = "availableKg")]
    pub available_kg: f64,
}

/// 建造目录缓存。
///
/// 接口默认只返回"当前可用/已解锁"的子集，完整集合是 100+ 项（电解器、藻类箱、种植箱都在其中）；
/// 因此这里永远带 `includeUnavailable=true`，并落盘缓存。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Catalog {
    pub fetched_at: u64,
    pub buildings: Vec<BuildingDef>,
}

impl Catalog {
    pub fn path() -> PathBuf {
        crate::state_dir().join("catalog.json")
    }

    /// 取目录：优先缓存，否则拉取并落盘。
    pub fn load(session: &McpSession) -> Result<Catalog, String> {
        let path = Catalog::path();
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Ok(cat) = serde_json::from_str::<Catalog>(&text) {
                let age = SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0)
                    .saturating_sub(cat.fetched_at);
                // 目录缓存 TTL = 30 分钟：目录会随研究解锁/换存档而变，缓存跨局复用会整批缺建筑
                //   （preflight 把 Generator 模糊匹配成 DevGenerator(1×1) → 落点判断全错）。
                if age < 30 * 60 && !cat.buildings.is_empty() {
                    return Ok(cat);
                }
            }
        }
        // 两个查询取并集：
        //   1) 默认查询 = 当前可用/已解锁的那批
        //   2) includeUnavailable=true = 另一批，
        //      但它按字母序截断（只到 G/L），所以单靠它会丢基础建筑。
        // 只看被过滤的子集会把已锁定的建筑（如电解器）误判为不存在。
        let mut merged: Vec<BuildingDef> = Vec::new();
        for uri in [
            "oni://buildings/defs?includeUnavailable=true&limit=1000",
            "oni://buildings/defs?limit=1000",
        ] {
            let raw = match session.read_text(uri) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("[catalog] {uri} 读取失败: {e}");
                    continue;
                }
            };
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
                continue;
            };
            let list = value
                .get("buildings")
                .or_else(|| value.get("results"))
                .cloned()
                .unwrap_or(serde_json::json!([]));
            let Ok(mut batch) = serde_json::from_value::<Vec<BuildingDef>>(list) else {
                continue;
            };
            eprintln!("[catalog] {} → {} 项", uri, batch.len());
            merged.append(&mut batch);
        }
        // 按 prefab_id 去重合并
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let buildings: Vec<BuildingDef> = merged
            .into_iter()
            .filter(|b| seen.insert(b.prefab_id.clone()))
            .collect();
        let cat = Catalog {
            fetched_at: SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            buildings,
        };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&path, serde_json::to_string_pretty(&cat).unwrap_or_default());
        Ok(cat)
    }

    pub fn get(&self, prefab_id: &str) -> Option<&BuildingDef> {
        self.buildings.iter().find(|b| b.prefab_id == prefab_id)
    }

    /// 按中文名/预制体名模糊查找（命令行里人用的是中文名）。
    pub fn find(&self, query: &str) -> Option<&BuildingDef> {
        let q = query.to_lowercase();
        self.buildings
            .iter()
            .find(|b| b.prefab_id.to_lowercase() == q)
            .or_else(|| self.buildings.iter().find(|b| b.name.contains(query)))
            .or_else(|| self.buildings.iter().find(|b| b.prefab_id.to_lowercase().contains(&q)))
    }

    /// 建造某 prefab 时应当使用的材料标签。
    ///
    /// 优先用调用方指定的；否则用目录的 `availableMaterials`（地上已有的）；
    /// 再否则用 `defaultMaterials`（目录推荐但地上可能没有）。
    /// 返回 None = 目录没给任何材料线索（例如梯子这类 Anywhere 规则建筑仍需要建材）。
    pub fn material_for(&self, prefab_id: &str, requested: &str) -> Option<String> {
        let def = self.get(prefab_id)?;
        if !requested.is_empty() {
            return Some(requested.to_string());
        }
        if let Some(m) = def.available_materials.first() {
            return Some(m.tag.clone());
        }
        def.default_materials.first().cloned()
    }

    /// 该 prefab 接受哪些材料**类别**（`BuildableRaw` / `BuildingWood` …）。
    /// 目录里 `materialCategories` 可能缺失，此时退回默认材料。
    pub fn material_categories(&self, prefab_id: &str) -> Vec<String> {
        let Some(def) = self.get(prefab_id) else { return Vec::new() };
        if !def.material_categories.is_empty() {
            return def.material_categories.clone();
        }
        // 退化：把默认材料本身当类别（只匹配它自己）
        def.default_materials.clone()
    }

    pub fn stale_after(&self, dur: Duration) -> bool {
        let age = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
            .saturating_sub(self.fetched_at);
        age > dur.as_secs()
    }
}
