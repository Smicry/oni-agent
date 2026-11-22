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
pub const DEFAULT_TTL: Duration = Duration::from_secs(2);

/// 资源快照缓存。
pub struct SnapshotCache {
    entries: Mutex<HashMap<String, (Instant, String)>>,
    ttl: Duration,
}

impl SnapshotCache {
    pub fn new(ttl: Duration) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            ttl,
        }
    }

    /// 取缓存；未命中或已过期返回 None。
    pub fn get(&self, key: &str) -> Option<String> {
        let map = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        match map.get(key) {
            Some((at, v)) if at.elapsed() < self.ttl => Some(v.clone()),
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