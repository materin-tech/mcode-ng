// MCode Desktop — AI coding agent desktop app
// Copyright (c) 2026 Materin-tech. All rights reserved.
// SPDX-License-Identifier: MIT

//! 运行时桥接 — tokio 后台 runtime 跑回合，GPUI 主线程经 channel 收事件。
//! 装配对齐 mcode-cli 的 session_setup（MakeBackend 工厂 / MCP / 会话落盘 /
//! 子代理 / 三层技能目录 + bundled 资产层），单泵把 UiEvent 转发进 GPUI 实体。

use anyhow::{Context, Result};
use mcode_core::agent::AgentEvent;
use mcode_core::agent::runner::{AgentDeps, ConfirmRequest, ConfirmSender, MakeBackend};
use mcode_core::api::{BackendConfig, Protocol, build_backend};
use mcode_core::config::Config;
use mcode_core::hooks::HookRunner;
use mcode_core::mcp::McpManager;
use mcode_core::permission::PermissionManager;
use mcode_core::plugins::PluginRegistry;
use mcode_core::sessions::{SessionHandle, SessionStore};
use mcode_core::skills::SkillRegistry;
use mcode_core::subagents::{AgentRegistry, install_task_tools};
use mcode_core::tools::{ToolContext, ToolRegistry};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::sync::watch;

/// tokio 运行时（GPUI 主线程不跑 future，全部丢这里）
pub struct UiRuntime {
    rt: tokio::runtime::Runtime,
}

impl UiRuntime {
    pub fn new() -> Result<Self> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?;
        Ok(Self { rt })
    }

    pub fn spawn<F>(&self, fut: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        self.rt.spawn(fut);
    }

    pub fn block_on<F: std::future::Future>(&self, fut: F) -> F::Output {
        self.rt.block_on(fut)
    }
}

/// UI 统一事件流
pub enum UiEvent {
    /// agent 回合事件（TextDelta / ToolUseStart / Done …）
    Agent(AgentEvent),
    /// 权限确认请求（确认泵从 runner 转发）
    Confirm(ConfirmRequest),
    /// goal 外层循环裁判结果（true = 已达成）
    GoalCheck(bool),
}

/// harness 组装结果：deps 交给回合任务，事件流交给 GPUI 泵
pub struct Harness {
    pub deps: Arc<AgentDeps>,
    /// 会话句柄（落盘追加用；App 派生 session_id 走扩展管理的重命名/删除）
    pub session: Arc<SessionHandle>,
    pub skills: SkillRegistry,
    pub agents: AgentRegistry,
    /// MCODE.md 记忆文档（工作区定档，回合时进系统提示）
    pub project_doc: Option<String>,
    pub ui_tx: tokio::sync::mpsc::UnboundedSender<UiEvent>,
    /// UI 事件接收端 — App 经 take_rx 接管（take 后为 None，重复接管安全）
    rx_slot: Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<UiEvent>>>,
    /// MCP 注册警告（失败 server 逐条说明，不阻断）
    pub mcp_report: Vec<String>,
}

impl Harness {
    /// App 接管事件接收端，用于挂接 GPUI 事件泵
    pub fn take_rx(&self) -> Option<tokio::sync::mpsc::UnboundedReceiver<UiEvent>> {
        self.rx_slot.lock().ok()?.take()
    }
}

/// 构建 harness：新会话（`~/.mcode/projects/<cwd>` 桶）+ 空对话。
pub fn build_harness(
    cfg: &Config,
    cwd: &Path,
    store: &SessionStore,
    rt: &UiRuntime,
) -> Result<(
    Harness,
    Arc<tokio::sync::Mutex<mcode_core::message::Conversation>>,
)> {
    rt.block_on(build_harness_async(cfg, cwd, store, None, None))
}

/// 续跑会话：对话从 jsonl 活跃路径重建。
pub fn build_harness_resumed(
    cfg: &Config,
    cwd: &Path,
    store: &SessionStore,
    session_id: &str,
    rt: &UiRuntime,
) -> Result<(
    Harness,
    Arc<tokio::sync::Mutex<mcode_core::message::Conversation>>,
)> {
    rt.block_on(build_harness_async(cfg, cwd, store, Some(session_id), None))
}

