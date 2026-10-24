//! API 错误分类学。
//!
//! 旧版靠 13 个字符串子串判断"可重试"，新方案按结构化 kind 分类，
//! 每种 kind 驱动主循环不同的恢复策略：
//! - `RateLimited` → 读 Retry-After 退避重试；
//! - `Overloaded` → 指数退避重试，N 次后降级 fallback 模型；
//! - `ContextOverflow` → **不重试**，触发 reactive compact 后重发；
//! - `MaxOutput` 类截断不是错误（stop_reason 表达）；
//! - `Auth/Billing/InvalidRequest` → 立即失败，重试无意义。

use std::time::Duration;

#[derive(Clone, Debug, PartialEq)]
pub enum ApiErrorKind {
    /// 用户/系统取消（cancel watch 触发），不属于失败。
    Cancelled,
    /// 429 限流。带 Retry-After 提示时优先采用。
    RateLimited { retry_after: Option<Duration> },
    /// 529 / 503+overloaded：服务端过载，可退避重试。
    Overloaded,
    /// 输入超出上下文窗口——主循环的 reactive compact continue 点。
    ContextOverflow,
    /// 401/403：密钥无效或无权限。
    Auth,
    /// 402：余额/配额问题。
    Billing,
    /// 400 其他：请求本身不合法（参数错、工具 schema 错），重试无意义。
    InvalidRequest { detail: String },
    /// 其他 5xx。
    Server { status: u16 },
    /// 连接建立/读写失败。
    Network,
    /// 显式超时（reqwest timeout / 手动计时）。
    Timeout,
    /// 流中断（收到一半连接断开）——可重试，已产出的部分文本由调用方保留。
    StreamAborted,
    /// SSE/JSON 结构不符合协议预期（网关兼容性问题）。
    Protocol { detail: String },
}

#[derive(Clone, Debug)]
pub struct ApiError {
    pub kind: ApiErrorKind,
    pub message: String,
    pub status: Option<u16>,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.status {
            Some(status) => write!(f, "[{status}] {}", self.message),
            None => write!(f, "{}", self.message),
        }
    }
}

impl std::error::Error for ApiError {}

impl ApiError {
    pub fn new(kind: ApiErrorKind, message: impl Into<String>) -> Self {
        ApiError {
            kind,
            message: message.into(),
            status: None,
        }
    }

    pub fn with_status(mut self, status: u16) -> Self {
        self.status = Some(status);
        self
    }

    pub fn cancelled() -> Self {
        ApiError::new(ApiErrorKind::Cancelled, "cancelled")
    }

    /// 是否值得原样重试（同参数重发）。
    pub fn is_retryable(&self) -> bool {
        matches!(
            self.kind,
            ApiErrorKind::RateLimited { .. }
                | ApiErrorKind::Overloaded
                | ApiErrorKind::Server { .. }
                | ApiErrorKind::Network
                | ApiErrorKind::Timeout
                | ApiErrorKind::StreamAborted
        )
    }

    /// 触发 reactive compact 的信号。
    pub fn is_context_overflow(&self) -> bool {
        matches!(self.kind, ApiErrorKind::ContextOverflow)
    }

    pub fn is_cancelled(&self) -> bool {
        matches!(self.kind, ApiErrorKind::Cancelled)
    }

    /// 建议的下一次重试延迟。RateLimited 优先服务端提示，
    /// 其余指数退避（base 1s × 2^attempt，封顶 32s）+ 少量抖动。
    pub fn suggested_retry_delay(&self, attempt: u32) -> Duration {
        if let ApiErrorKind::RateLimited {
            retry_after: Some(d),
        } = self.kind
        {
            return d.min(Duration::from_secs(120));
        }
        let exp = 1u64.saturating_add(attempt as u64).min(6); // 2^attempt, 封顶 64
        let base_ms = 1000u64.saturating_mul(1u64 << exp) / 2;
        let jitter = (attempt as u64 * 137) % 250; // 确定性伪抖动，测试可复现
        Duration::from_millis(base_ms + jitter).min(Duration::from_secs(32))
    }
}

