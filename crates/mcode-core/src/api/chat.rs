//! Chat Completions 协议后端（vLLM / SGLang / one-api 等私有化网关）。
//!
//! 关键升级（相对旧版）：工具调用/结果用原生 `tool_calls` / `role:"tool"`
//! 消息结构化回喂，不再折叠成 user 文本。
//!
//! 兼容性取舍（延续旧版网关最大兼容策略）：
//! - 默认不发 `max_tokens`（避免 gpt-5 系 `max_completion_tokens` 分歧）；
//! - `reasoning_effort` 仅在显式配置 thinking 时发送；
//! - reasoning 增量兼容 `delta.reasoning_content`（DeepSeek）与
//!   `delta.reasoning`（字符串形态）两种键。

use super::error::{classify, parse_retry_after, ApiError, ApiErrorKind};
use super::sse::{parse_sse, with_read_timeout, SseEvent};
use super::{
    join_url, BackendConfig, ModelBackend, StreamEvent, StreamRequest, STREAM_IDLE_TIMEOUT,
};
use crate::message::{ApiMessage, ContentBlock, StopReason, Usage};
use async_trait::async_trait;
use futures::stream::{BoxStream, StreamExt};
use serde_json::{json, Value};
use std::collections::BTreeMap;

pub struct ChatBackend {
    cfg: BackendConfig,
    http: reqwest::Client,
}

impl ChatBackend {
    pub fn new(cfg: &BackendConfig) -> Self {
        ChatBackend {
            cfg: cfg.clone(),
            http: super::http_client(),
        }
    }

    pub(crate) fn build_body(req: &StreamRequest) -> Value {
        let mut messages: Vec<Value> = Vec::new();
        let system = req.system.join("\n\n");
        if !system.is_empty() {
            messages.push(json!({ "role": "system", "content": system }));
        }
        messages.extend(serialize_messages(&req.messages));

        let mut body = json!({
            "model": req.model,
            "stream": true,
            "messages": messages,
            "stream_options": { "include_usage": true },
        });
        if !req.tools.is_empty() {
            body["tools"] = Value::Array(
                req.tools
                    .iter()
                    .map(|t| {
                        json!({
                            "type": "function",
                            "function": {
                                "name": t.name,
                                "description": t.description,
                                "parameters": t.parameters,
                            },
                        })
                    })
                    .collect(),
            );
        }
        if let Some(thinking) = req.thinking {
            body["reasoning_effort"] = json!(thinking.effort.as_str());
        }
        body
    }

    async fn send(
        &self,
        req: StreamRequest,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ApiError>>, ApiError> {
        let url = join_url(&self.cfg.base_url, "/chat/completions");
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
        Ok(ChatSse::default().into_stream(with_read_timeout(sse, STREAM_IDLE_TIMEOUT)))
    }
}

// ---------------------------------------------------------------------------
// 消息序列化
// ---------------------------------------------------------------------------

pub(crate) fn serialize_messages(messages: &[ApiMessage]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for msg in messages {
        match msg {
            ApiMessage::User { blocks } => {
                // ToolResult → 独立 role:"tool" 消息；文本/图片 → user 消息
                let mut content_parts: Vec<Value> = Vec::new();
                let mut text_acc = String::new();
                for block in blocks {
                    match block {
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            ..
                        } => {
                            if !text_acc.is_empty() {
                                content_parts.push(json!({ "type": "text", "text": text_acc }));
                                text_acc.clear();
                            }
                            out.push(json!({
                                "role": "tool",
                                "tool_call_id": tool_use_id,
                                "content": content,
                            }));
                        }
                        ContentBlock::Text { text } => {
                            if !text_acc.is_empty() {
                                text_acc.push('\n');
                            }
                            text_acc.push_str(text);
                        }
                        ContentBlock::Image { media_type, data } => {
                            if !text_acc.is_empty() {
                                content_parts.push(json!({ "type": "text", "text": text_acc }));
                                text_acc.clear();
                            }
                            content_parts.push(json!({
                                "type": "image_url",
                                "image_url": { "url": format!("data:{media_type};base64,{data}") },
                            }));
                        }
                        _ => {}
                    }
                }
                if !text_acc.is_empty() {
                    content_parts.push(json!({ "type": "text", "text": text_acc }));
                }
                if content_parts.len() == 1 {
                    if let Some(text) = content_parts[0].get("text").and_then(|t| t.as_str()) {
                        out.push(json!({ "role": "user", "content": text }));
                        continue;
                    }
                }
                if !content_parts.is_empty() {
                    out.push(json!({ "role": "user", "content": content_parts }));
                }
            }
            ApiMessage::Assistant { blocks, .. } => {
                let mut text = String::new();
                let mut tool_calls: Vec<Value> = Vec::new();
                for block in blocks {
                    match block {
                        ContentBlock::Text { text: t } => {
                            if !text.is_empty() {
                                text.push('\n');
                            }
                            text.push_str(t);
                        }
                        ContentBlock::ToolUse { id, name, input } => {
                            tool_calls.push(json!({
                                "id": id,
                                "type": "function",
                                "function": { "name": name, "arguments": input.to_string() },
                            }));
                        }
                        _ => {} // thinking 不回喂（网关兼容）
                    }
                }
                let mut m = json!({ "role": "assistant" });
                m["content"] = if text.is_empty() {
                    Value::Null
                } else {
                    json!(text)
                };
                if !tool_calls.is_empty() {
                    m["tool_calls"] = Value::Array(tool_calls);
                }
                out.push(m);
            }
        }
    }
    out
}

