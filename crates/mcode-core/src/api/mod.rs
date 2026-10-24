//! API 层：三协议后端 + 统一事件流 + 结构化错误。
//!
//! 顶层设计：
//! - [`ModelBackend::stream`] 是唯一入口：入参协议无关的 [`StreamRequest`]，
//!   出参归一化的 [`StreamEvent`] 流。协议差异（tool_call 累积、usage 位置、
//!   reasoning 字段名）全部封在后端内部。
//! - 错误用 [`ApiError`] 分类学表达，主循环按 kind 走不同恢复路径
//!   （重试 / 降级 fallback 模型 / reactive compact / 立即失败）。
//! - 超时策略：连接 15s；**流块间空闲 300s**（见 [`sse::with_read_timeout`]）；
//!   不设总时长——长推理合法，挂死才是要掐的。

pub mod anthropic;
pub mod chat;
pub mod error;
pub mod responses;
pub mod sse;

pub use error::{parse_retry_after, ApiError, ApiErrorKind};
pub use sse::{parse_sse, with_read_timeout, SseEvent};

use crate::message::{ApiMessage, StopReason, Usage};
use async_trait::async_trait;
use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;

/// 协议族。`Anthropic` 是消息模型的规范形（块 1:1 映射）；
/// `Responses`/`Chat` 由后端做降级翻译。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    Anthropic,
    #[default]
    Responses,
    Chat,
}

impl Protocol {
    pub fn parse(s: &str) -> Option<Protocol> {
        match s.trim().to_lowercase().as_str() {
            "anthropic" | "claude" | "claude_messages" => Some(Protocol::Anthropic),
            "responses" | "openai_responses" => Some(Protocol::Responses),
            "chat" | "chat_completions" | "chatcompletions" => Some(Protocol::Chat),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Protocol::Anthropic => "anthropic",
            Protocol::Responses => "responses",
            Protocol::Chat => "chat",
        }
    }
}

/// 思考强度（默认档位不发送字段，保持网关最大兼容——旧版同语义）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingEffort {
    Low,
    #[default]
    Medium,
    High,
}

impl ThinkingEffort {
    pub fn parse(s: &str) -> Option<ThinkingEffort> {
        match s.trim().to_lowercase().as_str() {
            "low" => Some(ThinkingEffort::Low),
            "medium" | "default" => Some(ThinkingEffort::Medium),
            "high" => Some(ThinkingEffort::High),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ThinkingEffort::Low => "low",
            ThinkingEffort::Medium => "medium",
            ThinkingEffort::High => "high",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThinkingConfig {
    pub effort: ThinkingEffort,
}

/// 线上工具定义（三家协议的 function 工具 schema 同形）。
#[derive(Clone, Debug, PartialEq)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

/// 一次流式请求的完整入参。
#[derive(Clone, Debug)]
pub struct StreamRequest {
    pub model: String,
    /// 系统提示分段（保持顺序；后端可对最后段打 cache 断点）。
    pub system: Vec<String>,
    pub messages: Vec<ApiMessage>,
    pub tools: Vec<ToolSpec>,
    pub max_output_tokens: u32,
    pub thinking: Option<ThinkingConfig>,
}

/// 归一化的流事件。后端保证顺序：每个 ToolUseStart 最终必有同 id 的
/// ToolUseDone；Done 恰好一次；Usage 至多一次（Done 前）。
#[derive(Clone, Debug, PartialEq)]
pub enum StreamEvent {
    TextDelta(String),
    ThinkingDelta {
        text: String,
    },
    /// 思考块收口（携带签名时，主循环把它存回 Thinking block 以便回传）。
    ThinkingDone {
        signature: Option<String>,
    },
    ToolUseStart {
        id: String,
        name: String,
    },
    ToolInputDelta {
        id: String,
        partial_json: String,
    },
    ToolUseDone {
        id: String,
        input: Value,
    },
    Usage(Usage),
    Done {
        stop_reason: StopReason,
    },
}

/// 模型后端。实现方必须线程安全（Arc 共享）。
#[async_trait]
pub trait ModelBackend: Send + Sync {
    fn backend_name(&self) -> &'static str;

    async fn stream(
        &self,
        req: StreamRequest,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ApiError>>, ApiError>;
}

/// 后端连接参数。由 config 层折叠而来——api 层不反向依赖 config 层。
#[derive(Clone, Debug)]
pub struct BackendConfig {
    pub protocol: Protocol,
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    /// Anthropic 协议下是否在 system 末段/最后一个工具上打
    /// `cache_control: ephemeral` 断点。原生 Anthropic 端点开启可省钱；
    /// 严格校验的私有网关应关闭（未知字段可能 400）。
    pub cache_control: bool,
}

impl BackendConfig {
    pub fn new(
        protocol: Protocol,
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        BackendConfig {
            protocol,
            base_url: base_url.into(),
            api_key: api_key.into(),
            model: model.into(),
            cache_control: false,
        }
    }
}

/// URL 拼接。Anthropic 端点：base 无 `/v1` 后缀时自动补；
/// Responses/Chat 维持旧版行为（`{base}/responses`、`{base}/chat/completions`
/// 原样拼接，兼容既有 provider 配置）。
pub(crate) fn join_url(base: &str, path: &str) -> String {
    let base = base.trim_end_matches('/');
    format!("{base}{path}")
}

pub(crate) fn join_url_v1(base: &str, path: &str) -> String {
    let base = base.trim_end_matches('/');
    if base.ends_with("/v1") {
        format!("{base}{path}")
    } else {
        format!("{base}/v1{path}")
    }
}

/// 按协议构造后端。
pub fn build_backend(cfg: &BackendConfig) -> Box<dyn ModelBackend> {
    match cfg.protocol {
        Protocol::Anthropic => Box::new(anthropic::AnthropicBackend::new(cfg)),
        Protocol::Responses => Box::new(responses::ResponsesBackend::new(cfg)),
        Protocol::Chat => Box::new(chat::ChatBackend::new(cfg)),
    }
}

/// 共享 HTTP 客户端。连接超时 15s；**不设总超时**——流式响应时长不可预估，
/// 挂死防护由 [`with_read_timeout`] 在事件层兜底。
pub fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .pool_idle_timeout(Duration::from_secs(90))
        .build()
        .expect("reqwest client 构建失败（静态配置，不可能失败）")
}

/// 流块间空闲超时：模型"思考中"两块之间可以很久，但 5 分钟无任何字节
/// 视为连接挂死。
pub const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(300);
