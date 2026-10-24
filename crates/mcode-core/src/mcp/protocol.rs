//! MCP JSON-RPC 2.0 协议类型。
//!
//! 只实现客户端用到的子集：initialize 握手、tools/list、tools/call，
//! 以及 server→client 通知的宽容解析（未知字段/方法一律忽略——协议前向
//! 兼容靠宽容：别因为不认识的字段拒掉对端）。

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 客户端声明的协议版本（2025-03-26 支持 streamable HTTP）。
pub const PROTOCOL_VERSION: &str = "2025-03-26";

// ---------------------------------------------------------------------------
// JSON-RPC 2.0 信封
// ---------------------------------------------------------------------------

/// 请求（id 由发送方分配；通知 id=None 不期待响应）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: &'static str, // 恒 "2.0"
    pub id: Option<u64>,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// 响应 error 对象。
#[derive(Debug, Clone, Deserialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(default)]
    pub data: Option<Value>,
}

/// 响应：result 与 error 二选一（协议规定互斥）。
#[derive(Debug, Clone, Deserialize)]
pub struct JsonRpcResponse {
    #[allow(dead_code)]
    pub id: Option<u64>,
    #[serde(default)]
    pub result: Option<Value>,
    #[serde(default)]
    pub error: Option<JsonRpcError>,
}

impl JsonRpcResponse {
    /// 取 result；error 或缺 result → Err（带 JSON-RPC code/message）。
    pub fn into_result(self) -> anyhow::Result<Value> {
        match (self.result, self.error) {
            (Some(v), _) => Ok(v),
            (None, Some(e)) => Err(anyhow::anyhow!(
                "MCP 错误 {}: {}{}",
                e.code,
                e.message,
                e.data
                    .as_ref()
                    .map(|d| format!("（{d}）"))
                    .unwrap_or_default()
            )),
            (None, None) => Err(anyhow::anyhow!("MCP 响应缺少 result 与 error")),
        }
    }
}

/// 构造请求信封。
pub fn request(id: u64, method: &str, params: Option<Value>) -> JsonRpcRequest {
    JsonRpcRequest {
        jsonrpc: "2.0",
        id: Some(id),
        method: method.into(),
        params,
    }
}

/// 构造通知（无 id，不期待响应）。
pub fn notification(method: &str, params: Option<Value>) -> JsonRpcRequest {
    JsonRpcRequest {
        jsonrpc: "2.0",
        id: None,
        method: method.into(),
        params,
    }
}

// ---------------------------------------------------------------------------
// MCP 载荷
// ---------------------------------------------------------------------------

/// initialize 响应的 serverInfo（诊断用）。
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ServerInfo {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub version: String,
}

/// initialize 响应整体。
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResult {
    #[serde(default)]
    pub server_info: ServerInfo,
    /// 服务端能力（tools 支持与否决定了该 server 有没有工具可挂）。
    #[serde(default)]
    pub capabilities: Value,
    /// 服务端实际协商的协议版本（前向兼容：客户端按需适配）。
    #[serde(default)]
    pub protocol_version: Option<String>,
}

/// 工具注解（readOnlyHint 映射 [`ToolDefinition::is_read_only`] 的数据源）。
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ToolAnnotations {
    /// 服务端声明的只读提示（hint 而非保证——但 MCP 生态的约定是可信的，
    /// 信任决策留给用户权限规则兜底）。
    #[serde(default)]
    pub read_only_hint: Option<bool>,
}

/// tools/list 返回的工具描述。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpToolDef {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// JSON Schema（原样透传给模型；空对象 = 无参数工具）
    #[serde(default)]
    pub input_schema: Value,
    #[serde(default)]
    pub annotations: Option<ToolAnnotations>,
}

/// tools/call 返回的内容块（MCP content types 子集）。
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[serde(rename_all_fields = "camelCase")]
pub enum McpContent {
    Text {
        text: String,
    },
    Image {
        #[serde(default)]
        mime_type: String,
        /// base64 裸数据
        data: String,
    },
    /// 未知/未来类型：保底不崩（原样文本化）
    #[serde(other)]
    Unknown,
}

