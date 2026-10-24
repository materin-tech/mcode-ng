//! OpenAI Responses 协议后端。
//!
//! 相比旧版的关键升级：工具调用/结果用原生 `function_call` /
//! `function_call_output` item 结构化回喂（旧版折成纯文本 user 消息），
//! 多步任务的模型可见上下文不再退化。
//!
//! wire 格式：
//! - 请求：`{model, instructions, input:[items], tools, stream:true, reasoning?}`
//! - 输入 item：user 消息（input_text/input_image）、assistant 消息
//!   （output_text）、`function_call`（call_id/name/arguments）、
//!   `function_call_output`（call_id/output）
//! - SSE：`response.output_item.added`（function_call 起）→
//!   `response.output_text.delta` / `response.function_call_arguments.delta`
//!   → `response.output_item.done`（function_call 收口）→
//!   `response.completed`（usage）/ `response.incomplete`（max_output_tokens）
//!   / `response.failed`（错误）。

use super::error::{classify, parse_retry_after, ApiError, ApiErrorKind};
use super::sse::{parse_sse, with_read_timeout, SseEvent};
use super::{
    join_url, BackendConfig, ModelBackend, StreamEvent, StreamRequest, STREAM_IDLE_TIMEOUT,
};
use crate::message::{ApiMessage, ContentBlock, StopReason, Usage};
use async_trait::async_trait;
use futures::stream::{BoxStream, StreamExt};
use serde_json::{json, Value};
use std::collections::HashMap;

pub struct ResponsesBackend {
    cfg: BackendConfig,
    http: reqwest::Client,
}

impl ResponsesBackend {
    pub fn new(cfg: &BackendConfig) -> Self {
        ResponsesBackend {
            cfg: cfg.clone(),
            http: super::http_client(),
        }
    }

    pub(crate) fn build_body(req: &StreamRequest) -> Value {
        let instructions = req.system.join("\n\n");
        let mut body = json!({
            "model": req.model,
            "stream": true,
            "input": serialize_input(&req.messages),
        });
        if !instructions.is_empty() {
            body["instructions"] = json!(instructions);
        }
        if !req.tools.is_empty() {
            body["tools"] = Value::Array(
                req.tools
                    .iter()
                    .map(|t| {
                        json!({
                            "type": "function",
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.parameters,
                        })
                    })
                    .collect(),
            );
        }
        if let Some(thinking) = req.thinking {
            body["reasoning"] = json!({ "effort": thinking.effort.as_str() });
        }
        body
    }

    async fn send(
        &self,
        req: StreamRequest,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ApiError>>, ApiError> {
        let url = join_url(&self.cfg.base_url, "/responses");
        let body = Self::build_body(&req);
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.cfg.api_key)
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
        Ok(ResponsesSse::default().into_stream(with_read_timeout(sse, STREAM_IDLE_TIMEOUT)))
    }
}

// ---------------------------------------------------------------------------
// 输入序列化：ApiMessage → Responses items
// ---------------------------------------------------------------------------

pub(crate) fn serialize_input(messages: &[ApiMessage]) -> Vec<Value> {
    let mut items: Vec<Value> = Vec::new();
    for msg in messages {
        match msg {
            ApiMessage::User { blocks } => {
                // 拆分：ToolResult → function_call_output；其余 → user 消息
                let mut content_parts: Vec<Value> = Vec::new();
                let mut text_acc = String::new();
                let flush_text = |acc: &mut String, parts: &mut Vec<Value>| {
                    if !acc.is_empty() {
                        parts.push(json!({"type": "input_text", "text": acc}));
                        acc.clear();
                    }
                };
                for block in blocks {
                    match block {
                        ContentBlock::Text { text } => {
                            if !text_acc.is_empty() {
                                text_acc.push('\n');
                            }
                            text_acc.push_str(text);
                        }
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            ..
                        } => {
                            flush_text(&mut text_acc, &mut content_parts);
                            items.push(json!({
                                "type": "function_call_output",
                                "call_id": tool_use_id,
                                "output": content,
                            }));
                        }
                        ContentBlock::Image { media_type, data } => {
                            flush_text(&mut text_acc, &mut content_parts);
                            content_parts.push(json!({
                                "type": "input_image",
                                "image_url": format!("data:{media_type};base64,{data}"),
                            }));
                        }
                        _ => {}
                    }
                }
                flush_text(&mut text_acc, &mut content_parts);
                if !content_parts.is_empty() {
                    items.push(json!({ "role": "user", "content": content_parts }));
                }
            }
            ApiMessage::Assistant { blocks, .. } => {
                let mut text_acc = String::new();
                let mut calls: Vec<Value> = Vec::new();
                for block in blocks {
                    match block {
                        ContentBlock::Text { text } => {
                            if !text_acc.is_empty() {
                                text_acc.push('\n');
                            }
                            text_acc.push_str(text);
                        }
                        ContentBlock::ToolUse { id, name, input } => {
                            calls.push(json!({
                                "type": "function_call",
                                "call_id": id,
                                "name": name,
                                "arguments": input.to_string(),
                            }));
                        }
                        _ => {}
                    }
                }
                if !text_acc.is_empty() {
                    items.push(json!({
                        "role": "assistant",
                        "content": [{ "type": "output_text", "text": text_acc }],
                    }));
                }
                items.extend(calls);
            }
        }
    }
    items
}

