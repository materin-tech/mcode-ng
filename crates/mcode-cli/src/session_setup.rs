//! 会话装配：config/desktop → 可跑回合的完整运行时（工具表、MCP、hooks、
//! 会话落盘、子代理、权限）。
//!
//! 装配是唯一知道「全部件怎么拼」的地方：交互层（print/TUI）只拿
//! [`SessionRuntime`] 用，测试可注入假 backend 工厂走通全链路。

use anyhow::{Context, Result};
use mcode_core::agent::runner::{AgentDeps, MakeBackend};
use mcode_core::api::{build_backend, BackendConfig, Protocol};
use mcode_core::config::{Config, DesktopConfig};
use mcode_core::hooks::HookRunner;
use mcode_core::mcp::McpManager;
use mcode_core::plugins::PluginRegistry;
use mcode_core::sessions::SessionStore;
use mcode_core::skills::SkillRegistry;
use mcode_core::subagents::{install_task_tools, AgentRegistry};
use mcode_core::tools::{ToolContext, ToolRegistry};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::watch;

/// 一次会话的全部运行时部件。
pub struct SessionRuntime {
    /// 回合循环依赖（工具表已含内置 + MCP + skill + task 系；session 句柄
    /// 在 deps.session 里）。
    pub deps: AgentDeps,
    pub skills: SkillRegistry,
    pub plugins: PluginRegistry,
    /// 权限确认请求接收端（UI 轮询 → 弹窗 → respond 回传 Decision）。
    /// Arc<Mutex> 以便 SessionRuntime 整体进 Arc 后仍可轮询。
    pub confirm_rx: Arc<
        tokio::sync::Mutex<tokio::sync::mpsc::Receiver<mcode_core::agent::runner::ConfirmRequest>>,
    >,
    /// 后台子代理任务表（task/task_output/task_stop 共享；TUI 轮询状态变化）。
    pub task_table: mcode_core::subagents::TaskTable,
    /// 子代理注册表（内置 + 用户 + 项目；/agents 展示用）。
    pub agents: mcode_core::subagents::AgentRegistry,
    /// MCP 注册告警（单 server 连接失败等；/mcp /doctor 展示用）。
    pub mcp_report: Vec<String>,
}

/// 装配运行时。`backend_override` 供测试注入假 backend 工厂（None = 按配置
/// 构造真实 HTTP backend）；`model_override` 对齐 `--model`；
/// `fallback_override` 对齐 `--fallback-model`（None = 用 config 的链）。
pub async fn build_runtime(
    cwd: &Path,
    config: &Config,
    desktop: &DesktopConfig,
    model_override: Option<&str>,
    fallback_override: Option<Vec<String>>,
    backend_override: Option<MakeBackend>,
) -> Result<SessionRuntime> {
    let model = model_override
        .map(str::to_string)
        .or_else(|| config.active_model())
        .context("未配置模型：请先运行 mcode 完成引导（或用 --model 指定）")?;
    let provider = config
        .active_provider()
        .context("未配置 provider：请先运行 mcode 完成引导")?
        .clone();

    // backend 工厂：任何模型名都按 active provider 的端点/密钥重建
    // （fallback 切换与 --model 覆盖共用此入口）。
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

    // 工具上下文：真实 cwd + 取消信号。
    let (tool_ctx, cancel_tx) = real_tool_context(cwd);

    // 工具表：内置 + MCP + skill + task 系。
    let registry = ToolRegistry::with_builtins();

    // MCP：懒连接，注册工具清单；单个 server 挂不拖垮整体。
    let mcp = McpManager::from_configs(&desktop.mcp_servers);
    let mcp_report = mcp.register_into(&registry).await;

    // 会话：先建（HookRunner 需要 session_id），再装配其余部件。
    let store = SessionStore::for_project(cwd)?;
    let (session, meta) = store.start(cwd, Some(model.clone()))?;
    let session = Arc::new(session);

    let hooks = if desktop.hooks.is_empty() {
        None
    } else {
        Some(Arc::new(HookRunner::new(
            desktop.hooks.clone(),
            meta.session_id.clone(),
        )))
    };

    let perms = Arc::new(tokio::sync::Mutex::new(
        mcode_core::permission::PermissionManager::new(
            config.default_mode,
            desktop.permission_rules.clone(),
        ),
    ));

    // 确认通道：UI 持 rx 弹窗；容量 8 足够（工具并行段同时要确认的量级）。
    let (confirm_tx, confirm_rx) =
        tokio::sync::mpsc::channel::<mcode_core::agent::runner::ConfirmRequest>(8);

    // 技能与子代理：用户/项目/插件三层目录组合（项目覆盖用户）。
    let skills = load_skill_registry(cwd);
    let agents = load_agent_registry(cwd);
    let agents_for_rt = load_agent_registry(cwd);

    let deps = AgentDeps {
        model: model.clone(),
        fallback_models: fallback_override.unwrap_or_else(|| config.fallback_models.clone()),
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
    };

    deps.registry
        .register(Arc::new(mcode_core::skills::SkillTool::new(skills.clone())));

    // task 工具族（task/task_output/task_stop）。任务表留在工具内部，
    // TUI 后台轮询增量需要时再暴露。
    let task_table = install_task_tools(&deps, agents);

    Ok(SessionRuntime {
        deps,
        skills,
        plugins: {
            let roots = plugin_root_dirs(cwd);
            let refs: Vec<&Path> = roots.iter().map(|p| p.as_path()).collect();
            PluginRegistry::load_dirs(&refs)
        },
        confirm_rx: Arc::new(tokio::sync::Mutex::new(confirm_rx)),
        task_table,
        agents: agents_for_rt,
        mcp_report,
    })
}