/// tools/call 的结果。
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct McpToolResult {
    #[serde(default)]
    pub content: Vec<McpContent>,
    /// 服务端声明的执行失败（协议层成功但业务失败）。
    #[serde(default)]
    pub is_error: bool,
}

impl McpToolResult {
    /// 文本拼接（多块 text 合并；供回喂模型）。
    pub fn text(&self) -> String {
        let mut out = String::new();
        for c in &self.content {
            if let McpContent::Text { text } = c {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(text);
            }
        }
        out
    }

    /// 图片块列表（→ [`crate::tools::ToolImage`]）。
    pub fn images(&self) -> Vec<(String, String)> {
        self.content
            .iter()
            .filter_map(|c| match c {
                McpContent::Image { mime_type, data } => Some((mime_type.clone(), data.clone())),
                _ => None,
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_envelope_serializes_jsonrpc_two_point_oh() {
        let req = request(1, "tools/list", None);
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], 1);
        assert_eq!(v["method"], "tools/list");
        assert!(v.get("params").is_none(), "params=None 不序列化");
    }

    #[test]
    fn notification_has_no_id() {
        let n = notification("notifications/initialized", None);
        let v = serde_json::to_value(&n).unwrap();
        assert_eq!(v["method"], "notifications/initialized");
        // serde 序列化 None id 会被 skip 吗？——JsonRpcRequest.id 是
        // Option 无 skip，会序列化为 null（协议允许通知 id 缺省/null）
        let id = &v["id"];
        assert!(id.is_null());
    }

    #[test]
    fn response_error_takes_precedence_when_no_result() {
        let resp: JsonRpcResponse = serde_json::from_str(
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"method not found"}}"#,
        )
        .unwrap();
        let err = resp.into_result().unwrap_err();
        assert!(err.to_string().contains("-32601"));
        assert!(err.to_string().contains("method not found"));
    }

    #[test]
    fn response_missing_both_is_error() {
        let resp: JsonRpcResponse = serde_json::from_str(r#"{"jsonrpc":"2.0","id":1}"#).unwrap();
        assert!(resp.into_result().is_err());
    }

    #[test]
    fn tool_def_tolerates_missing_optionals() {
        let t: McpToolDef = serde_json::from_str(r#"{"name":"ping"}"#).unwrap();
        assert_eq!(t.name, "ping");
        assert!(t.description.is_none());
        // annotations.readOnlyHint → 只读判定
        let t2: McpToolDef =
            serde_json::from_str(r#"{"name":"w","annotations":{"readOnlyHint":true}}"#).unwrap();
        assert_eq!(t2.annotations.unwrap().read_only_hint, Some(true));
    }

    #[test]
    fn tool_result_content_parsing() {
        let r: McpToolResult = serde_json::from_str(
            r#"{"content":[{"type":"text","text":"hello"},{"type":"image","mimeType":"image/png","data":"aGk="},{"type":"holo"}],"isError":true}"#,
        )
        .unwrap();
        assert!(r.is_error);
        assert_eq!(r.text(), "hello");
        assert_eq!(r.images(), vec![("image/png".into(), "aGk=".into())]);
    }

    #[test]
    fn unknown_content_type_is_tolerated() {
        let r: McpToolResult =
            serde_json::from_str(r#"{"content":[{"type":"future-thing","x":1}]}"#).unwrap();
        assert_eq!(r.content.len(), 1);
        assert_eq!(r.text(), "");
    }

    #[test]
    fn initialize_result_defaults() {
        let r: InitializeResult =
            serde_json::from_str(r#"{"protocolVersion":"2024-11-05"}"#).unwrap();
        assert_eq!(r.server_info.name, "");
        assert_eq!(r.protocol_version.as_deref(), Some("2024-11-05"));
    }
}
