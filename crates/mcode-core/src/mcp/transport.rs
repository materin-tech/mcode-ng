//! MCP 传输层：stdio（子进程，行分隔 JSON-RPC）+ HTTP（streamable，POST
//! JSON-RPC，响应 application/json 或 SSE）。
//!
//! 设计约束：
//! - 传输层只管「发请求收响应」（按 id 配对由 client 层做——stdio 需要
//!   读任务分派，HTTP 是请求响应一一对应）；
//! - stdio 子进程的 stderr 透传继承（诊断可见），进程退出即断连；
//! - HTTP 走 rustls（workspace 约束：禁 OpenSSL，musl 交叉编译兼容）。

use super::protocol::PROTOCOL_VERSION;
use anyhow::{Context as _, Result};
use serde_json::Value;
use std::collections::BTreeMap;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{oneshot, Mutex};

/// 单请求超时（对工具调用足够宽；断连由错误触发重连，不靠超时兜底）。
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

// ---------------------------------------------------------------------------
// 传输抽象
// ---------------------------------------------------------------------------

#[async_trait::async_trait]
pub trait McpTransport: Send + Sync {
    /// 发送一条 JSON-RPC 请求并等待响应（原始 Value）。断连/超时 → Err。
    async fn send(&self, body: Value) -> Result<Value>;
    /// 发送通知（无 id，不等待响应；stdio 只写行，HTTP 容忍 202/空体）。
    async fn notify(&self, body: Value) -> Result<()>;
    /// 传输是否仍然健康（stdio：子进程存活；HTTP：恒真——错误即重连）。
    fn is_healthy(&self) -> bool;
    /// 关闭传输（stdio：先礼后兵关 stdin，3s 不退则 kill）。
    async fn shutdown(&self);
}

// ---------------------------------------------------------------------------
// stdio
// ---------------------------------------------------------------------------

/// stdio 传输：子进程 stdin/stdout 行分隔 JSON-RPC。
///
/// 读任务持续解析 stdout 行，按 id 分派到等待者；无 id 的通知/日志行忽略。
pub struct StdioTransport {
    /// 写端（锁序列化并发写入）。
    stdin: Mutex<tokio::process::ChildStdin>,
    /// id → 响应等待者（读任务投递，请求侧按 id 取）。
    pending: Arc<Mutex<BTreeMap<u64, oneshot::Sender<Result<Value>>>>>,
    child: Mutex<tokio::process::Child>,
}

impl StdioTransport {
    /// 启动子进程并拉起读任务。失败（命令不存在/管道建立失败）→ Err。
    pub fn spawn(command: &str, args: &[String], env: &BTreeMap<String, String>) -> Result<Self> {
        let mut cmd = tokio::process::Command::new(command);
        cmd.args(args)
            .envs(env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        crate::util::hide_console_window(&mut cmd);

        let mut child = cmd
            .spawn()
            .with_context(|| format!("启动 MCP server `{command}` 失败"))?;
        let stdin = child.stdin.take().context("MCP 子进程 stdin 管道缺失")?;
        let stdout = child.stdout.take().context("MCP 子进程 stdout 管道缺失")?;

        let pending: Arc<Mutex<BTreeMap<u64, oneshot::Sender<Result<Value>>>>> =
            Arc::new(Mutex::new(BTreeMap::new()));
        let reader_pending = pending.clone();
        tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, BufReader};
            let mut lines = BufReader::new(stdout).lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        if let Ok(v) = serde_json::from_str::<Value>(&line) {
                            if let Some(id) = v.get("id").and_then(Value::as_u64) {
                                let mut map = reader_pending.lock().await;
                                if let Some(tx) = map.remove(&id) {
                                    let _ = tx.send(Ok(v));
                                }
                            }
                            // 无 id：通知/日志——宽容忽略（协议前向兼容）
                        }
                        // 非 JSON 行（server 的调试输出）：宽容忽略
                    }
                    Ok(None) | Err(_) => {
                        // stdout 关闭：所有等待者报断连（整体换出 pending，
                        // BTreeMap::drain 在 stable 不可用）
                        let mut map = reader_pending.lock().await;
                        for (_, tx) in std::mem::take(&mut *map) {
                            let _ = tx.send(Err(anyhow::anyhow!("MCP server 进程已退出")));
                        }
                        return;
                    }
                }
            }
        });

        Ok(Self {
            stdin: Mutex::new(stdin),
            pending,
            child: Mutex::new(child),
        })
    }
}