/// 测试注入用：显式给后端工厂（绕过真实 HTTP），会话落临时根，其余组装完全一致。
#[cfg(test)]
pub fn build_harness_with_backend(
    cfg: &Config,
    cwd: &Path,
    store: &SessionStore,
    rt: &UiRuntime,
    backend: Arc<dyn mcode_core::api::ModelBackend>,
) -> Result<(
    Harness,
    Arc<tokio::sync::Mutex<mcode_core::message::Conversation>>,
)> {
    let make: MakeBackend = Arc::new(move |_| Ok(backend.clone()));
    rt.block_on(build_harness_async(cfg, cwd, store, None, Some(make)))
}

async fn build_harness_async(
    cfg: &Config,
    cwd: &Path,
    store: &SessionStore,
    resume_id: Option<&str>,
    backend_override: Option<MakeBackend>,
) -> Result<(
    Harness,
    Arc<tokio::sync::Mutex<mcode_core::message::Conversation>>,
)> {
    cfg.provider_ready()?;
    let model = cfg
        .active_model()
        .context("未配置模型：请先完成供应商引导")?;
    let provider = cfg
        .active_provider()
        .context("未配置 provider：请先完成供应商引导")?
        .clone();

    // backend 工厂：任何模型名都按 active provider 的端点/密钥重建
    //（fallback 切换与 /model 覆盖共用此入口）。
    let make_backend: MakeBackend = backend_override.unwrap_or_else(|| {
        Arc::new(move |m: &str| {
            let mut bc = BackendConfig::new(
                provider.protocol,
                provider.base_url.clone(),
                provider.resolve_api_key().unwrap_or_default(),
                m,
            );
            bc.cache_control = provider.protocol == Protocol::Anthropic;
            Ok(Arc::from(build_backend(&bc)) as Arc<dyn mcode_core::api::ModelBackend>)
        })
    });

    // 会话：先建（HookRunner 需要 session_id），对话随 resume 恢复或新建。
    let (session, conv) = match resume_id {
        Some(id) => {
            let (c, _) = store.load(id)?;
            let handle = store.resume(id)?;
            (Arc::new(handle), Arc::new(tokio::sync::Mutex::new(c)))
        }
        None => {
            let (handle, _meta) = store.start(cwd, Some(model.clone()))?;
            (
                Arc::new(handle),
                Arc::new(tokio::sync::Mutex::new(
                    mcode_core::message::Conversation::new(),
                )),
            )
        }
    };

    let (confirm_tx, mut confirm_rx): (ConfirmSender, _) = tokio::sync::mpsc::channel(16);
    let (ui_tx, ui_rx) = tokio::sync::mpsc::unbounded_channel::<UiEvent>();

    let desktop = mcode_core::config::DesktopConfig::load().unwrap_or_default();
    let perms = Arc::new(tokio::sync::Mutex::new(PermissionManager::new(
        cfg.default_mode,
        desktop.permission_rules.clone(),
    )));

    let hooks = if desktop.hooks.is_empty() {
        None
    } else {
        let sid = super::extension_manage::session_id_of(&session);
        Some(Arc::new(HookRunner::new(desktop.hooks.clone(), sid)))
    };

    // 工具上下文：真实 cwd + 取消信号。deps 自持 cancel 发送端（通道恒活，
    // esc 的 send 不会因无接收端而丢——老版 keepalive 接收端技巧不再需要）。
    let (cancel_tx, cancel_rx) = watch::channel(false);
    let tool_ctx = ToolContext {
        cwd: cwd.to_path_buf(),
        cancel: cancel_rx,
        shared: Arc::default(),
        extra: Arc::new(()),
    };

    let registry = ToolRegistry::with_builtins();
    let skills = load_skill_registry(cwd);
    let agents = load_agent_registry(cwd);

    let deps = Arc::new(AgentDeps {
        model: model.clone(),
        fallback_models: cfg.fallback_models.clone(),
        make_backend,
        registry: Arc::new(registry),
        perms,
        confirm: Some(confirm_tx),
        tool_ctx,
        cancel_tx,
        token_estimator: Box::new(mcode_core::token::raw_estimate_messages),
        hooks,
        session: Some(session.clone()),
        task_bus: Default::default(),
    });

    deps.registry
        .register(Arc::new(mcode_core::skills::SkillTool::new(skills.clone())));
    // task 工具族进 registry；任务表回执暂无 UI 面板，落变量保持所有权
    let _task_table = install_task_tools(&deps, agents.clone());

    // MCP：懒连接 + 工具注册（失败 server 记警告，不阻断）
    let mcp = McpManager::from_configs(&desktop.mcp_servers);
    let mcp_report = mcp.register_into(&deps.registry).await;

    // 确认泵：ConfirmRequest → UI 事件流
    let pump_tx = ui_tx.clone();
    tokio::spawn(async move {
        while let Some(req) = confirm_rx.recv().await {
            if pump_tx.send(UiEvent::Confirm(req)).is_err() {
                break;
            }
        }
    });

    let project_doc = super::system_prompt::load_project_doc(cwd);

    Ok((
        Harness {
            deps,
            session,
            skills,
            agents,
            project_doc,
            ui_tx,
            rx_slot: Mutex::new(Some(ui_rx)),
            mcp_report,
        },
        conv,
    ))
}