// ---------------------------------------------------------------------------
// SSE 事件机
// ---------------------------------------------------------------------------

/// outbox 模式：completed 事件要产出 Usage + Done 两个事件。
#[derive(Default)]
struct ResponsesSse {
    /// item_id → call_id（arguments delta 用 item_id 定位）
    item_to_call: HashMap<String, String>,
    saw_function_call: bool,
    usage: Usage,
    outbox: Vec<StreamEvent>,
    /// Done 是否已产出——incomplete/completed 先行收尾时，
    /// 流结束的兜底 finalize 不得再补一个 Done（否则主循环看到两次终止）。
    done_emitted: bool,
}

impl ResponsesSse {
    fn into_stream(
        self,
        sse: impl futures::Stream<Item = Result<SseEvent, ApiError>> + Unpin + Send + 'static,
    ) -> BoxStream<'static, Result<StreamEvent, ApiError>> {
        Box::pin(futures::stream::unfold(
            (self, sse, false),
            |(mut acc, mut sse, mut done)| async move {
                loop {
                    if let Some(ev) = acc.pop_outbox() {
                        return Some((Ok(ev), (acc, sse, done)));
                    }
                    if done {
                        return None;
                    }
                    match sse.next().await {
                        None => {
                            done = true;
                            acc.finalize();
                        }
                        Some(Err(e)) => return Some((Err(e), (acc, sse, true))),
                        Some(Ok(ev)) => {
                            if ev.data.trim() == "[DONE]" {
                                done = true;
                                acc.finalize();
                            } else if let Some(err) = acc.on_event(&ev.data) {
                                return Some((Err(err), (acc, sse, true)));
                            }
                        }
                    }
                }
            },
        ))
    }

    fn pop_outbox(&mut self) -> Option<StreamEvent> {
        if self.outbox.is_empty() {
            None
        } else {
            Some(self.outbox.remove(0))
        }
    }

    /// 兜底收尾：completed/incomplete 都没来时补 Done。
    fn finalize(&mut self) {
        if !self.done_emitted {
            let stop = if self.saw_function_call {
                StopReason::ToolUse
            } else {
                StopReason::EndTurn
            };
            self.push_done(stop);
        }
    }

    fn push_done(&mut self, stop_reason: StopReason) {
        if !self.done_emitted {
            self.done_emitted = true;
            self.outbox.push(StreamEvent::Done { stop_reason });
        }
    }

    /// 处理一条 SSE data。Ok 事件进 outbox；Some(err) 表示流中止。
    fn on_event(&mut self, data: &str) -> Option<ApiError> {
        let Ok(v) = serde_json::from_str::<Value>(data) else {
            return None;
        };
        let typ = v.get("type").and_then(|x| x.as_str())?;

        match typ {
            "response.output_item.added" => {
                let item = v.get("item")?;
                if item.get("type").and_then(|x| x.as_str()) == Some("function_call") {
                    let item_id = item.get("id").and_then(|x| x.as_str()).unwrap_or("");
                    let call_id = item
                        .get("call_id")
                        .and_then(|x| x.as_str())
                        .unwrap_or(item_id);
                    if !item_id.is_empty() {
                        self.item_to_call
                            .insert(item_id.to_string(), call_id.to_string());
                    }
                    self.saw_function_call = true;
                    let name = item
                        .get("name")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string();
                    self.outbox.push(StreamEvent::ToolUseStart {
                        id: call_id.to_string(),
                        name,
                    });
                }
                None
            }
            "response.output_text.delta" => {
                let text = v.get("delta").and_then(|x| x.as_str()).unwrap_or("");
                if !text.is_empty() {
                    self.outbox.push(StreamEvent::TextDelta(text.to_string()));
                }
                None
            }
            "response.reasoning_summary_text.delta" => {
                let text = v.get("delta").and_then(|x| x.as_str()).unwrap_or("");
                if !text.is_empty() {
                    self.outbox.push(StreamEvent::ThinkingDelta {
                        text: text.to_string(),
                    });
                }
                None
            }
            "response.function_call_arguments.delta" => {
                let item_id = v.get("item_id").and_then(|x| x.as_str()).unwrap_or("");
                let call_id = self
                    .item_to_call
                    .get(item_id)
                    .cloned()
                    .unwrap_or_else(|| item_id.to_string());
                let delta = v.get("delta").and_then(|x| x.as_str()).unwrap_or("");
                if !delta.is_empty() {
                    self.outbox.push(StreamEvent::ToolInputDelta {
                        id: call_id,
                        partial_json: delta.to_string(),
                    });
                }
                None
            }
            "response.output_item.done" => {
                let item = v.get("item")?;
                if item.get("type").and_then(|x| x.as_str()) == Some("function_call") {
                    let call_id = item
                        .get("call_id")
                        .and_then(|x| x.as_str())
                        .unwrap_or_else(|| item.get("id").and_then(|x| x.as_str()).unwrap_or(""));
                    let arguments = item.get("arguments").and_then(|x| x.as_str()).unwrap_or("");
                    let input: Value = if arguments.trim().is_empty() {
                        json!({})
                    } else {
                        serde_json::from_str(arguments).unwrap_or(json!({}))
                    };
                    self.outbox.push(StreamEvent::ToolUseDone {
                        id: call_id.to_string(),
                        input,
                    });
                }
                None
            }
            "response.completed" => {
                if let Some(usage) = v.pointer("/response/usage") {
                    self.usage.input_tokens = usage
                        .get("input_tokens")
                        .and_then(|x| x.as_u64())
                        .unwrap_or(0);
                    self.usage.output_tokens = usage
                        .get("output_tokens")
                        .and_then(|x| x.as_u64())
                        .unwrap_or(0);
                    self.usage.cache_read_input_tokens = usage
                        .pointer("/input_tokens_details/cached_tokens")
                        .and_then(|x| x.as_u64())
                        .unwrap_or(0);
                }
                let stop = if self.saw_function_call {
                    StopReason::ToolUse
                } else {
                    StopReason::EndTurn
                };
                self.outbox.push(StreamEvent::Usage(self.usage));
                self.push_done(stop);
                None
            }
            "response.incomplete" => {
                let reason = v
                    .pointer("/response/incomplete_details/reason")
                    .and_then(|x| x.as_str())
                    .unwrap_or("");
                let stop = match reason {
                    "max_output_tokens" => StopReason::MaxTokens,
                    _ => StopReason::Other,
                };
                self.push_done(stop);
                None
            }
            "response.failed" => {
                let msg = v
                    .pointer("/response/error/message")
                    .and_then(|x| x.as_str())
                    .unwrap_or("response failed");
                Some(ApiError::new(
                    ApiErrorKind::Protocol {
                        detail: msg.to_string(),
                    },
                    msg,
                ))
            }
            "error" => {
                let msg = v
                    .get("message")
                    .and_then(|x| x.as_str())
                    .unwrap_or("stream error");
                Some(ApiError::new(
                    ApiErrorKind::Protocol {
                        detail: msg.to_string(),
                    },
                    msg,
                ))
            }
            _ => None,
        }
    }
}

