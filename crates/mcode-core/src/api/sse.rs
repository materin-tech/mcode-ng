//! 最小 SSE 解析器：把 HTTP 字节流折叠成 `(event, data)` 事件。
//!
//! 三家协议（Anthropic / Responses / Chat）的流式响应全是 SSE 信封，
//! 差异只在 `data` 内部的 JSON 结构——解析器只做一层，协议语义归各后端。
//!
//! 细节：
//! - `\r\n` 与 `\n` 都按行界处理；
//! - `:` 开头注释行忽略；`id:`/`retry:` 等未知字段忽略；
//! - 空 行 = 事件边界，`data` 多行用 `\n` 连接；
//! - 部分网关不发结尾空行——流结束时 flush 未派发的事件；
//! - 块间空闲超时由 [`crate::api::sse::with_read_timeout`] 包装实现。

use super::error::{ApiError, ApiErrorKind};
use bytes::Bytes;
use futures::stream::{Stream, StreamExt};
use std::time::Duration;
use tokio::time::timeout;

#[derive(Clone, Debug, PartialEq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

impl SseEvent {
    pub fn data_only(data: impl Into<String>) -> Self {
        SseEvent {
            event: None,
            data: data.into(),
        }
    }
}

enum LineKind {
    Empty,
    Event(String),
    Data(String),
    Comment,
    Ignore,
}

fn classify_line(line: &str) -> LineKind {
    if line.is_empty() {
        return LineKind::Empty;
    }
    if line.starts_with(':') {
        return LineKind::Comment;
    }
    if let Some(rest) = line.strip_prefix("data:") {
        let data = rest.strip_prefix(' ').unwrap_or(rest);
        return LineKind::Data(data.to_string());
    }
    if let Some(rest) = line.strip_prefix("event:") {
        return LineKind::Event(rest.trim().to_string());
    }
    LineKind::Ignore
}

struct SseState<S> {
    stream: S,
    buf: Vec<u8>,
    event_name: Option<String>,
    data_lines: Vec<String>,
    finished: bool,
}

/// 字节流 → SSE 事件流。返回 BoxStream：unfold 产物不是 Unpin，
/// 上游（with_read_timeout / 各后端 into_stream）都按 Unpin 消费，
/// 在这里钉进 Box 一次，避免每个调用方各自 Box::pin。
pub fn parse_sse<S, E>(stream: S) -> futures::stream::BoxStream<'static, Result<SseEvent, ApiError>>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin + Send + 'static,
    E: std::fmt::Display + Send + 'static,
{
    let state = SseState {
        stream,
        buf: Vec::with_capacity(8 * 1024),
        event_name: None,
        data_lines: Vec::new(),
        finished: false,
    };
    Box::pin(futures::stream::unfold(state, |mut state| async move {
        loop {
            // 1) 先消化缓冲区里的完整行
            while let Some(pos) = state.buf.iter().position(|&b| b == b'\n') {
                let line_bytes: Vec<u8> = state.buf.drain(..=pos).collect();
                let raw = String::from_utf8_lossy(&line_bytes[..line_bytes.len() - 1]);
                let line = raw.trim_end_matches('\r');
                match classify_line(line) {
                    LineKind::Empty => {
                        if !state.data_lines.is_empty() || state.event_name.is_some() {
                            let ev = SseEvent {
                                event: state.event_name.take(),
                                data: state.data_lines.join("\n"),
                            };
                            state.data_lines.clear();
                            return Some((Ok(ev), state));
                        }
                    }
                    LineKind::Event(name) => state.event_name = Some(name),
                    LineKind::Data(data) => state.data_lines.push(data),
                    LineKind::Comment | LineKind::Ignore => {}
                }
            }
            if state.finished {
                return None;
            }
            // 2) 缓冲不足，拉更多字节
            match state.stream.next().await {
                Some(Ok(bytes)) => state.buf.extend_from_slice(&bytes),
                Some(Err(e)) => {
                    state.finished = true;
                    let err =
                        ApiError::new(ApiErrorKind::StreamAborted, format!("流读取失败: {e}"));
                    return Some((Err(err), state));
                }
                None => {
                    state.finished = true;
                    // 结尾无换行的残行：补一次行分类（部分网关最后一个
                    // data 行不带 \n 就直接关流）
                    if !state.buf.is_empty() {
                        let line_bytes: Vec<u8> = std::mem::take(&mut state.buf);
                        let raw = String::from_utf8_lossy(&line_bytes);
                        let line = raw.trim_end_matches('\r');
                        match classify_line(line) {
                            LineKind::Data(data) => state.data_lines.push(data),
                            LineKind::Event(name) => state.event_name = Some(name),
                            LineKind::Empty | LineKind::Comment | LineKind::Ignore => {}
                        }
                    }
                    // 结尾无空行的收尾事件（部分网关行为）
                    if !state.data_lines.is_empty() {
                        let ev = SseEvent {
                            event: state.event_name.take(),
                            data: state.data_lines.join("\n"),
                        };
                        state.data_lines.clear();
                        return Some((Ok(ev), state));
                    }
                }
            }
        }
    }))
}

