//! mcp —— MCP 客户端：stdio + HTTP 传输、懒连接、失效即重连、工具桥接。
//!
//! 布局：
//! - [`protocol`]：JSON-RPC 2.0 信封 + MCP 载荷类型（宽容解析）；
//! - [`transport`]：stdio（子进程行协议）/ HTTP（streamable，含 SSE）；
//! - [`client`]：单 server 客户端（懒连接 + 断连重试一次的状态机）；
//! - 本文件：[`McpManager`]（多 server 编排）+ [`McpToolAdapter`]
//!   （MCP 工具 → 本地 [`crate::tools::Tool`] 系统，命名
//!   `mcp__{server}__{tool}`，`readOnlyHint` → `is_read_only`）。

pub mod client;
pub mod protocol;
pub mod transport;

pub use client::{McpClient, McpConnectParams};

use crate::config::McpServerConfig;
use crate::tools::{Tool, ToolContext, ToolDefinition, ToolRegistry, ToolResult};
use protocol::McpToolDef;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Manager：多 server 编排
// ---------------------------------------------------------------------------

/// 刷新发现的单条 MCP 工具。
#[derive(Debug, Clone)]
pub struct McpToolInfo {
    pub server: String,
    pub def: McpToolDef,
}

/// 已启用 server 的客户端集合（懒连接：构造不触发任何进程/网络）。
#[derive(Clone, Default)]
pub struct McpManager {
    clients: Vec<(String, Arc<McpClient>)>,
}

impl McpManager {
    /// 从配置构造；`enabled: false` 的 server 不纳入。
    pub fn from_configs(cfgs: &[McpServerConfig]) -> Self {
        let clients = cfgs
            .iter()
            .filter(|c| c.enabled)
            .map(|c| (c.name.clone(), Arc::new(McpClient::new(params_from(c)))))
            .collect();
        Self { clients }
    }

    pub fn client(&self, name: &str) -> Option<Arc<McpClient>> {
        self.clients
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, c)| c.clone())
    }

    pub fn server_names(&self) -> Vec<String> {
        self.clients.iter().map(|(n, _)| n.clone()).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.clients.is_empty()
    }

    /// 刷新工具目录：逐 server list_tools；失败的 server 记警告不拖垮整体
    /// （单个 MCP server 挂了只影响自己，其余照常可用）。
    pub async fn refresh_tools(&self) -> (Vec<McpToolInfo>, Vec<String>) {
        let mut tools = Vec::new();
        let mut warnings = Vec::new();
        for (name, client) in &self.clients {
            match client.list_tools().await {
                Ok(defs) => {
                    for def in defs {
                        tools.push(McpToolInfo {
                            server: name.clone(),
                            def,
                        });
                    }
                }
                Err(e) => warnings.push(format!("MCP server {name}: {e}")),
            }
        }
        (tools, warnings)
    }

    /// 刷新并注册进工具注册表（先清旧 `mcp__` 前缀条目再注册，
    /// 幂等可重复调用）。返回失败 server 的警告。
    pub async fn register_into(&self, registry: &ToolRegistry) -> Vec<String> {
        let (tools, warnings) = self.refresh_tools().await;
        registry.unregister_by_prefix("mcp__");
        for info in &tools {
            let Some(client) = self.client(&info.server) else {
                continue;
            };
            registry.register(Arc::new(McpToolAdapter {
                client,
                server: info.server.clone(),
                def: info.def.clone(),
            }));
        }
        warnings
    }
}

/// [`McpServerConfig`] → 连接参数（字段直映射；url 空串视同 stdio）。
fn params_from(c: &McpServerConfig) -> McpConnectParams {
    McpConnectParams {
        command: c.command.clone(),
        args: c.args.clone(),
        env: c.env.clone(),
        url: c.url.clone().filter(|u| !u.is_empty()),
        headers: c.headers.clone(),
    }
}

// ---------------------------------------------------------------------------
// 工具适配器：MCP 工具 → 本地 Tool 系统
// ---------------------------------------------------------------------------

/// 把一条 MCP 工具桥接为本地 [`Tool`]。
pub struct McpToolAdapter {
    client: Arc<McpClient>,
    server: String,
    def: McpToolDef,
}

impl McpToolAdapter {
    /// 本地注册名（对齐 config 注释里的 `mcp__server__tool` 约定）。
    pub fn mcp_name(&self) -> String {
        format!("mcp__{}__{}", self.server, self.def.name)
    }
}