/// 从 HTTP 状态码 + 响应体 + Retry-After 头归类。
///
/// body 嗅探覆盖三家协议的上下文溢出措辞：
/// - Anthropic: "prompt is too long" / "input length and max_tokens exceed context limit"
/// - OpenAI:    "context_length_exceeded" / "maximum context length"
/// - vLLM 等:   "This model's maximum context length" / "longer than the model's context length"
pub fn classify(status: u16, body: &str, retry_after: Option<Duration>) -> ApiError {
    let lower = body.to_lowercase();
    // 溢出嗅探优先于状态码：私有网关常用 500/502 包装 context overflow。
    let kind = if status >= 400 && is_context_overflow_text(&lower) {
        ApiErrorKind::ContextOverflow
    } else {
        match status {
            429 => ApiErrorKind::RateLimited { retry_after },
            401 | 403 => ApiErrorKind::Auth,
            402 => ApiErrorKind::Billing,
            529 => ApiErrorKind::Overloaded,
            400..=499 => ApiErrorKind::InvalidRequest {
                detail: truncate_body(body),
            },
            s if s >= 500 => {
                if lower.contains("overloaded") {
                    ApiErrorKind::Overloaded
                } else {
                    ApiErrorKind::Server { status: s }
                }
            }
            _ => ApiErrorKind::Protocol {
                detail: truncate_body(body),
            },
        }
    };
    ApiError {
        kind,
        message: extract_error_message(body).unwrap_or_else(|| truncate_body(body)),
        status: Some(status),
    }
}

pub fn is_context_overflow_text(lower: &str) -> bool {
    lower.contains("context_length_exceeded")
        || lower.contains("context length")
        || lower.contains("context window")
        || lower.contains("prompt is too long")
        || lower.contains("prompt too long")
        || lower.contains("input is too long")
        || lower.contains("input length and `max_tokens`")
        || lower.contains("reduce the length of the messages")
}

/// 尽力从各家错误体里抠出 message 字段；抠不出就返回截断的原文。
/// 逐字段独立探测（不能用 `?` 短路——`{"message":...}` 体里没有 error 键
/// 不该放弃提取）。
fn extract_error_message(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let candidates = [
        value
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str()),
        value.get("error").and_then(|e| e.as_str()),
        value.get("message").and_then(|m| m.as_str()),
        value.get("detail").and_then(|d| d.as_str()),
    ];
    candidates.iter().flatten().next().map(|s| (*s).to_string())
}

fn truncate_body(body: &str) -> String {
    const MAX: usize = 500;
    if body.chars().count() <= MAX {
        body.to_string()
    } else {
        let truncated: String = body.chars().take(MAX).collect();
        format!("{truncated}…")
    }
}

/// 解析 Retry-After 头（秒数或 HTTP-date；只处理秒数，date 场景罕见且成本高）。
pub fn parse_retry_after(value: Option<&str>) -> Option<Duration> {
    let value = value?.trim();
    value.parse::<u64>().ok().map(Duration::from_secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_rate_limit_with_header() {
        let err = classify(
            429,
            r#"{"error":{"message":"slow down"}}"#,
            Some(Duration::from_secs(7)),
        );
        assert!(
            matches!(err.kind, ApiErrorKind::RateLimited { retry_after: Some(d) } if d == Duration::from_secs(7))
        );
        assert!(err.is_retryable());
        assert_eq!(err.suggested_retry_delay(3), Duration::from_secs(7));
    }

    #[test]
    fn classifies_context_overflow_across_wordings() {
        for body in [
            r#"{"error":{"message":"prompt is too long: 200000 tokens > 180000"}}"#,
            r#"{"error":{"code":"context_length_exceeded"}}"#,
            r#"This model's maximum context length is 8192 tokens"#,
        ] {
            let err = classify(400, body, None);
            assert!(
                err.is_context_overflow(),
                "should classify as overflow: {body}"
            );
            assert!(!err.is_retryable(), "overflow must not be retried as-is");
        }
    }

    #[test]
    fn classifies_overloaded_from_529_and_503() {
        assert!(matches!(
            classify(529, "overloaded", None).kind,
            ApiErrorKind::Overloaded
        ));
        assert!(matches!(
            classify(503, "server overloaded, try later", None).kind,
            ApiErrorKind::Overloaded
        ));
    }

    #[test]
    fn auth_and_billing_never_retry() {
        assert!(!classify(401, "bad key", None).is_retryable());
        assert!(!classify(402, "no credit", None).is_retryable());
    }

    #[test]
    fn retry_backoff_is_bounded() {
        let err = classify(500, "boom", None);
        assert!(err.suggested_retry_delay(0).as_millis() >= 900);
        assert!(err.suggested_retry_delay(30) <= Duration::from_secs(32));
    }

    #[test]
    fn extracts_message_field() {
        let err = classify(
            400,
            r#"{"error":{"message":"tools.0: invalid schema"}}"#,
            None,
        );
        assert_eq!(err.message, "tools.0: invalid schema");
    }

    #[test]
    fn parse_retry_after_seconds() {
        assert_eq!(
            parse_retry_after(Some(" 12 ")),
            Some(Duration::from_secs(12))
        );
        assert_eq!(
            parse_retry_after(Some("Tue, 23 Oct 2026 10:00:00 GMT")),
            None
        );
        assert_eq!(parse_retry_after(None), None);
    }
}
