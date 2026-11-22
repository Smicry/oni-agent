//! oni-mcp —— 我们自己的 MCP（Model Context Protocol）客户端。
//!
//! 协议：MCP `2025-11-25` over Streamable HTTP。
//!
//! 设计要点（来自实战教训）：
//! * **持久 session**：握手一次、复用 `Mcp-Session-Id`，不要每次读数都重连。
//! * **容错**：连接失败自动重连；工具层错误与协议层错误分开表达。
//! * **不猜**：`read_text` 永远返回原始文本，解析交给上层；不做隐式降级。
//! * **目录必须全量**：`catalog` 永远带 `includeUnavailable=true`——默认接口只返回
//!   "当前可用"的子集，历史上因为拿它当全集，直接导致"这游戏没有电解器"的致命误判。

pub mod cache;
pub mod catalog;
pub mod gate;
pub mod save;
pub mod session;
pub mod transport;

pub use catalog::{BuildingDef, Catalog};
pub use gate::{Gate, GateGuard};
pub use session::McpSession;
pub use transport::{McpError, Transport, PROTOCOL_VERSION};

/// **代理状态目录**：`~/.oni-agent`。
///
/// 仓库改名后统一走这里；旧目录 `~/.oni-mcp` 首次访问时自动迁移，
/// 免得丢缓存与趋势样本。
pub fn state_dir() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let new = std::path::PathBuf::from(&home).join(".oni-agent");
    let old = std::path::PathBuf::from(&home).join(".oni-mcp");
    if !new.exists() && old.exists() {
        let _ = std::fs::rename(&old, &new);
    }
    new
}
