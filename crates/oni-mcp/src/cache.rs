//! 快照缓存：短时间内重复读取同一资源直接复用，避免重复打 MCP。
//!
//! 为什么需要：
//!   守护进程每轮要读 11 个观察源，主 agent 的查询也在读同样的源 ——
//!   同一份数据在几秒内被反复拉取，既浪费又给 mod 端点加压。
//!   缓存让"同一时刻的地图/状态"只有一个真值来源。
//!
//! 设计取舍：
//! - **短 TTL**：只覆盖"同一轮内的重复读"，不跨越游戏状态变化。
//! - **按 key 存整份 JSON**：读一次缓存整份，避免部分命中带来的不一致。
//! - **写操作后失效**：任何变更类调用都会清空缓存（由上层显式调用）。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 默认 TTL：够覆盖一轮循环内的重复读，又不会让状态过期到误导决策。
///
/// 一轮 cycle（间隔 10s）里快系统会分多个阶段读同一批 cell
/// （danger_cells / deadly_below_cells / 建筑安全复核 / 实时复核）；
/// 8s 覆盖"一轮内的重复读"，又不跨越一轮的边界（状态变化仍会被写操作清空）。
pub const DEFAULT_TTL: Duration = Duration::from_secs(8);

/// 资源快照缓存。
pub struct SnapshotCache {
    entries: Mutex<HashMap<String, (Instant, String)>>,
    ttl: Duration,
    /// **per-URI TTL 覆盖**：关键状态用更短的新鲜度，其余用默认 TTL。
    ///
    /// 为什么需要（事件驱动查询融进 singleflight）：
    ///   世界运行/暂停状态（`oni://colony/status`）是**判定世界在不在跑**的唯一依据，
    ///   拿过期值会让代理在游戏已停时继续下工单 → 工单永不执行 → 判零进展 →
    ///   熔断好节点（契约 §2 不变量 1 禁止的"世界不跑却下工单"）。
    ///   而绕过缓存另开请求又会打爆 mod 端点 —— 所以**不绕过缓存**，
    ///   而是把关键 URI 的 TTL 调到秒级：
    ///   判定时（10s 轮询）缓存必然已过期 → 读到真值；
    ///   同一时刻的并发读仍由 singleflight 合并成一次请求 → 不打爆端点。
    ttl_overrides: Mutex<HashMap<String, Duration>>,
}

impl SnapshotCache {
    pub fn new(ttl: Duration) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            ttl,
            ttl_overrides: Mutex::new(HashMap::new()),
        }
    }

    /// 给某个 URI 设独立 TTL（秒级新鲜度）。
    pub fn set_ttl_override(&self, key: &str, ttl: Duration) {
        self.ttl_overrides
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key.to_string(), ttl);
    }

    /// 取该 key 生效的 TTL（覆盖优先）。
    fn ttl_for(&self, key: &str) -> Duration {
        self.ttl_overrides
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(key)
            .copied()
            .unwrap_or(self.ttl)
    }

    /// 取缓存；未命中或已过期返回 None。
    pub fn get(&self, key: &str) -> Option<String> {
        let ttl = self.ttl_for(key);
        let map = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        match map.get(key) {
            Some((at, v)) if at.elapsed() < ttl => Some(v.clone()),
            _ => None,
        }
    }

    /// 写缓存。
    pub fn put(&self, key: &str, value: &str) {
        let mut map = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        map.insert(key.to_string(), (Instant::now(), value.to_string()));
    }

    /// 清空缓存（任何状态变更后调用）。
    pub fn clear(&self) {
        let mut map = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        map.clear();
    }

    /// 当前缓存条目数（诊断用）。
    pub fn len(&self) -> usize {
        self.entries.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for SnapshotCache {
    fn default() -> Self {
        Self::new(DEFAULT_TTL)
    }
}