/// 真实工具上下文（生产路径）。
fn real_tool_context(cwd: &Path) -> (ToolContext, watch::Sender<bool>) {
    let (tx, rx) = watch::channel(false);
    (
        ToolContext {
            cwd: cwd.to_path_buf(),
            cancel: rx,
            shared: Arc::default(),
            extra: Arc::new(()),
        },
        tx,
    )
}

/// 技能目录四层：bundled（ecc-core → pua-core）→ 用户 → 项目 → 插件
///（后加载覆盖同名，与桌面端同源）。
fn load_skill_registry(cwd: &Path) -> SkillRegistry {
    load_skill_registry_at(cwd, mcode_core::util::mcode_dir().as_deref())
}

/// [`load_skill_registry`] 的可注入形态（测试与自定义 HOME 用）。
fn load_skill_registry_at(cwd: &Path, home: Option<&Path>) -> SkillRegistry {
    let mut dirs = mcode_core::skills::bundled_dirs_at(home);
    if let Some(d) = home {
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

/// 子代理目录三层：bundled（ecc-core agents）→ 用户 → 项目 → 插件。
fn load_agent_registry(cwd: &Path) -> AgentRegistry {
    load_agent_registry_at(cwd, mcode_core::util::mcode_dir().as_deref())
}

/// [`load_agent_registry`] 的可注入形态（测试与自定义 HOME 用）。
fn load_agent_registry_at(cwd: &Path, home: Option<&Path>) -> AgentRegistry {
    let mut dirs = mcode_core::subagents::bundled_agent_dirs_at(home);
    if let Some(d) = home {
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

/// 插件根目录：`~/.mcode/plugins` → `.mcode/plugins`（项目覆盖用户）。
fn plugin_root_dirs(cwd: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(d) = mcode_core::util::mcode_dir() {
        dirs.push(d.join("plugins"));
    }
    dirs.push(cwd.join(".mcode").join("plugins"));
    dirs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uniq(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let d = std::env::temp_dir().join(format!(
            "mcode_cli_layers_{}_{}_{}",
            tag,
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write(path: &std::path::Path, content: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn skill_md(desc: &str) -> String {
        format!("---\ndescription: {desc}\n---\n\n按说明书行动。\n")
    }

    #[test]
    fn skill_registry_includes_bundled_tier_and_user_overrides() {
        let home = uniq("home");
        let cwd = uniq("cwd");
        // bundled 层：桌面端物化布局 ~/.mcode/assets/ecc-core/skills/<name>/SKILL.md
        write(
            &home.join("assets/ecc-core/skills/bundled-skill/SKILL.md"),
            &skill_md("来自内置资产"),
        );
        write(
            &home.join("assets/pua-core/pua-skill/SKILL.md"),
            &skill_md("来自 pua 资产"),
        );
        // 用户层 + 项目层
        write(
            &home.join("skills/user-skill/SKILL.md"),
            &skill_md("来自用户层"),
        );
        write(
            &cwd.join(".mcode/skills/proj-skill/SKILL.md"),
            &skill_md("来自项目层"),
        );
        // 同名覆盖：bundled 与 user 各有一份 dup → 用户层胜
        write(
            &home.join("assets/ecc-core/skills/dup/SKILL.md"),
            &skill_md("bundled 版"),
        );
        write(&home.join("skills/dup/SKILL.md"), &skill_md("用户版"));

        let reg = load_skill_registry_at(&cwd, Some(&home));
        let names = reg.names();
        for expected in ["bundled-skill", "pua-skill", "user-skill", "proj-skill"] {
            assert!(
                names.iter().any(|n| n == expected),
                "缺 {expected}: {names:?}"
            );
        }
        let dup = reg
            .all()
            .into_iter()
            .find(|s| s.name == "dup")
            .expect("dup 应存在且去重为一份");
        assert_eq!(dup.description, "用户版", "同名技能用户层应覆盖 bundled 层");
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn agent_registry_includes_bundled_tier() {
        let home = uniq("agent_home");
        let cwd = uniq("agent_cwd");
        write(
            &home.join("assets/ecc-core/agents/bundled-agent.md"),
            "---\nname: bundled-agent\ndescription: 内置子代理\n---\n\n负责调研。\n",
        );
        write(
            &cwd.join(".mcode/agents/proj-agent.md"),
            "---\nname: proj-agent\ndescription: 项目子代理\n---\n\n负责改动。\n",
        );
        let reg = load_agent_registry_at(&cwd, Some(&home));
        let names = reg.names();
        assert!(names.iter().any(|n| n == "bundled-agent"), "{names:?}");
        assert!(names.iter().any(|n| n == "proj-agent"), "{names:?}");
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&cwd);
    }
}
