//! Anthropic Messages 协议后端（规范形）。
//!
//! 消息模型的 ContentBlock 与 Anthropic wire format 1:1——本后端是
//! 三家中的"零翻译"参考实现；Responses/Chat 后端各自做降级映射。
//!
//! 流式协议（SSE 事件序）：
//! `message_start`（usage.input）→ 若干 `content_block_start/delta/stop`
//! （text / thinking / tool_use）→ `message_delta`（stop_reason + usage.output）
//! → `message_stop`。mid-stream `error` 事件转 [`ApiErrorKind::Protocol`]。

use super::error::{classify, parse_retry_after, ApiError, ApiErrorKind};
use super::sse::{parse_sse, with_read_timeout, SseEvent};
use super::{
    join_url_v1, BackendConfig, ModelBackend, StreamEvent, StreamRequest, STREAM_IDLE_TIMEOUT,
};
use crate::message::{ApiMessage, ContentBlock, StopReason, Usage};
use async_trait::async_trait;
use futures::stream::{BoxStream, StreamExt};
use serde_json::{json, Value};

const ANTHROPIC_VERSION: &str = "2023-06-01";

pub struct AnthropicBackend {
    cfg: BackendConfig,
    http: reqwest::Client,
}

impl AnthropicBackend {
    pub fn new(cfg: &BackendConfig) -> Self {
        AnthropicBackend {
            cfg: cfg.clone(),
            http: super::http_client(),
        }
    }

    /// 组装请求体。Thinking 开启时附带 budget（按强度分档），
    /// 并保证 max_tokens > budget（Anthropic 硬约束）。
    pub(crate) fn build_body(req: &StreamRequest, cache_control: bool) -> Value {
        let mut system_blocks: Vec<Value> = req
            .system
            .iter()
            .filter(|s| !s.is_empty())
            .map(|s| json!({ "type": "text", "text": s }))
            .collect();
        if cache_control {
            if let Some(last) = system_blocks.last_mut() {
                last["cache_control"] = json!({ "type": "ephemeral" });
            }
        }

        let mut body = json!({
            "model": req.model,
            "max_tokens": req.max_output_tokens,
            "stream": true,
            "messages": serialize_messages(&req.messages),
        });
        if !system_blocks.is_empty() {
            body["system"] = Value::Array(system_blocks);
        }
        if !req.tools.is_empty() {
            let mut tools: Vec<Value> = req
                .tools
                .iter()
                .map(|t| {
                    json!({
                        "name": t.name,
                        "description": t.description,
                        "input_schema": t.parameters,
                    })
                })
                .collect();
            if cache_control {
                if let Some(last) = tools.last_mut() {
                    last["cache_control"] = json!({ "type": "ephemeral" });
                }
            }
            body["tools"] = Value::Array(tools);
        }
        if let Some(thinking) = req.thinking {
            let budget = thinking_budget(thinking.effort.as_str());
            // max_tokens 必须大于 thinking budget
            let max_tokens = req.max_output_tokens.max(budget + 4096);
            body["max_tokens"] = json!(max_tokens);
            body["thinking"] = json!({ "type": "enabled", "budget_tokens": budget });
        }
        body
    }

    async fn send(
        &self,
        req: StreamRequest,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ApiError>>, ApiError> {
        let url = join_url_v1(&self.cfg.base_url, "/messages");
        let body = Self::build_body(&req, self.cfg.cache_control);
        let resp = self
            .http
            .post(&url)
            .header("x-api-key", &self.cfg.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .json(&body)
            .send()
            .await
            .map_err(|e| ApiError::new(ApiErrorKind::Network, format!("请求发送失败: {e}")))?;

        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            let retry_after = parse_retry_after(
                resp.headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok()),
            );
            let text = resp.text().await.unwrap_or_default();
            return Err(classify(status, &text, retry_after));
        }

        let byte_stream = resp.bytes_stream().map(|r| r.map_err(|e| e.to_string()));
        let sse = parse_sse(byte_stream);
        let events =
            AnthropicSse::default().into_stream(with_read_timeout(sse, STREAM_IDLE_TIMEOUT));
        Ok(events)
    }
}

