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
}

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
        Ok(Self { t, cache: SnapshotCache::default() })
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
        let v = self.t.request("resources/read", Some(json!({ "uri": uri })))?;
        if let Ok(text) = serde_json::to_string(&v) {
            self.cache.put(uri, &text);
        }
        Ok(v)
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