#[async_trait::async_trait]
impl McpTransport for StdioTransport {
    async fn send(&self, body: Value) -> Result<Value> {
        let id = body
            .get("id")
            .and_then(Value::as_u64)
            .context("请求缺 id")?;
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        use tokio::io::AsyncWriteExt;
        let mut line = serde_json::to_string(&body)?;
        line.push('\n');
        {
            let write = {
                let mut si = self.stdin.lock().await;
                async {
                    si.write_all(line.as_bytes()).await?;
                    si.flush().await
                }
                .await
            };
            if let Err(e) = write {
                // 写失败：撤单等（防悬挂条目），错误交由调用方触发重连
                self.pending.lock().await.remove(&id);
                anyhow::bail!("写 MCP server stdin 失败: {e}");
            }
        }

        let result = match tokio::time::timeout(DEFAULT_REQUEST_TIMEOUT, rx).await {
            Ok(Ok(res)) => res,
            Ok(Err(_)) => Err(anyhow::anyhow!("MCP 响应等待者被弃")),
            Err(_) => Err(anyhow::anyhow!(
                "MCP 请求超时（{}s）",
                DEFAULT_REQUEST_TIMEOUT.as_secs()
            )),
        };
        // 无论成败都清掉本请求的等待者（超时/断连后不留悬挂条目）
        self.pending.lock().await.remove(&id);
        result
    }

    async fn notify(&self, body: Value) -> Result<()> {
        use tokio::io::AsyncWriteExt;
        let mut line = serde_json::to_string(&body)?;
        line.push('\n');
        let mut si = self.stdin.lock().await;
        async {
            si.write_all(line.as_bytes()).await?;
            si.flush().await
        }
        .await
        .map_err(|e| anyhow::anyhow!("写 MCP server stdin 失败: {e}"))
    }

    fn is_healthy(&self) -> bool {
        // 锁被占用（正在 shutdown）→ 不干预；能拿到锁就探进程状态
        self.child
            .try_lock()
            .map(|mut c| c.try_wait().map(|s| s.is_none()).unwrap_or(true))
            .unwrap_or(true)
    }

    async fn shutdown(&self) {
        use tokio::io::AsyncWriteExt;
        // 先关 stdin 让 server 自然退出；3s 不退则 kill
        if let Ok(mut si) = self.stdin.try_lock() {
            let _ = si.shutdown().await;
        }
        let mut child = self.child.lock().await;
        if tokio::time::timeout(Duration::from_secs(3), child.wait())
            .await
            .is_err()
        {
            let _ = child.kill().await;
        }
    }
}

// ---------------------------------------------------------------------------
// HTTP（streamable）
// ---------------------------------------------------------------------------

/// streamable HTTP 传输：POST JSON-RPC；响应 application/json 直接收，
/// text/event-stream 解析 SSE 帧取匹配 id 的响应。
pub struct HttpTransport {
    url: String,
    headers: BTreeMap<String, String>,
    client: reqwest::Client,
}

/// 提取一段 SSE 文本里的 `data:` 载荷（多行 data 合并；注释/空行忽略）。
pub fn parse_sse_data(chunk: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for line in chunk.lines() {
        if let Some(data) = line.strip_prefix("data:") {
            let data = data.strip_prefix(' ').unwrap_or(data);
            cur.push_str(data);
            cur.push('\n');
        } else if line.trim().is_empty() && !cur.is_empty() {
            out.push(cur.trim_end_matches('\n').to_string());
            cur.clear();
        }
    }
    if !cur.is_empty() {
        out.push(cur.trim_end_matches('\n').to_string());
    }
    out
}

/// 找到 buf 中第一个完整 SSE 帧（以空行结尾）的边界；无完整帧 → None。
fn frame_end(buf: &str) -> Option<usize> {
    let b = buf.as_bytes();
    let mut i = 0;
    while i + 1 < b.len() {
        if b[i] == b'\n' {
            if b[i + 1] == b'\n' {
                return Some(i + 2);
            }
            if b[i + 1] == b'\r' && i + 2 < b.len() && b[i + 2] == b'\n' {
                return Some(i + 3);
            }
        }
        i += 1;
    }
    None
}

impl HttpTransport {
    pub fn new(url: String, headers: BTreeMap<String, String>) -> Result<Self> {
        let mut ua = reqwest::header::HeaderMap::new();
        ua.insert(
            reqwest::header::ACCEPT,
            "application/json, text/event-stream"
                .parse()
                .expect("静态 header 值"),
        );
        ua.insert(
            reqwest::header::CONTENT_TYPE,
            "application/json".parse().expect("静态 header 值"),
        );
        ua.insert(
            "MCP-Protocol-Version",
            PROTOCOL_VERSION.parse().expect("静态 protocol version"),
        );
        let client = reqwest::Client::builder()
            .timeout(DEFAULT_REQUEST_TIMEOUT)
            .default_headers(ua)
            .build()?;
        Ok(Self {
            url,
            headers,
            client,
        })
    }
}

