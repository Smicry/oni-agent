use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};

use serde_json::{json, Value};

use crate::cache::SnapshotCache;
use crate::transport::{McpError, Transport, PROTOCOL_VERSION};

/// 一个已握手的 MCP 会话。可跨线程共享（内部用 Mutex/Atomic 保证安全）。
pub struct McpSession {
    t: Transport,
    /// 资源快照缓存：短时间内重复读同一 URI 直接复用。
    ///
    /// 写入类调用（call_tool）会自动清空缓存 —— 状态变了，旧快照就是错的。
    cache: SnapshotCache,
    /// ★ singleflight：同 URI 的并发读合并成一次（MCP 请求按 uri 合并，见 transport 说明）。
    ///
    /// 为什么需要：cell 事实被快系统多个阶段反复读、且快慢两线程会同时读同一格
    ///   → 几百个重复 HTTP 请求 → mod 端点排队 → 60s 超时 → 周期空窗。
    inflight: Mutex<HashMap<String, Arc<InflightSlot>>>,
}

/// singleflight 槽位：一次请求的结果 + 完成信号。
type InflightSlot = (Mutex<Option<Result<Value, String>>>, Condvar);

impl McpSession {
    pub fn connect(endpoint: &str) -> Result<Self, McpError> {
        let t = Transport::new(endpoint);
        t.request(
            "initialize",
            Some(json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": "oni-agent", "version": "0.1.0" }
            })),
        )?;
        t.notify("notifications/initialized", None);
        let cache = SnapshotCache::default();
        // ★ 关键状态走**秒级新鲜度**（事件驱动查询融进 singleflight）：
        //   世界运行/暂停是"判定世界在不在跑"的唯一依据，拿过期值会让代理
        //   在游戏已停时继续下工单 → 工单永不执行 → 判零进展 → 误熔断好节点。
        //   不绕过缓存（那会打爆 mod 端点），而是把这条 URI 的 TTL 调到 1s：
        //   判定时缓存必然已过期 → 读到真值；同一时刻并发读仍由 singleflight 合并。
        cache.set_ttl_override("oni://colony/status", std::time::Duration::from_secs(1));
        Ok(Self { t, cache, inflight: Mutex::new(HashMap::new()) })
    }

    pub fn call_tool(&self, name: &str, args: Value) -> Result<Value, McpError> {
        // 工具调用可能改变世界 → 缓存立即失效（宁可多读一次，也不能用过期快照决策）
        self.cache.clear();
        self.t.request("tools/call", Some(json!({ "name": name, "arguments": args })))
    }

    pub fn read_resource(&self, uri: &str) -> Result<Value, McpError> {
        // 快照复用：同一 URI 在 TTL 内只打一次 MCP。
        //   守护进程每轮读 11 个观察源，主 agent 的查询也在读同样的源 ——
        //   没有缓存时同一份数据在几秒内被反复拉取，既浪费又给 mod 端点加压。
        if let Some(v) = self.cache.get(uri) {
            if let Ok(parsed) = serde_json::from_str::<Value>(&v) {
                return Ok(parsed);
            }
        }
        // ★ singleflight：同 URI 的**并发**读合并成一次请求，其余线程等共享结果。
        //   为什么需要：cell 事实（元素/质量/建筑/标记）被快系统多个阶段反复读
        //   （danger_cells / deadly_below_cells / 建筑安全复核 / 实时复核），
        //   快慢两线程还会同时读同一格 → 几百个重复 HTTP 请求 → mod 端点排队
        //   → 60s 超时 → 周期空窗 89 秒（实测多次）。
        //   合并后同一时刻每个 URI 只打一次，其余线程直接拿结果。
        {
            let mut map = self.inflight.lock().unwrap_or_else(|e| e.into_inner());
            let existing = map.get(uri).cloned();
            if existing.is_none() {
                // 我是第一个 → 登记槽位并去发请求
                map.insert(uri.to_string(), Arc::new((Mutex::new(None), Condvar::new())));
            }
            drop(map);
            let slot = match existing {
                Some(s) => s,
                None => {
                    // leader：发请求 → 写结果 → 唤醒所有等待者
                    let slot = self
                        .inflight
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .get(uri)
                        .cloned()
                        .expect("leader slot 刚登记");
                    let result = self.t.request("resources/read", Some(json!({ "uri": uri })));
                    let (lock, cv) = &*slot;
                    let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
                    *guard = Some(result.as_ref().map_err(|e| e.to_string()).cloned());
                    cv.notify_all();
                    drop(guard);
                    // 槽位留着不删：删会有竞态（新线程可能刚加入）。
                    // 后续读走缓存（TTL 8s），缓存过期后自然会有新的 leader。
                    let v = result?;
                    if let Ok(text) = serde_json::to_string(&v) {
                        self.cache.put(uri, &text);
                    }
                    return Ok(v);
                }
            };
            // 等待者：等结果（带超时降级 —— leader 若异常退出不至于永久卡死）
            let (lock, cv) = &*slot;
            let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
            while guard.is_none() {
                let remain = deadline.saturating_duration_since(std::time::Instant::now());
                if remain.is_zero() { break; }
                let (g, _t) = cv.wait_timeout(guard, remain).unwrap_or_else(|e| e.into_inner());
                guard = g;
            }
            // ★ 用 as_ref().clone() 而**不是 take()**：结果要被所有等待者共享读取，
            //   take() 只能取一次 → 第二个等待者永远等不到（死锁：快系统线程卡死，
            //   实测表现为"游戏十分钟没进展"）。
            match guard.as_ref().cloned() {
                Some(Ok(v)) => Ok(v),
                Some(Err(e)) => Err(McpError::Http(e)),
                // 超时降级：自己发一次（不合并），保证不卡死
                None => {
                    drop(guard);
                    let v = self.t.request("resources/read", Some(json!({ "uri": uri })))?;
                    if let Ok(text) = serde_json::to_string(&v) {
                        self.cache.put(uri, &text);
                    }
                    Ok(v)
                }
            }
        }
    }

    /// 读资源并取文本（资源返回 `contents[].text`）。
    pub fn read_text(&self, uri: &str) -> Result<String, McpError> {
        Ok(content_text(&self.read_resource(uri)?))
    }

    /// 读资源并解析 JSON。解析失败是错误，不做静默降级。
    pub fn read_json(&self, uri: &str) -> Result<Value, McpError> {
        let raw = self.read_text(uri)?;
        serde_json::from_str(&raw).map_err(|e| McpError::Json(format!("{e} (uri={uri})")))
    }

    /// 调工具并取文本（工具返回 `content[].text`，通常是 YAML 片段）。
    pub fn call_text(&self, name: &str, args: Value) -> Result<String, McpError> {
        Ok(content_text(&self.call_tool(name, args)?))
    }
}

/// 从 MCP 响应里提取文本：兼容工具(`content`)与资源(`contents`)两种形态。
pub fn content_text(v: &Value) -> String {
    for key in ["content", "contents"] {
        if let Some(arr) = v.get(key).and_then(|c| c.as_array()) {
            let mut s = String::new();
            for item in arr {
                if let Some(t) = item.get("text").and_then(|t| t.as_str()) {
                    if !s.is_empty() {
                        s.push('\n');
                    }
                    s.push_str(t);
                }
            }
            if !s.is_empty() {
                return s;
            }
        }
    }
    if let Some(sc) = v.get("structuredContent") {
        return sc.to_string();
    }
    v.to_string()
}
