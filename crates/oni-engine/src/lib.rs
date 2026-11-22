//! oni-engine —— 可持续游戏循环引擎（状态机）。
//!
//! ```text
//! SENSE → JUDGE → FREEZE → DECIDE → EXEC → VERIFY → RESUME → RECORD
//! ```
//!
//! 实战教训直接编码进结构，而不是留在注释里：
//!
//! * **SENSE 一次采全**：13 个源并行拉取，禁止只看单一数据源做判断。
//! * **JUDGE 用阈值表**：输出的是**判定**不是建议。
//! * **FREEZE/RESUME 必须配对**：暂停只用于紧急态与写世界；任何一次暂停都要有配对的恢复，
//!   且恢复前必须已经"验证有效"，否则就是让复制人白等死。
//! * **EXEC 前必过预检**：预检不过 → 换方案，绝不执行。
//! * **VERIFY 不过 = 没做成**：换方案，不是加轮次。

pub mod agent;
pub mod autoact;
pub mod decide;
pub mod live;
pub mod priority;
pub mod exec;
pub mod audit;
pub mod bus;
pub mod commands;
pub mod escalate;
pub mod judge;
pub mod planner;
pub mod saveswitch;
pub mod ledger;
pub mod mapedit;
pub mod sense;
pub mod stuck;
pub mod trend;

pub use agent::{Agent, AgentConfig, CycleReport};
pub use decide::{Node, Plan, Decision};
pub use exec::{Action, ActionResult, ExecOutcome};
pub use judge::{Judgment, Verdict};
pub use ledger::Ledger;
pub use sense::{Dupe, Snapshot};
pub use trend::{Trend, sample_path};