/// 技能目录四层：bundled（ecc-core → pua-core）→ 用户级 → 项目级 → 插件
///（后加载覆盖同名，对齐老 core load_all 顺序）。
pub(crate) fn load_skill_registry(cwd: &Path) -> SkillRegistry {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(d) = mcode_core::util::mcode_dir() {
        dirs.push(d.join("assets").join("ecc-core").join("skills"));
        dirs.push(d.join("assets").join("pua-core"));
        dirs.push(d.join("skills"));
    }
    dirs.push(cwd.join(".mcode").join("skills"));
    let plugin_roots = plugin_root_dirs(cwd);
    let plugin_refs: Vec<&Path> = plugin_roots.iter().map(|p| p.as_path()).collect();
    let plugins = PluginRegistry::load_dirs(&plugin_refs);
    dirs.extend(plugins.skill_dirs());
    let refs: Vec<&Path> = dirs.iter().map(|p| p.as_path()).collect();
    SkillRegistry::load_dirs(&refs)
}

/// 子代理目录三层：bundled → 用户级 → 项目级 → 插件。
pub(crate) fn load_agent_registry(cwd: &Path) -> AgentRegistry {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(d) = mcode_core::util::mcode_dir() {
        dirs.push(d.join("assets").join("ecc-core").join("agents"));
        dirs.push(d.join("agents"));
    }
    dirs.push(cwd.join(".mcode").join("agents"));
    let plugin_roots = plugin_root_dirs(cwd);
    let plugin_refs: Vec<&Path> = plugin_roots.iter().map(|p| p.as_path()).collect();
    let plugins = PluginRegistry::load_dirs(&plugin_refs);
    dirs.extend(plugins.agent_dirs());
    let refs: Vec<&Path> = dirs.iter().map(|p| p.as_path()).collect();
    AgentRegistry::load_dirs(&refs)
}

/// 插件根目录：`~/.mcode/plugins` → `.mcode/plugins`。
fn plugin_root_dirs(cwd: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(d) = mcode_core::util::mcode_dir() {
        dirs.push(d.join("plugins"));
    }
    dirs.push(cwd.join(".mcode").join("plugins"));
    dirs
}

/// 事件汇：AgentEvent → UI 事件流
pub fn event_sink(tx: tokio::sync::mpsc::UnboundedSender<UiEvent>) -> mcode_core::agent::EventSink {
    Arc::new(move |ev| {
        let _ = tx.send(UiEvent::Agent(ev));
    })
}