fn thinking_budget(effort: &str) -> u32 {
    match effort {
        "low" => 2048,
        "high" => 20480,
        _ => 8192,
    }
}

// ---------------------------------------------------------------------------
// 消息序列化：ContentBlock → Anthropic wire format（近乎直通）
// ---------------------------------------------------------------------------

pub(crate) fn serialize_messages(messages: &[ApiMessage]) -> Vec<Value> {
    messages
        .iter()
        .map(|m| match m {
            ApiMessage::User { blocks } => json!({
                "role": "user",
                "content": blocks.iter().map(serialize_block).collect::<Vec<_>>(),
            }),
            ApiMessage::Assistant { blocks, .. } => json!({
                "role": "assistant",
                "content": blocks.iter().map(serialize_block).collect::<Vec<_>>(),
            }),
        })
        .collect()
}

pub(crate) fn serialize_block(block: &ContentBlock) -> Value {
    match block {
        ContentBlock::Text { text } => json!({ "type": "text", "text": text }),
        ContentBlock::Thinking { text, signature } => {
            let mut v = json!({ "type": "thinking", "thinking": text });
            if let Some(sig) = signature {
                v["signature"] = json!(sig);
            }
            v
        }
        ContentBlock::RedactedThinking { data } => {
            json!({ "type": "redacted_thinking", "data": data })
        }
        ContentBlock::ToolUse { id, name, input } => {
            json!({ "type": "tool_use", "id": id, "name": name, "input": input })
        }
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => json!({
            "type": "tool_result",
            "tool_use_id": tool_use_id,
            "content": [{ "type": "text", "text": content }],
            "is_error": is_error,
        }),
        ContentBlock::Image { media_type, data } => json!({
            "type": "image",
            "source": { "type": "base64", "media_type": media_type, "data": data },
        }),
    }
}

fn map_stop_reason(raw: Option<&str>) -> StopReason {
    match raw {
        Some("max_tokens") => StopReason::MaxTokens,
        Some("tool_use") => StopReason::ToolUse,
        Some("refusal") => StopReason::Refusal,
        Some("end_turn") | Some("stop_sequence") | None => StopReason::EndTurn,
        Some(_) => StopReason::Other,
    }
}

// ---------------------------------------------------------------------------
// SSE 事件机：按 content block index 累积
// ---------------------------------------------------------------------------

#[derive(Default)]
struct AnthropicSse {
    /// index → 块累积状态
    blocks: std::collections::BTreeMap<u64, BlockAcc>,
    input_usage: Usage,
    stop_reason: Option<StopReason>,
}

enum BlockAcc {
    Text,
    Thinking {
        signature: String,
    },
    /// `start_input`：部分兼容网关（MiniMax /anthropic）把完整参数放在
    /// content_block_start.input 且不发分片——留作收口兜底。
    Tool {
        id: String,
        json: String,
        start_input: Value,
    },
    Other,
}