fn map_finish_reason(raw: Option<&str>) -> StopReason {
    match raw {
        Some("tool_calls") | Some("function_call") => StopReason::ToolUse,
        Some("length") => StopReason::MaxTokens,
        Some("content_filter") => StopReason::Refusal,
        Some("stop") | None => StopReason::EndTurn,
        Some(_) => StopReason::Other,
    }
}

// ---------------------------------------------------------------------------
// SSE 事件机：tool_calls 按 index 槽位累积（分片跨 chunk 拼接）
// ---------------------------------------------------------------------------

#[derive(Default)]
struct ToolCallSlot {
    id: String,
    name: String,
    args: String,
    started: bool,
}

/// outbox 模式：一条 SSE data 可能产生多个 StreamEvent（如 finish 时
/// 收口多个 tool_calls 槽位 + Done），先入队、由 unfold 循环逐个弹出。
#[derive(Default)]
struct ChatSse {
    slots: BTreeMap<u64, ToolCallSlot>,
    finish_reason: Option<StopReason>,
    usage: Usage,
    outbox: Vec<StreamEvent>,
    /// Done 是否已产出——防 finalize 双重收尾（同 responses.rs）。
    done_emitted: bool,
}

impl ChatSse {
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

    /// 收尾：吐出全部未收口槽位 + 终止事件（部分网关不发 [DONE]，
    /// 也不发 finish_reason——两条路都由这里兜底）。
    fn finalize(&mut self) {
        let indexes: Vec<u64> = self.slots.keys().copied().collect();
        for i in indexes {
            if let Some(ev) = self.finish_slot(i) {
                self.outbox.push(ev);
            }
        }
        if !self.done_emitted {
            self.done_emitted = true;
            let stop = self.finish_reason.unwrap_or(StopReason::EndTurn);
            self.outbox.push(StreamEvent::Done { stop_reason: stop });
        }
    }

    fn finish_slot(&mut self, index: u64) -> Option<StreamEvent> {
        let slot = self.slots.remove(&index)?;
        let input: Value = if slot.args.trim().is_empty() {
            json!({})
        } else {
            serde_json::from_str(&slot.args).unwrap_or(json!({}))
        };
        Some(StreamEvent::ToolUseDone { id: slot.id, input })
    }

    /// 处理一条 SSE data。Ok 事件进 outbox；返回 Some(err) 表示流要中止。
    fn on_event(&mut self, data: &str) -> Option<ApiError> {
        let v: Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(_) => return None, // 非 JSON data（如注释）忽略
        };

