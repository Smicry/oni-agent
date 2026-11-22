//! 并发闸门 + 过载退避：保护 mod 的 HTTP 端点不被压垮。
//!
//! 为什么需要：
//!   mod 的 MCP 端点在**几十个并发请求**下会直接返回 503。
//!   `oni graph --full` 逐格读全图时，单次查询就是几百个请求；
//!   多个查询叠加（守护进程 + 主 agent + 子代理）→ 每个查询 300+ 格读取失败。
//!   症状伪装成"地图数据缺失"，很容易误判成地图问题。
//!
//! 本模块提供：
//! - [`Gate`]：信号量，限制同时在飞的请求数。**跨进程共享**（文件锁槽位）。
//! - [`is_overload`]：判断错误是否为服务端过载（值得重试）。
//! - [`retry_delay`]：指数退避时长。

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// 默认最大并发请求数（**跨进程**共享）。
///
/// 取值理由：mod 端点是本地单进程 ONI。 8 并发在单进程内没问题，
/// 但 4 进程 × 8 = 32 并发就会大面积 503；降到 2 时 4 进程并发零失败。
/// 8 是"单进程够快、跨进程合计可能偏高"的折中；用 `ONI_MAX_CONCURRENT` 可调。
pub const DEFAULT_MAX_CONCURRENT: usize = 4;

/// 允许用环境变量覆盖。
pub fn max_concurrent_requests() -> usize {
    std::env::var("ONI_MAX_CONCURRENT")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_MAX_CONCURRENT)
}

/// 跨进程槽位文件：每个槽位记录持有者的时间戳，进程启动时抢占已过期的槽位。
fn slots_path() -> PathBuf {
    crate::state_dir().join("mcp_slots")
}

/// 信号量式并发闸门（**跨进程**）。
///
/// 进程内用 `Mutex<usize>` + `Condvar`；
/// 跨进程用**槽位文件**（每个槽位一个文件，写入持有者 PID + 时间戳）。
/// 槽位超过 TTL 未更新视为过期（持有者崩溃时自动释放）。
pub struct Gate {
    permits: Mutex<usize>,
    cv: Condvar,
    limit: usize,
    in_flight: AtomicUsize,
    /// 跨进程槽位目录
    slots: PathBuf,
    /// 槽位过期时长（秒）
    slot_ttl: u64,
}

impl Gate {
    pub fn new(limit: usize) -> Self {
        let slots = slots_path();
        let _ = std::fs::create_dir_all(&slots);
        Self {
            permits: Mutex::new(limit.max(1)),
            cv: Condvar::new(),
            limit: limit.max(1),
            in_flight: AtomicUsize::new(0),
            slots,
            slot_ttl: 120,
        }
    }

    /// 取得一个许可（阻塞直到有空位）。返回守卫，drop 时自动归还。
    pub fn acquire(&self) -> GateGuard<'_> {
        // 1) 进程内信号量
        let mut permits = self.permits.lock().unwrap_or_else(|e| e.into_inner());
        while *permits == 0 {
            permits = self
                .cv
                .wait(permits)
                .unwrap_or_else(|e| e.into_inner());
        }
        *permits -= 1;
        drop(permits);
        self.in_flight.fetch_add(1, Ordering::Relaxed);

        // 2) 跨进程槽位
        let slot = self.acquire_slot();
        GateGuard {
            gate: self,
            slot,
        }
    }

    /// 抢占一个跨进程槽位（阻塞重试直到成功）。
    fn acquire_slot(&self) -> Option<PathBuf> {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            for i in 0..self.limit {
                let p = self.slots.join(format!("slot-{i}.lock"));
                if self.try_claim(&p) {
                    return Some(p);
                }
            }
            if Instant::now() > deadline {
                return None;   // 拿不到就放弃跨进程限制（进程内限制仍在）
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// 尝试占用槽位：文件锁 + 陈旧检查。
    /// 尝试占用槽位。
    ///
    /// 用 **create_new（O_CREAT|O_EXCL）** 做原子抢占：
    /// 多个进程同时尝试同一槽位时，只有一个能创建成功 —— 这就是跨进程互斥。
    /// 持有者崩溃时槽位文件会残留 → 用 mtime 判断陈旧（超过 TTL）→ 删除后重抢。
    fn try_claim(&self, path: &PathBuf) -> bool {
        match OpenOptions::new().write(true).create_new(true).open(path) {
            Ok(mut f) => {
                let _ = f.write_all(format!("{}", std::process::id()).as_bytes());
                let _ = f.flush();
                true
            }
            Err(_) => {
                // 已存在：判断是否陈旧（持有者崩溃残留）
                let stale = std::fs::metadata(path)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() + self.slot_ttl < now_secs())
                    .unwrap_or(false);
                if !stale {
                    return false;
                }
                let _ = std::fs::remove_file(path);
                match OpenOptions::new().write(true).create_new(true).open(path) {
                    Ok(mut f) => {
                        let _ = f.write_all(format!("{}", std::process::id()).as_bytes());
                        let _ = f.flush();
                        true
                    }
                    Err(_) => false,
                }
            }
        }
    }

    /// 当前在飞的请求数（本进程）。
    pub fn in_flight(&self) -> usize {
        self.in_flight.load(Ordering::Relaxed)
    }

    pub fn limit(&self) -> usize {
        self.limit
    }
}

/// 许可守卫：drop 时归还许可并唤醒等待者。
pub struct GateGuard<'a> {
    gate: &'a Gate,
    slot: Option<PathBuf>,
}

impl Drop for GateGuard<'_> {
    fn drop(&mut self) {
        self.gate.in_flight.fetch_sub(1, Ordering::Relaxed);
        // 释放跨进程槽位
        if let Some(p) = &self.slot {
            let _ = std::fs::remove_file(p);
        }
        let mut permits = self.gate.permits.lock().unwrap_or_else(|e| e.into_inner());
        *permits += 1;
        self.gate.cv.notify_one();
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 判断错误是否为"服务端过载"（值得重试）。
pub fn is_overload(err: &str) -> bool {
    err.contains("503")
        || err.contains("502")
        || err.contains("504")
        || err.contains("429")
}

/// 指数退避时长：100ms → 200ms → 400ms ...。
pub fn retry_delay(attempt: u32) -> Duration {
    Duration::from_millis(100u64 << attempt.saturating_sub(1).min(4)).min(Duration::from_secs(2))
}