impl AnthropicSse {
    fn into_stream(
        self,
        sse: impl futures::Stream<Item = Result<SseEvent, ApiError>> + Unpin + Send + 'static,
    ) -> BoxStream<'static, Result<StreamEvent, ApiError>> {
        Box::pin(futures::stream::unfold(
            (self, sse, false),
            |(mut acc, mut sse, done)| async move {
                if done {
                    return None;
                }
                loop {
                    match sse.next().await {
                        None => {
                            // 流结束但没收到 message_stop：补一个 Done 防止主循环悬挂
                            return Some((
                                Ok(StreamEvent::Done {
                                    stop_reason: acc.stop_reason.unwrap_or(StopReason::EndTurn),
                                }),
                                (acc, sse, true),
                            ));
                        }
                        Some(Err(e)) => return Some((Err(e), (acc, sse, done))),
                        Some(Ok(ev)) => match acc.on_event(&ev.data) {
                            Some(Ok(out)) => return Some((Ok(out), (acc, sse, done))),
                            Some(Err(e)) => return Some((Err(e), (acc, sse, true))),
                            None => {}
                        },
                    }
                }
            },
        ))
    }

    /// 处理一条 SSE data；返回需要外发的事件（至多一条，可为错误）。
    fn on_event(&mut self, data: &str) -> Option<Result<StreamEvent, ApiError>> {
        let v: Value = serde_json::from_str(data).ok()?;
        let typ = v.get("type")?.as_str()?;

        match typ {
            "message_start" => {
                if let Some(usage) = v.pointer("/message/usage") {
                    self.input_usage.input_tokens = usage
                        .get("input_tokens")
                        .and_then(|x| x.as_u64())
                        .unwrap_or(0);
                    self.input_usage.cache_read_input_tokens = usage
                        .get("cache_read_input_tokens")
                        .and_then(|x| x.as_u64())
                        .unwrap_or(0);
                    self.input_usage.cache_creation_input_tokens = usage
                        .get("cache_creation_input_tokens")
                        .and_then(|x| x.as_u64())
                        .unwrap_or(0);
                }
                None
            }
            "content_block_start" => {
                let index = v.get("index").and_then(|x| x.as_u64()).unwrap_or(0);
                let block = v.get("content_block")?;
                match block.get("type").and_then(|x| x.as_str()) {
                    Some("text") => {
                        self.blocks.insert(index, BlockAcc::Text);
                        None
                    }
                    Some("thinking") | Some("redacted_thinking") => {
                        self.blocks.insert(
                            index,
                            BlockAcc::Thinking {
                                signature: String::new(),
                            },
                        );
                        None
                    }
                    Some("tool_use") => {
                        let id = block
                            .get("id")
                            .and_then(|x| x.as_str())
                            .unwrap_or("")
                            .to_string();
                        let name = block
                            .get("name")
                            .and_then(|x| x.as_str())
                            .unwrap_or("")
                            .to_string();
                        // start 可能已带完整 input（兼容网关形态）——仅在非空对象时记录
                        let start_input = match block.get("input") {
                            Some(v @ Value::Object(map)) if !map.is_empty() => v.clone(),
                            _ => Value::Null,
                        };
                        self.blocks.insert(
                            index,
                            BlockAcc::Tool {
                                id: id.clone(),
                                json: String::new(),
                                start_input,
                            },
                        );
                        Some(Ok(StreamEvent::ToolUseStart { id, name }))
                    }
                    _ => {
                        self.blocks.insert(index, BlockAcc::Other);
                        None
                    }
                }
            }
            "content_block_delta" => {
                let index = v.get("index").and_then(|x| x.as_u64()).unwrap_or(0);
                let delta = v.get("delta")?;
                match delta.get("type").and_then(|x| x.as_str()) {
                    Some("text_delta") => {
                        let text = delta.get("text").and_then(|x| x.as_str()).unwrap_or("");
                        (!text.is_empty()).then(|| Ok(StreamEvent::TextDelta(text.to_string())))
                    }
                    Some("thinking_delta") => {
                        let text = delta.get("thinking").and_then(|x| x.as_str()).unwrap_or("");
                        (!text.is_empty()).then(|| {
                            Ok(StreamEvent::ThinkingDelta {
                                text: text.to_string(),
                            })
                        })
                    }
                    Some("signature_delta") => {
                        if let Some(BlockAcc::Thinking { signature }) = self.blocks.get_mut(&index)
                        {
                            signature.push_str(
                                delta
                                    .get("signature")
                                    .and_then(|x| x.as_str())
                                    .unwrap_or(""),
                            );
                        }
                        None
                    }
                    Some("input_json_delta") => {
                        if let Some(BlockAcc::Tool { json, id, .. }) = self.blocks.get_mut(&index) {
                            json.push_str(
                                delta
                                    .get("partial_json")
                                    .and_then(|x| x.as_str())
                                    .unwrap_or(""),
                            );
                            let id = id.clone();
                            let partial = delta
                                .get("partial_json")
                                .and_then(|x| x.as_str())
                                .unwrap_or("")
                                .to_string();
                            return (!partial.is_empty()).then_some(Ok(
                                StreamEvent::ToolInputDelta {
                                    id,
                                    partial_json: partial,
                                },
                            ));
                        }
                        None
                    }
                    _ => None,
                }
            }
            "content_block_stop" => {
                let index = v.get("index").and_then(|x| x.as_u64()).unwrap_or(0);
                match self.blocks.remove(&index) {
                    Some(BlockAcc::Tool {
                        id,
                        json,
                        start_input,
                    }) => {
                        // 兜底优先级：完整分片解析 > start.input > 空对象
                        let parsed: Option<Value> = if json.trim().is_empty() {
                            None
                        } else {
                            serde_json::from_str(&json).ok()
                        };
                        let input = parsed
                            .or_else(|| match start_input {
                                v @ Value::Object(_) if !v.is_null() => Some(v),
                                _ => None,
                            })
                            .unwrap_or_else(|| json!({}));
                        Some(Ok(StreamEvent::ToolUseDone { id, input }))
                    }
                    Some(BlockAcc::Thinking { signature }) => Some(Ok(StreamEvent::ThinkingDone {
                        signature: (!signature.is_empty()).then_some(signature),
                    })),
                    _ => None,
                }
            }
            "message_delta" => {
                if let Some(usage) = v.get("usage") {
                    self.input_usage.output_tokens = usage
                        .get("output_tokens")
                        .and_then(|x| x.as_u64())
                        .unwrap_or(self.input_usage.output_tokens);
                }
                if let Some(reason) = v.pointer("/delta/stop_reason").and_then(|x| x.as_str()) {
                    self.stop_reason = Some(map_stop_reason(Some(reason)));
                } else if v
                    .pointer("/delta/stop_reason")
                    .map(|x| x.is_null())
                    .unwrap_or(false)
                {
                    self.stop_reason = Some(StopReason::EndTurn);
                }
                None
            }
            "message_stop" => Some(Ok(StreamEvent::Done {
                stop_reason: self.stop_reason.unwrap_or(StopReason::EndTurn),
            })),
            "error" => {
                let msg = v
                    .pointer("/error/message")
                    .and_then(|x| x.as_str())
                    .unwrap_or("mid-stream error");
                Some(Err(ApiError::new(
                    ApiErrorKind::Protocol {
                        detail: msg.to_string(),
                    },
                    msg,
                )))
            }
            _ => None,
        }
    }
}

