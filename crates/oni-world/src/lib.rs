//! oni-world —— 世界模型与**下单前预检**。
//!
//! 本 crate 把实战踩出来的硬规则固化成代码，而不是靠人眼判断：
//!
//! ## 坐标系（★ 最容易全盘搞错）
//! * mod 的 **Y 轴向上递增**：地图文件按 Y 降序打印，Y=219 在最上面（地表），Y=179 在最下面。
//! * **锚点 = 左下角 = Y 最小的那一行**；建筑从锚点**向上**生长，占 `(x..x+w-1, y..y+h-1)`。
//! * **支撑在 `y-1`**（物理意义上的下方）。例：床铺放 `(129,199)` → 占 rows 199..200，
//!   支撑是 row 198；放 `(129,200)` 会报 `missing_support`（它要 row 199 实心）。
//!
//! ## 可站立 / 工位可达
//! * **可站立** = 该格非实心 **且** 其 `y-1` 格实心。
//! * **工位可达** = 挖掘目标格至少有一个 8 邻居是"可站立"格。
//!   少了这一条就会复现历史事故：指令 `ok:true`，标记也在，但没人能走过去干活。

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