        // usage 收尾块（choices 为空或缺失时也读取）
        if let Some(usage) = v.get("usage").filter(|u| !u.is_null()) {
            self.usage.input_tokens = usage
                .get("prompt_tokens")
                .and_then(|x| x.as_u64())
                .unwrap_or(0);
            self.usage.output_tokens = usage
                .get("completion_tokens")
                .and_then(|x| x.as_u64())
                .unwrap_or(0);
            self.usage.cache_read_input_tokens = usage
                .pointer("/prompt_tokens_details/cached_tokens")
                .and_then(|x| x.as_u64())
                .unwrap_or(0);
        }

        let choice = v
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|c| c.first())?;

        // 结束原因只记录、不提前返回：vLLM 网关会把最后一段 content 和
        // finish_reason 放进同一个 chunk，提前 return 会把正文吞掉
        if let Some(reason) = choice.get("finish_reason").filter(|r| !r.is_null()) {
            self.finish_reason = Some(map_finish_reason(reason.as_str()));
        }

        let delta = choice.get("delta")?;

        // reasoning 增量（deepseek: reasoning_content；其他: reasoning 字符串）
        for key in ["reasoning_content", "reasoning"] {
            if let Some(text) = delta.get(key).and_then(|x| x.as_str()) {
                if !text.is_empty() {
                    self.outbox.push(StreamEvent::ThinkingDelta {
                        text: text.to_string(),
                    });
                }
            }
        }

        // 文本增量
        if let Some(text) = delta.get("content").and_then(|x| x.as_str()) {
            if !text.is_empty() {
                self.outbox.push(StreamEvent::TextDelta(text.to_string()));
            }
        }

        // 工具调用分片
        if let Some(calls) = delta.get("tool_calls").and_then(|x| x.as_array()) {
            for call in calls {
                let index = call.get("index").and_then(|x| x.as_u64()).unwrap_or(0);
                let slot = self.slots.entry(index).or_default();
                if let Some(id) = call.get("id").and_then(|x| x.as_str()) {
                    if !id.is_empty() {
                        slot.id = id.to_string();
                    }
                }
                if let Some(name) = call.pointer("/function/name").and_then(|x| x.as_str()) {
                    if !name.is_empty() && slot.name.is_empty() {
                        slot.name = name.to_string();
                    }
                }
                let args_delta = call
                    .pointer("/function/arguments")
                    .and_then(|x| x.as_str())
                    .unwrap_or("");
                slot.args.push_str(args_delta);

                let id = if slot.id.is_empty() {
                    format!("call_{index}")
                } else {
                    slot.id.clone()
                };
                if !slot.started {
                    slot.started = true;
                    self.outbox.push(StreamEvent::ToolUseStart {
                        id: id.clone(),
                        name: slot.name.clone(),
                    });
                    // 首个分片可能已带非空 arguments——start 之后立刻补发，
                    // 否则 UI 累积的 partial_json 会永久丢失这一段
                    if !args_delta.is_empty() {
                        self.outbox.push(StreamEvent::ToolInputDelta {
                            id,
                            partial_json: args_delta.to_string(),
                        });
                    }
                } else if !args_delta.is_empty() {
                    self.outbox.push(StreamEvent::ToolInputDelta {
                        id,
                        partial_json: args_delta.to_string(),
                    });
                }
            }
        }

        None
    }
}