#[async_trait]
impl ModelBackend for AnthropicBackend {
    fn backend_name(&self) -> &'static str {
        "anthropic"
    }

    async fn stream(
        &self,
        req: StreamRequest,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ApiError>>, ApiError> {
        self.send(req).await
    }
}

#[cfg(test)]
mod tests {
    use super::super::ToolSpec;
    use super::*;
    use crate::message::{normalize_for_api, Conversation};
    use futures::stream::{self, Stream};

    fn sse_events(datas: Vec<&str>) -> impl Stream<Item = Result<SseEvent, ApiError>> {
        stream::iter(
            datas
                .into_iter()
                .map(|d| Ok(SseEvent::data_only(d)))
                .collect::<Vec<_>>(),
        )
    }

    fn run(acc_data: Vec<&str>) -> Vec<StreamEvent> {
        let backend_acc = AnthropicSse::default();
        let sse = sse_events(acc_data);
        futures::executor::block_on(async {
            backend_acc.into_stream(sse).collect::<Vec<_>>().await
        })
        .into_iter()
        .map(|r| r.unwrap())
        .collect()
    }

    #[test]
    fn builds_canonical_request_body() {
        let req = StreamRequest {
            model: "claude-sonnet-5-5".into(),
            system: vec!["you are mcode".into()],
            messages: normalize_for_api(&{
                let mut c = Conversation::new();
                c.push(crate::message::Message::user_text("hi"));
                c.active_messages()
            }),
            tools: vec![ToolSpec {
                name: "read_file".into(),
                description: "read".into(),
                parameters: json!({"type":"object","properties":{}}),
            }],
            max_output_tokens: 8192,
            thinking: None,
        };
        let body = AnthropicBackend::build_body(&req, false);
        assert_eq!(body["model"], "claude-sonnet-5-5");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
        assert!(body.get("thinking").is_none());
    }