#[async_trait::async_trait]
impl McpTransport for HttpTransport {
    async fn send(&self, body: Value) -> Result<Value> {
        let expect_id = body.get("id").and_then(Value::as_u64);
        let mut req = self.client.post(&self.url).json(&body);
        for (k, v) in &self.headers {
            req = req.header(k, v);
        }
        let resp = req
            .send()
            .await
            .with_context(|| format!("请求 MCP endpoint {} 失败", self.url))?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("MCP endpoint 返回 {}: {}", status, text);
        }

        let ctype = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        if ctype.contains("text/event-stream") {
            // SSE 流：按完整帧切（帧可跨 chunk），取匹配 id 的 JSON-RPC 响应
            let mut stream = resp.bytes_stream();
            let mut buf = String::new();
            use futures::StreamExt;
            while let Some(chunk) = stream.next().await {
                buf.push_str(&String::from_utf8_lossy(&chunk?));
                while let Some(end) = frame_end(&buf) {
                    let frame: String = buf.drain(..end).collect();
                    for data in parse_sse_data(&frame) {
                        if let Ok(v) = serde_json::from_str::<Value>(&data) {
                            let id = v.get("id").and_then(Value::as_u64);
                            if id.is_some() && id == expect_id {
                                return Ok(v);
                            }
                            // 通知/其他 id 的帧：宽容忽略（协议允许服务器推事件）
                        }
                    }
                }
            }
            anyhow::bail!("SSE 流结束仍未收到 id={expect_id:?} 的响应")
        }

        // application/json：直接解析
        let v: Value = resp
            .json()
            .await
            .with_context(|| "解析 MCP 响应 JSON 失败")?;
        Ok(v)
    }

    async fn notify(&self, body: Value) -> Result<()> {
        // 通知：server 通常回 202 Accepted（可能无 body）——只要非 4xx/5xx 即成功
        let mut req = self.client.post(&self.url).json(&body);
        for (k, v) in &self.headers {
            req = req.header(k, v);
        }
        let resp = req
            .send()
            .await
            .with_context(|| format!("发送通知到 {} 失败", self.url))?;
        let status = resp.status();
        if status.is_client_error() || status.is_server_error() {
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("MCP endpoint 通知返回 {}: {}", status, text);
        }
        Ok(())
    }

    fn is_healthy(&self) -> bool {
        true // HTTP 无连接态；失败由 send 报错触发重连（重连=重新初始化会话）
    }

    async fn shutdown(&self) {}
}

// ---------------------------------------------------------------------------
// 便捷构造
// ---------------------------------------------------------------------------