#[async_trait]
impl ModelBackend for ChatBackend {
    fn backend_name(&self) -> &'static str {
        "chat"
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
            ChatSse::default()
                .into_stream(sse)
                .collect::<Vec<_>>()
                .await
        })
    }

    #[test]
    fn finish_chunk_can_carry_final_content() {
        // vLLM 网关实测：content 与 finish_reason 同块到达，正文不能被吞
        let events = run(vec![
            r#"{"choices":[{"delta":{"role":"assistant","content":""}}]}"#,
            r#"{"choices":[{"delta":{"content":"ok"},"finish_reason":"stop"}]}"#,
            r#"{"choices":[],"usage":{"prompt_tokens":1,"completion_tokens":1}}"#,
            "[DONE]",
        ]);
        let texts: Vec<String> = events
            .iter()
            .filter_map(|e| match e.as_ref().ok() {
                Some(StreamEvent::TextDelta(t)) => Some(t.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["ok"], "{events:?}");
        assert!(matches!(
            events.last().map(|e| e.as_ref()),
            Some(Ok(StreamEvent::Done { .. }))
        ));
    }

    #[test]
    fn tool_results_become_role_tool_messages() {
        let mut conv = Conversation::new();
        conv.push(Message::user_text("run ls"));
        conv.push(Message::new(
            crate::message::Role::Assistant,
            vec![ContentBlock::ToolUse {
                id: "call_1".into(),
                name: "bash".into(),
                input: json!({"command": "ls"}),
            }],
        ));
        conv.push(Message::new(
            crate::message::Role::User,
            vec![ContentBlock::tool_result("call_1", "main.rs")],
        ));
        let msgs = serialize_messages(&normalize_for_api(&conv.active_messages()));
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[1]["role"], "assistant");
        assert_eq!(msgs[1]["tool_calls"][0]["id"], "call_1");
        assert_eq!(msgs[2]["role"], "tool");
        assert_eq!(msgs[2]["tool_call_id"], "call_1");
        assert_eq!(msgs[2]["content"], "main.rs");
    }

    #[test]
    fn image_becomes_image_url_part() {
        let msgs = vec![ApiMessage::User {
            blocks: vec![
                ContentBlock::text("看这张图"),
                ContentBlock::Image {
                    media_type: "image/png".into(),
                    data: "QUJD".into(),
                },
            ],
        }];
        let out = serialize_messages(&msgs);
        assert_eq!(out.len(), 1);
        assert!(out[0]["content"].is_array());
        assert_eq!(out[0]["content"][0]["type"], "text");
        assert_eq!(
            out[0]["content"][1]["image_url"]["url"],
            "data:image/png;base64,QUJD"
        );
    }

    #[test]
    fn stream_accumulates_split_tool_call() {
        let out = run(vec![
            r#"{"choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"callA","function":{"name":"grep","arguments":""}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"pattern\":"}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"foo\"}"}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":11,"completion_tokens":5,"prompt_tokens_details":{"cached_tokens":3}}}"#,
            "[DONE]",
        ]);
        assert!(out.iter().any(|r| matches!(
            r.as_ref().unwrap(),
            StreamEvent::ToolUseStart { id, name } if id == "callA" && name == "grep"
        )));
        let done = out
            .iter()
            .find_map(|r| match r.as_ref().unwrap() {
                StreamEvent::ToolUseDone { id, input } => Some((id.clone(), input.clone())),
                _ => None,
            })
            .unwrap();
        assert_eq!(done.0, "callA");
        assert_eq!(done.1["pattern"], "foo");
        match out.last().unwrap().as_ref().unwrap() {
            StreamEvent::Done { stop_reason } => assert_eq!(*stop_reason, StopReason::ToolUse),
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[test]
    fn reasoning_content_key_supported() {
        let out = run(vec![
            r#"{"choices":[{"index":0,"delta":{"reasoning_content":"想想"}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"content":"答"}}]}"#,
            "[DONE]",
        ]);
        assert!(out.iter().any(|r| matches!(
            r.as_ref().unwrap(),
            StreamEvent::ThinkingDelta { text } if text == "想想"
        )));
        assert!(out.iter().any(|r| matches!(
            r.as_ref().unwrap(),
            StreamEvent::TextDelta(t) if t == "答"
        )));
    }

    #[test]
    fn body_omits_max_tokens_and_sends_reasoning_only_when_set() {
        let req = StreamRequest {
            model: "deepseek-v4".into(),
            system: vec!["sys".into()],
            messages: vec![],
            tools: vec![],
            max_output_tokens: 8192,
            thinking: None,
        };
        let body = ChatBackend::build_body(&req);
        assert!(body.get("max_tokens").is_none());
        assert!(body.get("reasoning_effort").is_none());
        assert_eq!(body["messages"][0]["role"], "system");

        let req2 = StreamRequest {
            thinking: Some(super::super::ThinkingConfig {
                effort: super::super::ThinkingEffort::High,
            }),
            ..req
        };
        let body2 = ChatBackend::build_body(&req2);
        assert_eq!(body2["reasoning_effort"], "high");
    }
}