    #[test]
    fn thinking_bumps_max_tokens() {
        let req = StreamRequest {
            model: "m".into(),
            system: vec![],
            messages: vec![],
            tools: vec![],
            max_output_tokens: 4096,
            thinking: Some(super::super::ThinkingConfig {
                effort: super::super::ThinkingEffort::High,
            }),
        };
        let body = AnthropicBackend::build_body(&req, false);
        assert_eq!(body["thinking"]["budget_tokens"], 20480);
        assert_eq!(body["max_tokens"], 20480 + 4096);
    }

    #[test]
    fn full_stream_roundtrip() {
        let out = run(vec![
            r#"{"type":"message_start","message":{"usage":{"input_tokens":100,"cache_read_input_tokens":50}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"想一下"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig9"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"text"}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"你好"}}"#,
            r#"{"type":"content_block_stop","index":1}"#,
            r#"{"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"tu1","name":"bash"}}"#,
            r#"{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"command\":"}}"#,
            r#"{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"\"ls\"}"}}"#,
            r#"{"type":"content_block_stop","index":2}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":42}}"#,
            r#"{"type":"message_stop"}"#,
        ]);
        let starts: Vec<_> = out
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ToolUseStart { id, name } => Some((id.clone(), name.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(starts, vec![("tu1".to_string(), "bash".to_string())]);
        assert!(matches!(
            out.iter().find(|e| matches!(e, StreamEvent::ThinkingDone { .. })),
            Some(StreamEvent::ThinkingDone { signature: Some(s) }) if s == "sig9"
        ));
        assert!(out
            .iter()
            .any(|e| matches!(e, StreamEvent::TextDelta(t) if t == "你好")));
        let done = out
            .iter()
            .find_map(|e| match e {
                StreamEvent::ToolUseDone { id, input } => Some((id.clone(), input.clone())),
                _ => None,
            })
            .unwrap();
        assert_eq!(done.0, "tu1");
        assert_eq!(done.1["command"], "ls");
        // 最后一个事件是 Done，stop_reason = ToolUse
        match out.last().unwrap() {
            StreamEvent::Done { stop_reason } => assert_eq!(*stop_reason, StopReason::ToolUse),
            other => panic!("expected Done, got {other:?}"),
        }
    }

    /// MiniMax 等 /anthropic 兼容层：参数完整出现在 content_block_start.input、
    /// 不发（或只发空）input_json_delta——收口必须回退到 start 的 input。
    #[test]
    fn tool_input_in_start_used_when_deltas_missing() {
        let out = run(vec![
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t1","name":"bash","input":{"command":"ls"}}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":""}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
        ]);
        let done = out
            .iter()
            .find_map(|e| match e {
                StreamEvent::ToolUseDone { id, input } => Some((id.clone(), input.clone())),
                _ => None,
            })
            .unwrap();
        assert_eq!(done.0, "t1");
        assert_eq!(done.1["command"], "ls");
    }

    /// 分片 JSON 残缺时同样回退 start.input（兜底优先级：完整分片 > start.input > 空对象）。
    #[test]
    fn broken_delta_json_falls_back_to_start_input() {
        let out = run(vec![
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t2","name":"bash","input":{"command":"pwd"}}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"command\":"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
        ]);
        let done = out
            .iter()
            .find_map(|e| match e {
                StreamEvent::ToolUseDone { id, input } => Some((id.clone(), input.clone())),
                _ => None,
            })
            .unwrap();
        assert_eq!(done.1["command"], "pwd");
    }

    #[test]
    fn stream_without_stop_yields_done_fallback() {
        let out = run(vec![
            r#"{"type":"message_start","message":{"usage":{"input_tokens":10}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"partial"}}"#,
        ]);
        match out.last().unwrap() {
            StreamEvent::Done { stop_reason } => assert_eq!(*stop_reason, StopReason::EndTurn),
            other => panic!("expected fallback Done, got {other:?}"),
        }
    }

    #[test]
    fn serializes_tool_result_with_is_error() {
        let block = ContentBlock::tool_result_error("t1", "boom");
        let v = serialize_block(&block);
        assert_eq!(v["is_error"], true);
        assert_eq!(v["content"][0]["text"], "boom");
    }
}