#[async_trait]
impl ModelBackend for ResponsesBackend {
    fn backend_name(&self) -> &'static str {
        "responses"
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
    use super::*;
    use crate::message::{normalize_for_api, Conversation, Message};
    use futures::stream;

    fn run(datas: Vec<&str>) -> Vec<Result<StreamEvent, ApiError>> {
        let sse = stream::iter(
            datas
                .into_iter()
                .map(SseEvent::data_only)
                .map(Ok)
                .collect::<Vec<_>>(),
        );
        futures::executor::block_on(async {
            ResponsesSse::default()
                .into_stream(sse)
                .collect::<Vec<_>>()
                .await
        })
    }

    #[test]
    fn serializes_function_call_and_output_as_native_items() {
        let mut conv = Conversation::new();
        conv.push(Message::user_text("list files"));
        conv.push(Message::new(
            crate::message::Role::Assistant,
            vec![
                ContentBlock::text("let me check"),
                ContentBlock::ToolUse {
                    id: "call_1".into(),
                    name: "glob".into(),
                    input: json!({"pattern": "*.rs"}),
                },
            ],
        ));
        conv.push(Message::new(
            crate::message::Role::User,
            vec![ContentBlock::tool_result("call_1", "main.rs")],
        ));
        let items = serialize_input(&normalize_for_api(&conv.active_messages()));
        assert_eq!(items[0]["role"], "user");
        assert_eq!(items[1]["role"], "assistant");
        assert_eq!(items[1]["content"][0]["type"], "output_text");
        assert_eq!(items[2]["type"], "function_call");
        assert_eq!(items[2]["call_id"], "call_1");
        assert_eq!(items[3]["type"], "function_call_output");
        assert_eq!(items[3]["call_id"], "call_1");
        assert_eq!(items[3]["output"], "main.rs");
    }

