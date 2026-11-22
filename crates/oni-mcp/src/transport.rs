use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use serde_json::{json, Value};

use crate::gate::{max_concurrent_requests, Gate};
use thiserror::Error;

pub const PROTOCOL_VERSION: &str = "2025-11-25";

#[derive(Debug, Error)]
pub enum McpError {
    #[error("HTTP 传输失败: {0}")]
    Http(String),
    #[error("JSON 解析失败: {0}")]
    Json(String),
    #[error("JSON-RPC 错误: {0}")]
    Rpc(String),
    #[error("模组返回错误: {0}")]
    Tool(String),
}

/// Streamable-HTTP 传输层：负责握手、session 复用、SSE 兜底、重连。
pub struct Transport {
    endpoint: String,
    agent: ureq::Agent,
    session_id: Mutex<Option<String>>,
    next_id: AtomicI64,
    /// **并发闸门**：所有 MCP 请求共用，保证同时在飞的请求不超过许可数。
    ///
    /// 为什么需要：
    ///   mod 的 HTTP 端点扛不住几百个并发请求 —— `graph --full` 逐格读全图时
    ///   单次查询就是几百个请求，多个查询叠加直接 503（ 6 个并发查询
    ///   每个 300+ 格读取失败）。没有闸门时 rayon 的并行度会直接压垮服务端。
    gate: Gate,
    /// 503/超时 的重试次数（带指数退避）。
    retries: u32,
}

impl Transport {
    pub fn new(endpoint: &str) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(60))
            .build();
        Self {
            endpoint: endpoint.to_string(),
            agent,
            session_id: Mutex::new(None),
            next_id: AtomicI64::new(1),
            gate: Gate::new(max_concurrent_requests()),
            retries: 3,
        }
    }

    /// 带并发闸门 + 指数退避重试的请求。
    ///
    /// 所有 MCP 请求都必须走这里：
    ///   1) **闸门**限制同时在飞的请求数
    ///      —— 防止几百个并发把 mod 端点打成 503。
    ///   2) **退避重试**：503/502/504/429 是"暂时过载"，退避后重试往往就成功了。
    ///      之前是直接失败 → 上层看到大面积"读取失败"，误判成地图问题。
    fn post_gated(&self, payload: &Value) -> Result<Value, McpError> {
        let attempts = self.retries.max(1);
        let mut last: Option<McpError> = None;
        for attempt in 1..=attempts {
            let _permit = self.gate.acquire();
            match self.post_raw(payload) {
                Ok(v) => return Ok(v),
                Err(e) => {
                    let msg = e.to_string();
                    let overloaded = crate::gate::is_overload(&msg);
                    if !overloaded || attempt >= attempts {
                        return Err(e);
                    }
                    // 指数退避：100ms -> 200ms -> 400ms
                    let wait = Duration::from_millis(100u64 << (attempt - 1).min(4))
                        .min(Duration::from_secs(2));
                    std::thread::sleep(wait);
                    last = Some(e);
                }
            }
        }
        Err(last.unwrap_or_else(|| McpError::Http("请求失败".into())))
    }

    fn post_raw(&self, payload: &Value) -> Result<Value, McpError> {
        let body = serde_json::to_string(payload).map_err(|e| McpError::Json(e.to_string()))?;
        let mut req = self
            .agent
            .post(&self.endpoint)
            .set("Content-Type", "application/json")
            .set("Accept", "application/json, text/event-stream")
            .set("Mcp-Protocol-Version", PROTOCOL_VERSION);
        if let Some(sid) = self.session_id.lock().ok().and_then(|g| g.clone()) {
            req = req.set("Mcp-Session-Id", &sid);
        }
        let resp = req.send_string(&body).map_err(|e| McpError::Http(e.to_string()))?;
        if let Some(sid) = resp.header("Mcp-Session-Id") {
            if let Ok(mut g) = self.session_id.lock() {
                *g = Some(sid.to_string());
            }
        }
        let raw = resp.into_string().map_err(|e| McpError::Http(e.to_string()))?;
        let raw = unwrap_sse(&raw);
        serde_json::from_str(&raw).map_err(|e| McpError::Json(format!("{e}: {}", &raw[..raw.len().min(300)])))
    }

    /// JSON-RPC 请求，返回 `result`。
    pub fn request(&self, method: &str, params: Option<Value>) -> Result<Value, McpError> {
        let mut payload = json!({ "jsonrpc": "2.0", "id": self.next_id.fetch_add(1, Ordering::SeqCst), "method": method });
        if let Some(p) = params {
            payload["params"] = p;
        }
        let v = self.post_gated(&payload)?;
        if let Some(err) = v.get("error") {
            return Err(McpError::Rpc(err.to_string()));
        }
        Ok(v.get("result").cloned().unwrap_or(Value::Null))
    }

    /// 通知（无响应）。失败不致命。
    pub fn notify(&self, method: &str, params: Option<Value>) {
        let mut payload = json!({ "jsonrpc": "2.0", "method": method });
        if let Some(p) = params {
            payload["params"] = p;
        }
        let _ = self.post_gated(&payload);
    }

    pub fn session_id(&self) -> Option<String> {
        self.session_id.lock().ok().and_then(|g| g.clone())
    }
}

/// Streamable HTTP 可能返回 SSE 帧；取最后一个 `data:` 行。
fn unwrap_sse(raw: &str) -> String {
    let t = raw.trim_start();
    if t.starts_with("event:") || t.starts_with("data:") {
        raw.lines()
            .filter(|l| l.starts_with("data:"))
            .last()
            .map(|l| l.trim_start_matches("data:").trim().to_string())
            .unwrap_or_else(|| raw.to_string())
    } else {
        raw.to_string()
    }
}
