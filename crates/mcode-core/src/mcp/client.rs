//! MCP 客户端：懒连接 + 失效即重连。
//!
//! 连接策略（对齐 CC 的 mcp client）：
//! - **懒连接**：首次操作才 spawn 子进程/建 HTTP 会话并完成 initialize
//!   握手——配置了但没用到的 server 零开销；
//! - **失效即重连**：操作失败（进程退出/网络错误/超时）→ 丢弃连接态，
//!   用新连接重试一次；再失败才把组合错误抛给调用方。重试只做一次，
//!   防止半死 server 把每次工具调用都拖成双重超时；
//! - 同一 client 的操作串行（Mutex 持锁跨 await）——MCP server 多为
//!   单进程单线程，并发请求没有收益，串行换来握手/重连状态机的简单可靠。

use super::protocol::{
    notification, request, InitializeResult, JsonRpcResponse, McpToolDef, McpToolResult,
    ServerInfo, PROTOCOL_VERSION,
};
use super::transport::{spawn_transport, McpTransport};
use anyhow::{Context as _, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::Mutex;

/// 一个 MCP server 的连接参数（从 [`crate::config::McpServerConfig`] 提取）。
#[derive(Debug, Clone, Default)]
pub struct McpConnectParams {
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub url: Option<String>,
    pub headers: BTreeMap<String, String>,
}

/// 传输工厂（生产 = [`spawn_transport`]；测试注入脚本化 mock）。
pub type MakeTransport =
    Arc<dyn Fn(&McpConnectParams) -> Result<Arc<dyn McpTransport>> + Send + Sync>;

enum ConnectionState {
    Disconnected,
    Connected {
        transport: Arc<dyn McpTransport>,
        server: InitializeResult,
        /// 下一个请求 id（1 被 initialize 占用）
        next_id: u64,
    },
}

/// 单 server 的 MCP 客户端。`Clone` 共享连接态。
#[derive(Clone)]
pub struct McpClient {
    params: McpConnectParams,
    make_transport: MakeTransport,
    state: Arc<Mutex<ConnectionState>>,
}

impl McpClient {
    pub fn new(params: McpConnectParams) -> Self {
        Self::with_factory(
            params,
            Arc::new(|p| spawn_transport(&p.command, &p.args, &p.env, p.url.clone(), &p.headers)),
        )
    }

    /// 测试/高级用法：自定义传输工厂。
    pub fn with_factory(params: McpConnectParams, make_transport: MakeTransport) -> Self {
        Self {
            params,
            make_transport,
            state: Arc::new(Mutex::new(ConnectionState::Disconnected)),
        }
    }

    /// 当前 server 名（诊断用：stdio 用命令名，HTTP 用 URL）。
    pub fn display_name(&self) -> String {
        match &self.params.url {
            Some(u) if !u.is_empty() => u.clone(),
            _ => self.params.command.clone(),
        }
    }

    /// 列出工具（连接失效自动重连重试一次）。
    pub async fn list_tools(&self) -> Result<Vec<McpToolDef>> {
        let res = self
            .rpc_with_reconnect("tools/list", Some(json!({})))
            .await
            .context("tools/list 失败")?;
        #[derive(serde::Deserialize)]
        struct ToolsPayload {
            #[serde(default)]
            tools: Vec<McpToolDef>,
        }
        let payload: ToolsPayload =
            serde_json::from_value(res).context("解析 tools/list 响应失败")?;
        Ok(payload.tools)
    }

    /// 调用工具（连接失效自动重连重试一次）。
    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<McpToolResult> {
        let res = self
            .rpc_with_reconnect(
                "tools/call",
                Some(json!({ "name": name, "arguments": arguments })),
            )
            .await
            .with_context(|| format!("tools/call({name}) 失败"))?;
        serde_json::from_value(res).context("解析 tools/call 响应失败")
    }

    /// 主动断连（会话关闭/配置变更时调用；下次操作自动重连）。
    pub async fn disconnect(&self) {
        let mut st = self.state.lock().await;
        if let ConnectionState::Connected { transport, .. } = &*st {
            transport.shutdown().await;
        }
        *st = ConnectionState::Disconnected;
    }

    /// 当前连接的 server 信息（未连接 → None；`/mcp` 状态展示用）。
    pub async fn server_info(&self) -> Option<ServerInfo> {
        let st = self.state.lock().await;
        match &*st {
            ConnectionState::Connected { server, .. } => Some(server.server_info.clone()),
            ConnectionState::Disconnected => None,
        }
    }

    /// RPC + 失效即重连。首连失败直接报错（spawn/握手失败是确定性故障，
    /// 重试无意义）；已连但 RPC 失败 → 丢弃连接、重连、重试一次。
    async fn rpc_with_reconnect(&self, method: &str, params: Option<Value>) -> Result<Value> {
        let mut st = self.state.lock().await;

        // 1) 懒连接 / 健康检查
        let needs_connect = match &*st {
            ConnectionState::Disconnected => true,
            ConnectionState::Connected { transport, .. } => !transport.is_healthy(),
        };
        if needs_connect {
            if let ConnectionState::Connected { transport, .. } = &*st {
                transport.shutdown().await;
            }
            *st = self.connect().await?;
        }

        // 2) 首次尝试
        let first_err = match self.rpc_locked(&mut st, method, params.clone()).await {
            Ok(v) => return Ok(v),
            Err(e) => e,
        };

        // 3) 失效即重连：新连接重试一次
        if let ConnectionState::Connected { transport, .. } = &*st {
            transport.shutdown().await;
        }
        *st = self.connect().await?;
        self.rpc_locked(&mut st, method, params)
            .await
            .map_err(|second| anyhow::anyhow!("{first_err}；重连后重试仍失败: {second}"))
    }

    /// 持锁状态下发起 RPC（id 分配 + JSON-RPC 解析）。
    async fn rpc_locked(
        &self,
        st: &mut ConnectionState,
        method: &str,
        params: Option<Value>,
    ) -> Result<Value> {
        let ConnectionState::Connected {
            transport, next_id, ..
        } = st
        else {
            anyhow::bail!("连接态异常");
        };
        let id = *next_id;
        *next_id += 1;
        let resp = transport
            .send(serde_json::to_value(request(id, method, params))?)
            .await?;
        // 传输层回的是完整 JSON-RPC 信封（Value）——这里解出 result/error
        serde_json::from_value::<JsonRpcResponse>(resp)
            .context("解析 JSON-RPC 响应信封失败")?
            .into_result()
    }

    /// 建立连接并完成 initialize 握手。
    async fn connect(&self) -> Result<ConnectionState> {
        let transport = (self.make_transport)(&self.params).context("建立 MCP 传输失败")?;

        let init_params = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "mcode", "version": env!("CARGO_PKG_VERSION") },
        });
        let resp = transport
            .send(serde_json::to_value(request(
                1,
                "initialize",
                Some(init_params),
            ))?)
            .await
            .context("initialize 握手失败")?;
        let init: InitializeResult = serde_json::from_value(
            serde_json::from_value::<JsonRpcResponse>(resp)
                .context("解析 initialize 响应信封失败")?
                .into_result()?,
        )
        .context("解析 initialize 响应失败")?;
        transport
            .notify(serde_json::to_value(notification(
                "notifications/initialized",
                None,
            ))?)
            .await
            .context("发送 initialized 通知失败")?;

        Ok(ConnectionState::Connected {
            transport,
            server: init,
            next_id: 2,
        })
    }
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// 脚本化传输：按预置队列回放；`calls` 记录方法名供断言。
    struct MockTransport {
        responses: std::sync::Mutex<VecDeque<Result<Value>>>,
        pub calls: std::sync::Mutex<Vec<String>>,
    }
    impl MockTransport {
        fn arc(responses: Vec<Result<Value>>) -> Arc<Self> {
            Arc::new(Self {
                responses: std::sync::Mutex::new(responses.into_iter().collect()),
                calls: std::sync::Mutex::new(vec![]),
            })
        }
    }
    #[async_trait::async_trait]
    impl McpTransport for MockTransport {
        async fn send(&self, body: Value) -> Result<Value> {
            self.record(body);
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Err(anyhow::anyhow!("脚本已空（模拟断连）")))
        }
        async fn notify(&self, body: Value) -> Result<()> {
            // 通知只记录不消耗脚本（fire-and-forget）
            self.record(body);
            Ok(())
        }
        fn is_healthy(&self) -> bool {
            true
        }
        async fn shutdown(&self) {}
    }
    impl MockTransport {
        fn record(&self, body: Value) {
            self.calls
                .lock()
                .unwrap()
                .push(body["method"].as_str().unwrap_or("?").to_string());
        }
    }

    fn init_ok() -> Value {
        serde_json::json!({"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-03-26","capabilities":{"tools":{}},"serverInfo":{"name":"mock"}}})
    }

    fn tools_ok() -> Value {
        serde_json::json!({"jsonrpc":"2.0","id":2,"result":{"tools":[
            {"name":"upper","description":"转大写","inputSchema":{"type":"object"},"annotations":{"readOnlyHint":true}}
        ]}})
    }

    /// 工厂脚本：每次 connect() 依次返回一个 mock。
    fn factory_script(mocks: Vec<Arc<MockTransport>>) -> MakeTransport {
        let queue = std::sync::Mutex::new(mocks.into_iter().collect::<VecDeque<_>>());
        Arc::new(move |_| {
            queue
                .lock()
                .unwrap()
                .pop_front()
                .map(|m| m as Arc<dyn McpTransport>)
                .ok_or_else(|| anyhow::anyhow!("工厂脚本已空"))
        })
    }

    fn params() -> McpConnectParams {
        McpConnectParams {
            command: "mock".into(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn list_tools_parses_and_marks_read_only_hint() {
        let mock = MockTransport::arc(vec![Ok(init_ok()), Ok(tools_ok())]);
        let c = McpClient::with_factory(params(), factory_script(vec![mock.clone()]));
        let tools = c.list_tools().await.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "upper");
        assert_eq!(
            tools[0].annotations.as_ref().unwrap().read_only_hint,
            Some(true)
        );
        assert_eq!(
            mock.calls.lock().unwrap().clone(),
            // 真实顺序：connect 时 initialize → notifications/initialized，然后 tools/list
            vec!["initialize", "notifications/initialized", "tools/list"]
        );
        // 握手信息可读（/mcp 状态展示的数据源）
        assert_eq!(c.server_info().await.unwrap().name, "mock");
    }

    #[tokio::test]
    async fn rpc_failure_reconnects_once_and_retries() {
        // 第一条连接：握手成功，tools/list 报断连；第二条连接：完整成功
        let bad = MockTransport::arc(vec![Ok(init_ok()), Err(anyhow::anyhow!("进程退出"))]);
        let good = MockTransport::arc(vec![Ok(init_ok()), Ok(tools_ok())]);
        let c = McpClient::with_factory(params(), factory_script(vec![bad.clone(), good.clone()]));
        let tools = c.list_tools().await.unwrap();
        assert_eq!(tools[0].name, "upper");
        // 第一条连接走到 initialize + 通知 + 失败的 tools/list；第二条走完握手 + 重试
        assert_eq!(bad.calls.lock().unwrap().len(), 3);
        assert_eq!(
            good.calls.lock().unwrap().clone(),
            vec!["initialize", "notifications/initialized", "tools/list"]
        );
    }

    #[tokio::test]
    async fn retry_exhausted_surfaces_combined_error() {
        let bad1 = MockTransport::arc(vec![Ok(init_ok()), Err(anyhow::anyhow!("断连一"))]);
        let bad2 = MockTransport::arc(vec![Ok(init_ok()), Err(anyhow::anyhow!("断连二"))]);
        let c = McpClient::with_factory(params(), factory_script(vec![bad1, bad2]));
        let err = c.list_tools().await.unwrap_err();
        // {:#} 打印整条 context 链（to_string 只有最外层）
        let chain = format!("{err:#}");
        assert!(chain.contains("断连一"), "{chain}");
        assert!(chain.contains("断连二"), "{chain}");
    }

    #[tokio::test]
    async fn connect_failure_propagates_without_retry() {
        // 工厂失败（如命令不存在）→ 立即报错，不做无意义重试
        let c = McpClient::with_factory(
            params(),
            Arc::new(|_| Err(anyhow::anyhow!("启动 MCP server `mock` 失败"))),
        );
        let err = c.list_tools().await.unwrap_err();
        assert!(format!("{err:#}").contains("建立 MCP 传输失败"), "{err:#}");
    }

    #[tokio::test]
    async fn call_tool_maps_result() {
        let call_ok = serde_json::json!({"jsonrpc":"2.0","id":3,"result":{
            "content":[{"type":"text","text":"HELLO"}],"isError":false}});
        let mock = MockTransport::arc(vec![Ok(init_ok()), Ok(call_ok.clone())]);
        // call 是 first rpc → next_id=2，但 mock 不校验 id，直接按序回放
        let c = McpClient::with_factory(params(), factory_script(vec![mock]));
        let r = c.call_tool("upper", json!({"text":"hello"})).await.unwrap();
        assert!(!r.is_error);
        assert_eq!(r.text(), "HELLO");
    }

    #[test]
    fn display_name_prefers_url() {
        let mut p = params();
        p.url = Some("http://x/mcp".into());
        assert_eq!(McpClient::new(p).display_name(), "http://x/mcp");
        assert_eq!(McpClient::new(params()).display_name(), "mock");
    }

    // Unix 专属：真实 stdio 子进程端到端（Windows 由上面的 mock 覆盖同语义）
    #[cfg(unix)]
    mod unix_e2e {
        use super::*;

        fn fake_server(exits_after_call: bool) -> String {
            let init = r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-03-26","capabilities":{"tools":{}},"serverInfo":{"name":"fake","version":"0.1"}}}"#;
            let list = r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"upper","description":"转大写","inputSchema":{"type":"object","properties":{"text":{"type":"string"}}},"annotations":{"readOnlyHint":true}}]}}"#;
            let call = r#"{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"HELLO"}],"isError":false}}"#;
            let tail = if exits_after_call { "" } else { "sleep 60\n" };
            format!(
                "read _ && echo '{init}'\nread _\nread _ && echo '{list}'\nread _ && echo '{call}'\n{tail}"
            )
        }

        fn client_for(script: String) -> McpClient {
            let mut env = BTreeMap::new();
            env.insert("K".to_string(), "v".to_string());
            McpClient::new(McpConnectParams {
                command: "sh".into(),
                args: vec!["-c".into(), script],
                env,
                url: None,
                headers: Default::default(),
            })
        }

        #[tokio::test]
        async fn full_flow_list_and_call_via_real_process() {
            let c = client_for(fake_server(false));
            let tools = c.list_tools().await.unwrap();
            assert_eq!(tools.len(), 1);
            assert_eq!(tools[0].name, "upper");
            let res = c.call_tool("upper", json!({"text":"hello"})).await.unwrap();
            assert!(!res.is_error);
            assert_eq!(res.text(), "HELLO");
            c.disconnect().await;
        }

        #[tokio::test]
        async fn reconnects_after_real_process_exit() {
            // server 答完 tools/call 即退出 → 第二次调用触发重连（新进程重放同脚本）
            let c = client_for(fake_server(true));
            let _ = c.call_tool("upper", json!({"text":"a"})).await.unwrap();
            let res = c.call_tool("upper", json!({"text":"b"})).await.unwrap();
            assert_eq!(res.text(), "HELLO", "进程退出后应自动重连成功");
        }

        #[tokio::test]
        async fn spawn_failure_propagates() {
            let c = McpClient::new(McpConnectParams {
                command: "mcode-no-such-mcp-xyz".into(),
                ..Default::default()
            });
            let err = c.list_tools().await.unwrap_err();
            assert!(format!("{err:#}").contains("建立 MCP 传输失败"), "{err:#}");
        }
    }
}