    #[test]
    fn tool_result_then_text_splits_into_output_and_user_msg() {
        let msgs = vec![ApiMessage::User {
            blocks: vec![
                ContentBlock::tool_result("c1", "out"),
                ContentBlock::text("and also:"),
            ],
        }];
        let items = serialize_input(&msgs);
        assert_eq!(items[0]["type"], "function_call_output");
        assert_eq!(items[1]["role"], "user");
    }

    #[test]
    fn stream_roundtrip_with_tool_call() {
        let out = run(vec![
            r#"{"type":"response.created","response":{"id":"r1"}}"#,
            r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_9","name":"bash","arguments":""}}"#,
            r#"{"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"{\"command\":"}"#,
            r#"{"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"\"ls\"}"}"#,
            r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_9","name":"bash","arguments":"{\"command\":\"ls\"}"}}"#,
            r#"{"type":"response.completed","response":{"usage":{"input_tokens":200,"output_tokens":30,"input_tokens_details":{"cached_tokens":100}}}}"#,
        ]);
        assert!(out.iter().any(|r| matches!(
            r.as_ref().unwrap(),
            StreamEvent::ToolUseStart { id, name } if id == "call_9" && name == "bash"
        )));
        let done = out
            .iter()
            .find_map(|r| match r.as_ref().unwrap() {
                StreamEvent::ToolUseDone { id, input } => Some((id.clone(), input.clone())),
                _ => None,
            })
            .unwrap();
        assert_eq!(done.0, "call_9");
        assert_eq!(done.1["command"], "ls");
        match out.last().unwrap().as_ref().unwrap() {
            StreamEvent::Done { stop_reason } => assert_eq!(*stop_reason, StopReason::ToolUse),
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[test]
    fn incomplete_maps_to_max_tokens() {
        let out = run(vec![
            r#"{"type":"response.output_text.delta","delta":"partial"}"#,
            r#"{"type":"response.incomplete","response":{"incomplete_details":{"reason":"max_output_tokens"}}}"#,
        ]);
        match out.last().unwrap().as_ref().unwrap() {
            StreamEvent::Done { stop_reason } => assert_eq!(*stop_reason, StopReason::MaxTokens),
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[test]
    fn failed_maps_to_error() {
        let out = run(vec![
            r#"{"type":"response.failed","response":{"error":{"message":"quota exceeded"}}}"#,
        ]);
        assert!(out[0].is_err());
    }

    #[test]
    fn done_sentinel_accepted() {
        let out = run(vec![
            r#"{"type":"response.output_text.delta","delta":"hi"}"#,
            "[DONE]",
        ]);
        match out.last().unwrap().as_ref().unwrap() {
            StreamEvent::Done { .. } => {}
            other => panic!("expected Done, got {other:?}"),
        }
    }
}