#[async_trait::async_trait]
impl Tool for McpToolAdapter {
    fn def(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.mcp_name(),
            description: self.def.description.clone().unwrap_or_else(|| {
                format!("MCP 工具 {}（server: {}）", self.def.name, self.server)
            }),
            // schema 缺失/非对象 → 空对象 schema（无参数工具），防模型拿到坏契约
            parameters: if self.def.input_schema.is_object() {
                self.def.input_schema.clone()
            } else {
                serde_json::json!({"type": "object", "properties": {}})
            },
            // readOnlyHint 是 hint 不是保证；误标的兜底是权限规则
            is_read_only: self
                .def
                .annotations
                .as_ref()
                .and_then(|a| a.read_only_hint)
                .unwrap_or(false),
        }
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        _ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        // MCP 层错误（断连重试后仍失败）→ 错误结果回喂模型，不 panic 不冒泡
        let r = match self.client.call_tool(&self.def.name, input).await {
            Ok(r) => r,
            Err(e) => return Ok(ToolResult::err(format!("MCP 调用失败: {e}"))),
        };
        let mut res = if r.is_error {
            let text = r.text();
            ToolResult::err(if text.is_empty() {
                "MCP 工具报告执行失败（无详情）".into()
            } else {
                text
            })
        } else {
            let text = r.text();
            ToolResult::ok(if text.is_empty() && r.images().is_empty() {
                "（工具返回空结果）".into()
            } else {
                text
            })
        };
        for (media_type, data) in r.images() {
            res = res.with_image(&media_type, data);
        }
        Ok(res)
    }
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::McpServerConfig;

    fn server_cfg(name: &str, command: &str, enabled: bool) -> McpServerConfig {
        McpServerConfig {
            name: name.into(),
            command: command.into(),
            args: vec![],
            env: Default::default(),
            url: None,
            headers: Default::default(),
            enabled,
        }
    }

    #[test]
    fn manager_filters_disabled_and_maps_params() {
        let m = McpManager::from_configs(&[
            server_cfg("a", "cmd-a", true),
            server_cfg("b", "cmd-b", false),
        ]);
        assert_eq!(m.server_names(), vec!["a".to_string()]);
        assert!(m.client("a").is_some());
        assert!(m.client("b").is_none());
        assert!(!m.is_empty());
    }

    #[tokio::test]
    async fn dead_server_warns_without_blocking_others() {
        let m = McpManager::from_configs(&[
            server_cfg("dead", "mcode-no-such-mcp-cmd-xyz", true),
            // good server 用 HTTP url 指向不存在的端口：连接失败但不 spawn 进程
            server_cfg("dead2", "whatever", true),
        ]);
        let (tools, warnings) = m.refresh_tools().await;
        assert!(tools.is_empty());
        assert_eq!(warnings.len(), 2);
        assert!(warnings[0].contains("MCP server dead"));
    }

    #[tokio::test]
    async fn url_empty_string_falls_back_to_stdio_params() {
        let mut cfg = server_cfg("u", "cmd", true);
        cfg.url = Some("".into());
        let p = params_from(&cfg);
        assert!(p.url.is_none(), "空 url 视同 stdio");
        let mut cfg2 = server_cfg("u", "cmd", true);
        cfg2.url = Some("http://x/mcp".into());
        assert_eq!(params_from(&cfg2).url.as_deref(), Some("http://x/mcp"));
    }

    #[test]
    fn adapter_def_maps_prefix_and_read_only_hint() {
        let def: McpToolDef = serde_json::from_value(serde_json::json!({
            "name": "upper",
            "description": "转大写",
            "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}},
            "annotations": {"readOnlyHint": true}
        }))
        .unwrap();
        let adapter = McpToolAdapter {
            client: Arc::new(McpClient::new(McpConnectParams::default())),
            server: "fake".into(),
            def,
        };
        let d = adapter.def();
        assert_eq!(d.name, "mcp__fake__upper");
        assert_eq!(d.description, "转大写");
        assert!(d.is_read_only, "readOnlyHint=true → is_read_only");

        // 无注解 → 非只读；schema 非对象 → 空对象兜底
        let def2: McpToolDef =
            serde_json::from_value(serde_json::json!({"name": "w", "inputSchema": "bogus"}))
                .unwrap();
        let a2 = McpToolAdapter {
            client: Arc::new(McpClient::new(McpConnectParams::default())),
            server: "fake".into(),
            def: def2,
        };
        let d2 = a2.def();
        assert!(!d2.is_read_only);
        assert_eq!(d2.parameters["type"], "object");
    }

    #[tokio::test]
    async fn adapter_execute_maps_call_failure_to_error_result() {
        let def: McpToolDef = serde_json::from_value(serde_json::json!({"name": "t"})).unwrap();
        // 工厂失败 → call_tool 失败 → 错误结果（不 panic 不冒泡）
        let client = McpClient::with_factory(
            McpConnectParams::default(),
            Arc::new(|_| Err(anyhow::anyhow!("连接失败"))),
        );
        let adapter = McpToolAdapter {
            client: Arc::new(client),
            server: "fake".into(),
            def,
        };
        let (_ctx, _tx) = ToolContext::for_tests(std::env::temp_dir());
        let res = adapter.execute(serde_json::json!({}), &_ctx).await.unwrap();
        assert!(!res.success);
        assert!(res.output.contains("MCP 调用失败"));
    }

    // Unix 专属：真实 stdio server 端到端（list → 注册 → 经 registry 调用）
    #[cfg(unix)]
    mod unix_e2e {
        use super::*;

        /// 通用假 server：按请求类型分发、以服务端计数器回显请求 id
        /// （罐头 id 会和多次刷新的递增 id 错位）；通知不响应；12 个请求
        /// 后自然退出。单引号包裹 JSON，避免转义地狱。
        fn fake_server() -> String {
            let init = r#"{"jsonrpc":"2.0","id":0,"result":{"protocolVersion":"2025-03-26","capabilities":{"tools":{}},"serverInfo":{"name":"fake","version":"0.1"}}}"#;
            let list = r#"{"jsonrpc":"2.0","id":0,"result":{"tools":[{"name":"upper","description":"转大写","inputSchema":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]},"annotations":{"readOnlyHint":true}}]}}"#;
            let call =
                r#"{"jsonrpc":"2.0","id":0,"result":{"content":[{"type":"text","text":"HELLO"}]}}"#;
            let mut s = String::from("n=0\n");
            s.push_str("while IFS= read -r line; do\n");
            s.push_str("  case \"$line\" in\n");
            s.push_str("    *'\"id\":null'*) ;;\n"); // 通知：不响应
            s.push_str("    *)\n      n=$((n+1))\n");
            s.push_str(&format!("      case \"$line\" in\n        *initialize*) printf '%s\\n' '{init}' | sed 's/\"id\":0/\"id\":'\"$n\"'/' ;;\n"));
            s.push_str(&format!("        *tools/list*) printf '%s\\n' '{list}' | sed 's/\"id\":0/\"id\":'\"$n\"'/' ;;\n"));
            s.push_str(&format!("        *tools/call*) printf '%s\\n' '{call}' | sed 's/\"id\":0/\"id\":'\"$n\"'/' ;;\n"));
            s.push_str("      esac\n      ;;\n    esac\n");
            s.push_str("  [ \"$n\" -ge 12 ] && break\ndone\n");
            s
        }

        #[tokio::test]
        async fn register_into_registry_and_execute() {
            let mut cfg = server_cfg("fake", "sh", true);
            cfg.args = vec!["-c".into(), fake_server()];
            let m = McpManager::from_configs(&[cfg]);

            let registry = ToolRegistry::new_empty();
            let warnings = m.register_into(&registry).await;
            assert!(warnings.is_empty(), "{warnings:?}");

            // 注册名 + 只读标记
            let def = registry.get("mcp__fake__upper").expect("工具已注册").def();
            assert!(def.is_read_only);

            // 经 registry 执行（与内置工具同一条路径）
            let tool = registry.get("mcp__fake__upper").unwrap();
            let (_ctx, _tx) = ToolContext::for_tests(std::env::temp_dir());
            let res = tool
                .execute(serde_json::json!({"text": "hello"}), &_ctx)
                .await
                .unwrap();
            assert!(res.success);
            assert_eq!(res.output, "HELLO");
        }

        #[tokio::test]
        async fn re_register_replaces_stale_entries() {
            let mut cfg = server_cfg("fake", "sh", true);
            cfg.args = vec!["-c".into(), fake_server()];
            let m = McpManager::from_configs(&[cfg]);
            let registry = ToolRegistry::new_empty();

            m.register_into(&registry).await;
            m.register_into(&registry).await; // 幂等：不产生重名条目
            let mcp_tools: Vec<_> = registry
                .names()
                .into_iter()
                .filter(|n| n.starts_with("mcp__"))
                .collect();
            assert_eq!(mcp_tools, vec!["mcp__fake__upper".to_string()]);
        }
    }
}