/// 给任意 SSE 流加块间空闲超时：两块之间超过 `gap` 视为挂死，报 Timeout。
/// （总时长不加限制——长推理合法；挂死的连接才是要掐的。）
pub fn with_read_timeout<S>(
    stream: S,
    gap: Duration,
) -> futures::stream::BoxStream<'static, Result<SseEvent, ApiError>>
where
    S: Stream<Item = Result<SseEvent, ApiError>> + Unpin + Send + 'static,
{
    Box::pin(futures::stream::unfold(
        stream,
        move |mut stream| async move {
            match timeout(gap, stream.next()).await {
                Ok(Some(item)) => Some((item, stream)),
                Ok(None) => None,
                Err(_) => {
                    let err = ApiError::new(
                        ApiErrorKind::Timeout,
                        format!("流空闲超过 {gap:?}，判定挂死并中止"),
                    );
                    Some((Err(err), stream))
                }
            }
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream;

    fn byte_stream(chunks: Vec<&[u8]>) -> impl Stream<Item = Result<Bytes, std::io::Error>> {
        stream::iter(
            chunks
                .into_iter()
                .map(|c| Ok(Bytes::copy_from_slice(c)))
                .collect::<Vec<_>>(),
        )
    }

    async fn collect(chunks: Vec<&[u8]>) -> Vec<Result<SseEvent, ApiError>> {
        parse_sse(byte_stream(chunks)).collect().await
    }

    #[tokio::test]
    async fn basic_event_with_data() {
        let out = collect(vec![b"event: foo\ndata: {\"a\":1}\n\n"]).await;
        assert_eq!(out.len(), 1);
        let ev = out[0].as_ref().unwrap();
        assert_eq!(ev.event.as_deref(), Some("foo"));
        assert_eq!(ev.data, "{\"a\":1}");
    }

    #[tokio::test]
    async fn crlf_and_multiline_data() {
        let out = collect(vec![b"event: x\r\ndata: line1\r\ndata: line2\r\n\r\n"]).await;
        let ev = out[0].as_ref().unwrap();
        assert_eq!(ev.data, "line1\nline2");
    }

    #[tokio::test]
    async fn comments_and_unknown_fields_ignored() {
        let out = collect(vec![b": ping\ndata: 1\nid: 42\nretry: 99\n\n"]).await;
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].as_ref().unwrap().data, "1");
    }

    #[tokio::test]
    async fn chunk_split_midline() {
        let out = collect(vec![b"even", b"t: a\nda", b"ta: ok\n", b"\n"]).await;
        let ev = out[0].as_ref().unwrap();
        assert_eq!(ev.event.as_deref(), Some("a"));
        assert_eq!(ev.data, "ok");
    }

    #[tokio::test]
    async fn missing_final_blank_line_is_flushed() {
        let out = collect(vec![b"data: tail"]).await;
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].as_ref().unwrap().data, "tail");
    }

    #[tokio::test]
    async fn read_timeout_fires_on_hang() {
        // 永不产出字节的流 → 超时错误
        let sse = parse_sse(byte_stream(vec![]));
        let guarded = with_read_timeout(sse, Duration::from_millis(30));
        let out: Vec<_> = guarded.collect().await;
        assert_eq!(out.len(), 0); // 流直接结束，无超时——结束不算挂死

        // 用 pending 流模拟挂死
        let hung: std::pin::Pin<
            Box<dyn futures::Stream<Item = Result<SseEvent, ApiError>> + Send>,
        > = Box::pin(stream::pending());
        let guarded = with_read_timeout(hung, Duration::from_millis(30));
        let out: Vec<_> = guarded.take(1).collect().await;
        assert!(matches!(
            out[0],
            Err(ref e) if e.kind == ApiErrorKind::Timeout
        ));
    }
}
