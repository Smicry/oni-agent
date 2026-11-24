//! oni-world —— 世界模型与下单前预检。
//!
//! ## 坐标系
//! * mod 的 Y 轴向上递增：地图文件按 Y 降序打印。
//! * 锚点 = 左下角 = Y 最小的那一行；建筑从锚点向上生长，占 `(x..x+w-1, y..y+h-1)`。
//! * 支撑在 `y-1`（物理下方）；支撑不成立时 mod 报 `missing_support`。
//!
//! ## 可站立 / 工位可达
//! * 可站立 = 该格非实心 且 其 `y-1` 格实心。
//! * 工位可达 = 挖掘目标格至少有一个 8 邻居是"可站立"格。
//!   约束：缺了这一条，指令 `ok:true`、标记也在，但没人能走过去干活。

pub mod cell;
pub mod search;
pub mod graph;
pub mod grid;
pub mod preflight;
pub mod rules;

pub use cell::{Cell, CellBuilding};
pub use graph::{Chamber, DigRoute, MapGraph};
pub use grid::{Grid, Kind};
pub use preflight::{BuildPreflight, DigPreflight, TargetCheck};
pub use rules::*;