/// 按配置建传输（HTTP url 优先，否则 stdio 子进程）。
pub fn spawn_transport(
    command: &str,
    args: &[String],
    env: &BTreeMap<String, String>,
    url: Option<String>,
    headers: &BTreeMap<String, String>,
) -> Result<Arc<dyn McpTransport>> {
    match url {
        Some(u) if !u.is_empty() => Ok(Arc::new(HttpTransport::new(u, headers.clone())?)),
        _ => Ok(Arc::new(StdioTransport::spawn(command, args, env)?)),
    }
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_data_extraction() {
        let frames = parse_sse_data("data: {\"a\":1}\n\n: comment\ndata: first\ndata: second\n\n");
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0], "{\"a\":1}");
        assert_eq!(frames[1], "first\nsecond");
    }

    #[test]
    fn sse_crlf_and_no_trailing_newline() {
        let frames = parse_sse_data("data: x\r\n\r\ndata: tail");
        assert_eq!(frames, vec!["x".to_string(), "tail".to_string()]);
    }

    #[test]
    fn frame_boundaries_across_chunk_splits() {
        // 帧被拆到两个 chunk：第一次不完整 → None；补齐后 → 边界 2
        assert_eq!(frame_end("dat"), None);
        assert_eq!(frame_end("data: 1\n"), None);
        assert_eq!(frame_end("data: 1\n\nrest"), Some(9));
        assert_eq!(frame_end("data: 1\r\n\r\nrest"), Some(11));
    }

    // ---------------- stdio 集成：shell 假 server ----------------

    /// 单行 JSON-RPC 假 server（Unix shell 版）：握手 → 列工具 → 调用。
    /// （无通知吸收行——传输层测试直接按请求逐行应答）
    fn fake_server_script() -> String {
        let init = r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-03-26","capabilities":{"tools":{}},"serverInfo":{"name":"fake","version":"0.1"}}}"#;
        let list = r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"upper","description":"转大写","inputSchema":{"type":"object","properties":{"text":{"type":"string"}}},"annotations":{"readOnlyHint":true}}]}}"#;
        let call = r#"{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"HELLO"}],"isError":false}}"#;
        format!(
            "read _ && echo '{init}'\nread _ && echo '{list}'\nread _ && echo '{call}'\nsleep 60\n"
        )
    }

    #[tokio::test]
    async fn stdio_transport_full_handshake_and_call() {
        let t = StdioTransport::spawn(
            "sh",
            &["-c".to_string(), fake_server_script()],
            &Default::default(),
        )
        .unwrap();
        assert!(t.is_healthy());

        let init = super::super::protocol::request(
            1,
            "initialize",
            Some(serde_json::json!({"protocolVersion": super::super::protocol::PROTOCOL_VERSION})),
        );
        let resp = t.send(serde_json::to_value(&init).unwrap()).await.unwrap();
        assert_eq!(resp["result"]["serverInfo"]["name"], "fake");

        let list = super::super::protocol::request(2, "tools/list", None);
        let resp = t.send(serde_json::to_value(&list).unwrap()).await.unwrap();
        assert_eq!(resp["result"]["tools"][0]["name"], "upper");

        let call = super::super::protocol::request(
            3,
            "tools/call",
            Some(serde_json::json!({"name":"upper","arguments":{"text":"hello"}})),
        );
        let resp = t.send(serde_json::to_value(&call).unwrap()).await.unwrap();
        assert_eq!(resp["result"]["content"][0]["text"], "HELLO");

        t.shutdown().await;
    }

    #[tokio::test]
    async fn stdio_spawn_failure_is_contextful_error() {
        let err = StdioTransport::spawn("mcode-no-such-mcp-cmd-xyz", &[], &Default::default())
            .err()
            .expect("spawn 不存在的命令应失败");
        assert!(err.to_string().contains("启动 MCP server"), "{err}");
    }

    // ---------------- HTTP 集成：手写 TCP 假 server ----------------

    /// 起一个一次性 HTTP server：读一个 POST，回一条 JSON 响应（或 SSE）。
    async fn one_shot_http_server(
        response_body: String,
        content_type: &'static str,
    ) -> (u16, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            // 读到请求头结束 + body（content-length 定长）
            let mut buf = vec![0u8; 65536];
            let mut req = Vec::new();
            loop {
                let n = sock.read(&mut buf).await.unwrap();
                if n == 0 {
                    break;
                }
                req.extend_from_slice(&buf[..n]);
                let head_end = req.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4);
                if let Some(hd) = head_end {
                    let clen = String::from_utf8_lossy(&req[..hd])
                        .to_lowercase()
                        .lines()
                        .find_map(|l| {
                            l.strip_prefix("content-length:")
                                .map(|v| v.trim().to_string())
                        })
                        .and_then(|v| v.parse::<usize>().ok())
                        .unwrap_or(0);
                    if req.len() >= hd + clen {
                        break;
                    }
                }
            }
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
                response_body.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
            let _ = sock.shutdown().await;
        });
        (port, handle)
    }

    #[tokio::test]
    async fn http_transport_plain_json_response() {
        let body = r#"{"jsonrpc":"2.0","id":7,"result":{"content":[{"type":"text","text":"ok"}]}}"#
            .to_string();
        let (port, handle) = one_shot_http_server(body, "application/json").await;
        let t =
            HttpTransport::new(format!("http://127.0.0.1:{port}/mcp"), Default::default()).unwrap();
        let req = super::super::protocol::request(7, "tools/call", None);
        let resp = t.send(serde_json::to_value(&req).unwrap()).await.unwrap();
        assert_eq!(resp["result"]["content"][0]["text"], "ok");
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn http_transport_sse_response() {
        // 响应是 SSE：一帧通知（无匹配 id）+ 一帧目标响应
        let sse = "data: {\"jsonrpc\":\"2.0\",\"method\":\"log\"}\n\ndata: {\"jsonrpc\":\"2.0\",\"id\":9,\"result\":{\"ok\":true}}\n\n";
        let (port, handle) = one_shot_http_server(sse.to_string(), "text/event-stream").await;
        let t =
            HttpTransport::new(format!("http://127.0.0.1:{port}/mcp"), Default::default()).unwrap();
        let req = super::super::protocol::request(9, "tools/list", None);
        let resp = t.send(serde_json::to_value(&req).unwrap()).await.unwrap();
        assert_eq!(resp["result"]["ok"], true);
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn http_error_status_surfaces_body() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 16384];
            let _ = sock.read(&mut buf).await;
            let _ = sock
                .write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 5\r\nConnection: close\r\n\r\nboom!")
                .await;
            let _ = sock.shutdown().await;
        });
        let t =
            HttpTransport::new(format!("http://127.0.0.1:{port}/mcp"), Default::default()).unwrap();
        let req = super::super::protocol::request(1, "tools/list", None);
        let err = t
            .send(serde_json::to_value(&req).unwrap())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("500"), "{err}");
        assert!(err.to_string().contains("boom!"), "{err}");
        server.await.unwrap();
    }
}
