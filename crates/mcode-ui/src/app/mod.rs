// MCode Desktop — AI coding agent desktop app
// Copyright (c) 2026 Materin-tech. All rights reserved.
// SPDX-License-Identifier: MIT

//! 主应用实体：真实流式回合（run_agent_turn）、权限弹窗真裁决（Confirm 通道）、
//! /goal 外层循环（check_goal_complete 裁判）、会话持久化（SessionStore）、指令系统。
//! 渲染分发在 views.rs（四视图）与 chat_view.rs（chat 布局），指令与会话操作在 session_ops.rs。

pub mod assets;
pub mod commands;
pub mod completion;
pub mod extension_manage;
pub mod goal;
pub mod manage;
pub mod msg;
pub mod runtime;
pub mod session_ops;
pub mod system_prompt;

use gpui::{
    App as GpuiApp, Context, Entity, FocusHandle, Focusable, Render, Window, actions, prelude::*,
};
use mcode_core::agent::AgentEvent;
use mcode_core::agent::runner::{RunOpts, run_agent_turn};
use mcode_core::config::Config;
use mcode_core::message::{ContentBlock, Conversation, Message, Role};
use mcode_core::permission::{Decision, Mode};
use mcode_core::sessions::{SessionStore, SessionSummary};
use mcode_core::skills::SkillDefinition;
use mcode_core::subagents::AgentDefinition;
use mcode_core::transcript::{NoticeKind, TranscriptState};
use mcode_core::util::file_refs::expand_file_refs;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::app::commands::{filter_commands, slash_skill_candidates};
use crate::app::completion::{Candidate, CompletionState, InputHistory, Trigger};
use crate::app::goal::{MAX_GOAL_ROUNDS, build_goal_prompt, check_goal_complete};
use crate::app::msg::PendingPermission;
use crate::app::runtime::{Harness, UiEvent, UiRuntime, event_sink};
use crate::app::session_ops::current_effort;
use crate::app::system_prompt::{SystemPromptOpts, build_pua_directive, build_system_prompt};
use crate::views::text_input::{InputEvent, TextInput};

actions!(
    app,
    [
        CycleMode,
        PaletteUp,
        PaletteDown,
        PaletteComplete,
        ClosePalette,
        NewChat,
        OpenSettings,
        OpenExtensions,
        OpenSessions,
        BackToChat,
        ToggleDetail
    ]
);

// ---------- 视图与消息视图模型 ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Chat,
    Settings,
    Extensions,
    Sessions,
    Onboarding,
    /// 启动工作区选择器（IDEA 式首屏；顶栏 cwd 徽章点击可重新进入）
    WorkspacePicker,
}

/// 本地路径安装对象
#[derive(Clone, Copy, PartialEq)]
pub enum InstallKind {
    Skill,
    Agent,
}

/// 删除二次确认的目标
#[derive(Clone)]
pub enum RemoveAction {
    Provider(String),
    Skill(String),
    Agent(String),
    Mcp(String),
}

/// 设置/扩展页表单对话框（字段为 Entity<TextInput>；渲染见 views/dialogs.rs）
pub enum Dialog {
    /// 供应商新增/编辑（editing = Some(原名) 为编辑模式）
    ProviderForm {
        editing: Option<String>,
        name: Entity<TextInput>,
        url: Entity<TextInput>,
        key: Entity<TextInput>,
        models: Entity<TextInput>,
        mm: Entity<TextInput>,
        protocol: mcode_core::config::Protocol,
    },
    /// agent 创建（正文 = system prompt）
    AgentCreate {
        name: Entity<TextInput>,
        desc: Entity<TextInput>,
        prompt: Entity<TextInput>,
    },
    /// 技能创建
    SkillCreate {
        name: Entity<TextInput>,
        desc: Entity<TextInput>,
    },
    /// 本地路径安装（skill 目录 / agent .md 文件）
    Install {
        kind: InstallKind,
        path: Entity<TextInput>,
    },
    /// MCP server 新增
    McpForm {
        name: Entity<TextInput>,
        command: Entity<TextInput>,
    },
    /// 删除二次确认
    ConfirmRemove { action: RemoveAction },
}

/// App — UI 侧状态 + 内核接线。回合一启动就持 conv 锁推进，UI 线程只 try_lock 只读。
pub struct App {
    focus_handle: FocusHandle,
    /// Composer 输入框
    pub input: Entity<TextInput>,
    /// Onboarding 表单输入（名称 / baseURL / apiKeyEnv / models 逗号分隔 / API 协议）
    pub ob_name: Entity<TextInput>,
    pub ob_url: Entity<TextInput>,
    pub ob_key: Entity<TextInput>,
    pub ob_models: Entity<TextInput>,
    pub ob_protocol: mcode_core::config::Protocol,
    pub(crate) rt: Arc<UiRuntime>,
    pub harness: Option<Arc<Harness>>,
    pub cfg: Config,
    /// run_agent_turn 消费的历史（回合任务持锁期间 UI 用 try_lock 只读）
    pub(crate) hist: Arc<Mutex<Conversation>>,
    /// 对话呈现状态（与 CLI 共用的唯一事实源；渲染走 render_block）
    pub transcript: TranscriptState,
    /// 运行中排队插话（finish_run 出队时再分类解释）
    pub(crate) queue: Vec<String>,
    pub(crate) running: bool,
    /// spinner/blink 动画帧计数（运行中每 120ms 前进一格）
    pub(crate) frame_tick: u64,
    pub(crate) completion: CompletionState,
    pub(crate) candidates: Vec<Candidate>,
    pub(crate) history: InputHistory,
    /// 当前权限模式（缓存；真值在 harness.deps.perms，UI try_lock 读写）
    pub mode: Mode,
    pub pending_permission: Option<PendingPermission>,
    /// 会话列表（resume 视图数据源；当前会话在 harness.session）
    pub sessions: Vec<SessionSummary>,
    /// 当前工作区的会话存储（列表 / resume / 重命名 / 删除）
    pub store: Option<SessionStore>,
    pub file_tree: Vec<String>,
    /// 正文中的 @path 引用（extract_refs 结果）：文件树按此高亮“@即选中”
    pub(crate) mentioned: Vec<String>,
    pub cwd: PathBuf,
    pub goal: Option<String>,
    /// goal 外层循环已跑的轮数（/goal clear 时清零）
    pub goal_rounds: usize,
    pub tokens_total: u64,
    /// 上次估算的上下文 token（回合运行中 conv 锁被回合任务持有，渲染用缓存）
    pub(crate) ctx_tokens: usize,
    pub view: View,
    /// 消息区滚动句柄（跟随模式每帧钉底）
    pub(crate) chat_scroll: gpui::ScrollHandle,
    /// 消息区滚动跟随：上翻即暂停，贴底（连续下滚 offset 不再变化）或点按钮恢复
    pub(crate) chat_follow: bool,
    /// 上一次下滚事件时的 offset.y（贴底探测：连续两次下滚位置不变 = 已贴底）
    pub(crate) chat_last_down_y: Option<gpui::Pixels>,
    // -- 工作区文件浏览器 / 布局 --
    /// 左侧功能区折叠（40px 图标条）
    pub(crate) sidebar_collapsed: bool,
    /// 右侧文件浏览器可见
    pub(crate) explorer_visible: bool,
    /// 浏览器/@ 补全是否显示点开头条目（默认 false）
    pub(crate) show_hidden: bool,
    /// 工作区选择器的路径输入框
    pub ws_input: Entity<TextInput>,
    /// 文件树展开的目录（默认折叠到深度 2：cwd 本层与次层目录预展开）
    pub(crate) expanded_dirs: std::collections::HashSet<PathBuf>,
    /// 浏览器内选中项（复制/剪切/删除/引用的目标）
    pub(crate) fs_selected: Option<std::path::PathBuf>,
    /// 文件剪贴板 (path, is_cut)
    pub(crate) fs_clipboard: Option<(std::path::PathBuf, bool)>,
    /// 删除二次确认（第一次点击武装，同一路径再点一次执行）
    pub(crate) fs_delete_armed: Option<std::path::PathBuf>,
    /// 文件浏览器键上下文的焦点句柄（cmd-c/x/v 拦截）
    pub explorer_focus: gpui::FocusHandle,
    /// 右键上下文菜单（渲染为绝对定位浮层）
    pub(crate) context_menu: Option<crate::views::context_menu::CtxMenu>,
    /// 行内重命名弹窗（预填当前名 + 确认/取消）
    pub(crate) rename_state: Option<crate::views::context_menu::RenameState>,
    /// 会话右键菜单（重命名/删除；与文件右键菜单互斥）
    pub(crate) session_menu: Option<crate::views::context_menu::SessMenu>,
    /// 会话重命名弹窗（预填当前标题 + 确认/取消）
    pub(crate) session_rename: Option<crate::views::context_menu::SessRename>,
    /// 管理/表单对话框（设置与扩展页增删改；None = 关闭）
    pub dialog: Option<Dialog>,
    /// 扩展页缓存：技能与 agent 清单（进入视图或 CRUD 后刷新）
    pub ext_skills: Vec<SkillDefinition>,
    pub ext_agents: Vec<AgentDefinition>,
}
/// 回合任务获取 conv 锁的等待上限（测试用 1s 加速锁超时用例）
#[cfg(test)]
const HIST_LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);
#[cfg(not(test))]
const HIST_LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

impl App {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self::init(cx, false)
    }

    /// 测试构造：不自动 build harness（测试用 build_harness_with_backend 注入 MockBackend），
    /// 直接落在 Chat 视图。test_mode 下连真实 ~/.mcode/config.json 的 harness 都不建。
    #[cfg(test)]
    pub(crate) fn new_for_test(cx: &mut Context<Self>) -> Self {
        Self::init(cx, true)
    }

    fn init(cx: &mut Context<Self>, test_mode: bool) -> Self {
        // 解析失败 → 默认骨架（对齐 Config::load_from 回退语义；错误细节看 stderr）
        let cfg = Config::load().unwrap_or_default();
        let cwd = std::env::current_dir().unwrap_or_default();
        let input = Self::make_input(cx, "向 MCode 发送消息…  （/ 指令 · @ 文件）");
        let ob_name = Self::make_input(cx, "如 zhipu / glm / openai");
        let ob_url = Self::make_input(cx, "https://open.bigmodel.cn/api/paas/v4");
        let ob_key = Self::make_input(cx, "API key 环境变量名，如 ZHIPU_API_KEY");
        let ob_models = Self::make_input(cx, "逗号分隔，如 glm-4.7-multimodal,glm-4.7-air");
        let ws_input = Self::make_input(cx, "/绝对/路径（支持 ~ 展开）");
        let needs_onboarding = cfg.needs_onboarding();
        // 内置资产（ecc-core agents/skills + pua-core）一次性物化到 ~/.mcode/assets，
        // skills/skill 工具/task 内置 agent/PUA 指令的数据源
        let materialized = crate::app::assets::materialize_bundled_assets();
        if materialized > 0 {
            eprintln!("已物化内置资产 {materialized} 组");
        }
        let mut me = Self {
            focus_handle: cx.focus_handle(),
            input,
            ob_name,
            ob_url,
            ob_key,
            ob_models,
            ob_protocol: mcode_core::config::Protocol::default(),
            ws_input,
            rt: Arc::new(UiRuntime::new().expect("tokio runtime 创建失败")),
            harness: None,
            cfg: cfg.clone(),
            hist: Arc::new(Mutex::new(Conversation::new())),
            transcript: TranscriptState::new(
                cfg.active_model()
                    .unwrap_or_else(|| "mcode".to_string())
                    .as_str(),
            ),
            queue: vec![],
            running: false,
            frame_tick: 0,
            completion: CompletionState::default(),
            candidates: vec![],
            history: InputHistory::default(),
            mode: cfg.default_mode,
            pending_permission: None,
            sessions: vec![],
            store: None,
            file_tree: crate::views::file_explorer::scan_file_tree(&cwd, false),
            mentioned: vec![],
            cwd,
            goal: None,
            goal_rounds: 0,
            tokens_total: 0,
            ctx_tokens: 0,
            view: if test_mode {
                View::Chat
            } else if needs_onboarding {
                View::Onboarding
            } else {
                // 启动首屏：工作区选择器（IDEA 等同；选定后才落 cwd 构建 harness）
                View::WorkspacePicker
            },
            chat_scroll: gpui::ScrollHandle::new(),
            chat_follow: true,
            chat_last_down_y: None,
            sidebar_collapsed: false,
            explorer_visible: true,
            show_hidden: false,
            expanded_dirs: Default::default(),
            fs_selected: None,
            fs_clipboard: None,
            fs_delete_armed: None,
            explorer_focus: cx.focus_handle(),
            context_menu: None,
            rename_state: None,
            session_menu: None,
            session_rename: None,
            dialog: None,
            ext_skills: vec![],
            ext_agents: vec![],
        };
        // 输入框事件 → 发送 / 补全状态机同步（对齐 prototype 的 useCompletion 链路）
        cx.subscribe(&me.input, |this, _, event, cx| match event {
            InputEvent::Submit(text) => this.send(text.clone(), cx),
            InputEvent::Changed(text) => this.sync_completion(text),
        })
        .detach();
        // spinner/blink 动画节拍：运行中每 120ms 前进一帧并通知重绘（空闲不空转）
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(120))
                    .await;
                if this
                    .update(cx, |app, cx| {
                        if app.running {
                            app.frame_tick = app.frame_tick.wrapping_add(1);
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        // 非 onboarding 启动：构建 harness（失败 → Notice 引导去设置，不 panic）；
        // 测试模式下由测试显式注入 harness
        if !needs_onboarding && !test_mode {
            me.rebuild_harness(cx);
        }
        me
    }

    fn make_input(cx: &mut Context<Self>, placeholder: &str) -> Entity<TextInput> {
        cx.new(|cx| TextInput {
            focus_handle: cx.focus_handle(),
            content: "".into(),
            placeholder: placeholder.to_string().into(),
            selected_range: 0..0,
            selection_reversed: false,
            marked_range: None,
            last_layout: None,
            last_bounds: None,
            is_selecting: false,
        })
    }

    // ---------- harness 生命周期 ----------

    /// 构建 harness（新会话随装配创建）并挂事件泵；失败不 panic（Notice 引导去设置）
    pub fn rebuild_harness(&mut self, cx: &mut Context<Self>) {
        self.harness = None;
        let store = match SessionStore::for_project(&self.cwd) {
            Ok(s) => s,
            Err(e) => {
                self.push_notice(
                    format!("会话目录初始化失败：{e}").to_string(),
                    NoticeKind::Info,
                );
                cx.notify();
                return;
            }
        };
        match crate::app::runtime::build_harness(&self.cfg, &self.cwd, &store, &self.rt) {
            Ok((h, conv)) => {
                self.ext_skills = h.skills.all().into_iter().cloned().collect();
                self.ext_agents = h.agents.all().into_iter().cloned().collect();
                let harness = Arc::new(h);
                for w in &harness.mcp_report {
                    self.push_notice(format!("MCP：{w}"), NoticeKind::Warning);
                }
                self.harness = Some(harness);
                self.hist = conv;
                self.store = Some(store);
                self.attach_pump(cx);
            }
            Err(e) => {
                self.push_notice(
                    format!("初始化失败：{e}（cmd-, 打开设置）"),
                    NoticeKind::Info,
                );
            }
        }
        self.refresh_sessions();
    }

    /// 挂接 GPUI 事件泵：tokio channel → 主线程实体更新（WeakEntity::update，实体释放即退出）
    fn attach_pump(&mut self, cx: &mut Context<Self>) {
        let Some(rx) = self.harness.as_ref().and_then(|h| h.take_rx()) else {
            return;
        };
        let mut rx = rx;
        cx.spawn(async move |this, cx| {
            while let Some(ev) = rx.recv().await {
                if this.update(cx, |app, cx| app.on_ui_event(ev, cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    // ---------- 发送 / 回合启动 ----------

    /// Composer 提交入口
    pub(crate) fn send(&mut self, text: String, cx: &mut Context<Self>) {
        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }
        self.completion.close();
        self.history.push(text.clone());
        // 首次发送后 placeholder 切为队列编辑提示（CLI 同款语义）
        if !self.transcript.submitted_once {
            self.set_placeholder("Press up to edit queued messages", cx);
        }
        // running 时入队（finish_run 出队时再分类解释）
        if self.running {
            self.queue.push(text);
            cx.notify();
            return;
        }
        self.dispatch_input(text, cx);
    }

    /// 更新 Composer placeholder（CLI 语义：首发后切队列编辑提示，新会话复位）
    pub(crate) fn set_placeholder(&mut self, text: &str, cx: &mut Context<Self>) {
        self.input
            .update(cx, |ti, _| ti.placeholder = text.to_owned().into());
    }

    /// transcript 复位（/clear、new_chat、resume 共用）：新状态 + placeholder 复位
    pub(crate) fn reset_transcript(&mut self, cx: &mut Context<Self>) {
        let model = self
            .harness
            .as_ref()
            .map(|h| h.deps.current_model())
            .unwrap_or_default();
        self.transcript = TranscriptState::new(&model);
        self.chat_follow = true;
        self.chat_last_down_y = None;
        self.set_placeholder("Try: 帮我看看这个仓库的结构", cx);
    }

    /// 输入分类：`!` → 本地 bash；`/` → 指令；其余 → 模型回合（CLI classify_input 同构）
    fn dispatch_input(&mut self, text: String, cx: &mut Context<Self>) {
        if let Some(cmd) = text.strip_prefix('!') {
            self.run_bash_local(cmd.trim().to_string(), cx);
            return;
        }
        // 指令本地解释，不进模型回合
        if text.starts_with('/') && self.handle_command(&text, cx) {
            cx.notify();
            return;
        }
        self.launch_turn(text, cx);
    }

    /// `!` 本地 bash：push_user("!cmd") + shell 三事件（ToolUseStart/InputDelta/
    /// Result，scope="shell"），与 CLI spawn_shell 同构。不 begin_turn——bash
    /// 旁路不显示 spinner（CLI 同款）。
    fn run_bash_local(&mut self, cmd: String, cx: &mut Context<Self>) {
        if cmd.is_empty() {
            return;
        }
        let Some(h) = self.harness.clone() else {
            self.push_notice("未初始化完成，无法执行本地命令", NoticeKind::Error);
            cx.notify();
            return;
        };
        self.transcript.push_user(&format!("!{cmd}"));
        let id = mcode_core::util::now_ms().to_string();
        let _ = h.ui_tx.send(UiEvent::Agent(AgentEvent::ToolUseStart {
            id: id.clone(),
            name: "bash".into(),
            scope: Some(mcode_core::transcript::SHELL_SCOPE.into()),
        }));
        // 参数直填（无流式过程）：Bash(cmd) 头行立刻可见
        let _ = h.ui_tx.send(UiEvent::Agent(AgentEvent::ToolInputDelta {
            id,
            partial_json: serde_json::json!({ "command": cmd }).to_string(),
            scope: Some(mcode_core::transcript::SHELL_SCOPE.into()),
        }));
        let ui_tx = h.ui_tx.clone();
        let cwd = Arc::new(self.cwd.clone());
        self.rt.spawn(async move {
            let mut command = if cfg!(windows) {
                let mut c = tokio::process::Command::new("cmd");
                c.arg("/C").arg(&cmd);
                c
            } else {
                let mut c = tokio::process::Command::new("sh");
                c.arg("-c").arg(&cmd);
                c
            };
            command.current_dir(cwd.as_path());
            let _ = mcode_core::util::hide_console_window(&mut command);
            let (success, output) = match command.output().await {
                Ok(o) => {
                    let mut text = String::from_utf8_lossy(&o.stdout).to_string();
                    let err = String::from_utf8_lossy(&o.stderr);
                    if !err.trim().is_empty() {
                        if !text.is_empty() && !text.ends_with('\n') {
                            text.push('\n');
                        }
                        text.push_str(&err);
                    }
                    (o.status.success(), text)
                }
                Err(e) => (false, format!("启动失败：{e}")),
            };
            let _ = ui_tx.send(UiEvent::Agent(AgentEvent::ToolResult {
                name: "bash".into(),
                success,
                output,
                display: None,
                scope: Some(mcode_core::transcript::SHELL_SCOPE.into()),
            }));
        });
        cx.notify();
    }

    /// 启动一次模型回合：@ 引用展开 → transcript 占位 → spawn run_agent_turn
    ///（会话落盘在 harness 装配时已就绪，用户/助手消息由回合任务与 runner 落盘）
    fn launch_turn(&mut self, text: String, cx: &mut Context<Self>) {
        let mut turn_input = text.clone();
        let mut attachments = vec![];

        // -- @ 文件引用展开（multimodal 门控在内核 expand_file_refs 内） --
        if text.contains('@') {
            let multimodal = self
                .cfg
                .active_provider()
                .zip(self.cfg.active_model())
                .map(|(p, m)| p.is_multimodal(&m))
                .unwrap_or(false);
            let r = expand_file_refs(&text, &self.cwd, multimodal);
            if !r.injected.is_empty() {
                self.push_notice(
                    format!("已注入 {} 个 @ 引用（文件内容/目录列表）", r.injected.len()),
                    NoticeKind::Info,
                );
            }
            if !r.missing.is_empty() {
                self.push_notice(
                    format!("引用未解析：{}", r.missing.join("、")),
                    NoticeKind::Warning,
                );
            }
            attachments = r.attachments;
            turn_input = r.text;
        }

        // -- CLI 提交次序：begin_turn 先于 push_user（用户块即冻结，之后是动态区）--
        self.transcript.begin_turn();
        self.transcript.push_user(&text);
        self.running = true;
        self.start_turn(turn_input, attachments, cx);
        cx.notify();
    }

    /// spawn run_agent_turn（rt 线程：持 conv 锁推进，事件经 ui_tx → 泵回主线程）。
    /// 用户消息入对话 + 落盘在此完成（checkpoint = 回合入口 user 消息），系统提示
    /// 每回合现场组装（技能/agent/MCP 工具清单与 goal 实时反映）。
    fn start_turn(
        &mut self,
        input: String,
        attachments: Vec<(String, String)>,
        cx: &mut Context<Self>,
    ) {
        let Some(h) = self.harness.clone() else {
            self.running = false;
            self.transcript.end_turn();
            self.push_notice(
                "未初始化完成，无法启动回合（⌘, 打开设置）",
                NoticeKind::Error,
            );
            cx.notify();
            return;
        };
        // 复位打断信号：start_turn 只在上一回合发出终态事件（Done/Error）之后才会被调到，
        // 旧任务此后不再读 cancel，主线程此处复位安全。
        // 若在 task 内复位，"任务尚未启动即被打断"时会把针对自己的 cancel 抹掉，
        // 随后 is_cancelled 读到 false 照常运行并挂死（case03 并行跑稳定复现）。
        h.deps.cancel_tx.send_replace(false);
        let deps = h.deps.clone();
        let hist = self.hist.clone();
        let sink = event_sink(h.ui_tx.clone());
        let ui_tx = h.ui_tx.clone();
        let goal = self.goal.clone();
        let skills: Vec<SkillDefinition> = h.skills.all().into_iter().cloned().collect();
        let agents: Vec<AgentDefinition> = h.agents.all().into_iter().cloned().collect();
        let project_doc = h.project_doc.clone();
        let tool_names: Vec<String> = deps
            .registry
            .definitions()
            .iter()
            .map(|d| d.name.clone())
            .collect();
        // 思考深度：每次 launch_turn 读 desktop.json（改设置即时生效，无需重建 harness）
        let effort = current_effort();
        let language = self.cfg.language;
        self.rt.spawn(async move {
            // 锁饿死防御：上游任务若异常持锁（如流挂起旧 bug），不能静默饿死后续回合。
            // 超时 → 经事件流发 Error + Done，UI 正常收尾并给用户明确文案。
            let mut guard = match tokio::time::timeout(HIST_LOCK_TIMEOUT, hist.lock()).await {
                Ok(g) => g,
                Err(_) => {
                    let _ = ui_tx.send(UiEvent::Agent(AgentEvent::Error {
                        message: format!(
                            "回合历史锁被占用超过 {}s（上一回合可能未正常收尾），本次回合已放弃",
                            HIST_LOCK_TIMEOUT.as_secs()
                        ),
                    }));
                    let _ = ui_tx.send(UiEvent::Agent(AgentEvent::Done {
                        reply: String::new(),
                    }));
                    return;
                }
            };
            // 用户消息（@ 图片附件 → Image 块）入对话 + 会话落盘
            let mut content = vec![ContentBlock::text(input.clone())];
            for (_, url) in &attachments {
                if let Some((meta, data)) =
                    url.strip_prefix("data:").and_then(|r| r.split_once(','))
                {
                    content.push(ContentBlock::Image {
                        media_type: meta.split(';').next().unwrap_or("image/png").to_string(),
                        data: data.to_string(),
                    });
                }
            }
            let msg = Message::new(Role::User, content);
            guard.push(msg);
            deps.record_message(guard.messages.last().expect("刚 push 必有"));
            let system = build_system_prompt(SystemPromptOpts {
                language,
                tool_names,
                skills: &skills,
                project_doc,
                goal,
                agents: &agents,
                pua_directive: Some(build_pua_directive(&skills)),
            });
            let opts = RunOpts::new(vec![system])
                .with_tools(
                    deps.registry
                        .definitions()
                        .iter()
                        .map(|d| d.to_spec())
                        .collect(),
                )
                // 0 = 不限轮数
                .with_max_turns(0)
                .with_thinking(effort);
            let _ = run_agent_turn(&deps, &mut guard, &opts, &sink).await;
        });
        cx.notify();
    }

    // ---------- goal 外层循环 ----------

    /// spawn goal 完成裁判（独立单发调用，结果经 GoalCheck 事件回主线程）
    fn spawn_goal_check(&mut self) {
        let Some(h) = self.harness.clone() else {
            return;
        };
        let Some(goal) = self.goal.clone() else {
            return;
        };
        let make_backend = h.deps.make_backend.clone();
        let model = h.deps.current_model();
        let cancel = h.deps.cancel_tx.subscribe();
        let ui_tx = h.ui_tx.clone();
        self.rt.spawn(async move {
            let done = match (make_backend)(&model) {
                Ok(backend) => check_goal_complete(backend.as_ref(), &model, &goal, cancel).await,
                Err(_) => false,
            };
            let _ = ui_tx.send(UiEvent::GoalCheck(done));
        });
    }

    /// goal 裁判结果：达成 → 收官 + drain 队列；未达成 → 以 goal prompt 再开一轮。
    /// 回合已被打断/出错收尾时，裁判结果是陈旧事件，直接丢弃。
    fn on_goal_check(&mut self, done: bool, cx: &mut Context<Self>) {
        if !self.running || self.goal.is_none() {
            return;
        }
        let goal = self.goal.clone().unwrap_or_default();
        if done {
            self.push_notice("goal 已达成", NoticeKind::Success);
            self.goal = None;
            self.goal_rounds = 0;
            self.finish_run(cx);
        } else if self.goal_rounds >= MAX_GOAL_ROUNDS {
            self.push_notice(
                format!("goal 已达 {MAX_GOAL_ROUNDS} 轮上限，循环终止"),
                NoticeKind::Warning,
            );
            self.goal = None;
            self.goal_rounds = 0;
            self.finish_run(cx);
        } else {
            self.goal_rounds += 1;
            self.push_notice(format!("goal 第 {} 轮", self.goal_rounds), NoticeKind::Info);
            let prompt = build_goal_prompt(&goal);
            self.start_turn(prompt, vec![], cx);
        }
    }

    // ---------- 内核事件处理 ----------

    /// 事件泵入口：Agent 事件 / 权限确认 / goal 裁判
    pub(crate) fn on_ui_event(&mut self, ev: UiEvent, cx: &mut Context<Self>) {
        match ev {
            UiEvent::Agent(e) => self.on_agent_event(e, cx),
            UiEvent::Confirm(req) => {
                self.pending_permission = Some(PendingPermission {
                    tool: req.tool,
                    args_summary: req.args_summary,
                    respond: Some(req.respond),
                });
            }
            UiEvent::GoalCheck(done) => self.on_goal_check(done, cx),
        }
        cx.notify();
    }

    /// 事件 → transcript 状态机（唯一事实源）；UI 只补三层旁路：
    /// Usage 累计、Done 的 goal 循环推进、Error 的防御性收尾。
    pub(crate) fn on_agent_event(&mut self, ev: AgentEvent, cx: &mut Context<Self>) {
        if let AgentEvent::Usage(u) = &ev {
            self.tokens_total += u.input_tokens + u.output_tokens;
        }
        self.transcript.apply(ev.clone());
        match ev {
            // 回合收尾：goal 激活时优先推进 goal 循环（queue 等 goal 结束再消化）。
            // running=false 时收到的 Done 是被放弃/已超时的陈旧回合，忽略以防污染当前状态。
            AgentEvent::Done { reply } => {
                if !self.running {
                    return;
                }
                if self.goal.is_some() && !reply.is_empty() {
                    self.spawn_goal_check();
                } else {
                    if self.goal.is_some() {
                        self.push_notice("goal 循环已终止（回合被打断或出错）", NoticeKind::Info);
                        self.goal = None;
                        self.goal_rounds = 0;
                    }
                    self.finish_run(cx);
                }
                self.refresh_ctx();
            }
            // 错误已由 transcript.apply 落为 Error notice。runner 契约是
            // Error 后必有 Done（空回复）收口；但锁超时等防御路径可能成对
            // 发 Error+Done——Done 到达时 running 已 false 会被陈旧门忽略，
            // 此处先防御性收尾，避免错误回合挂着不放行队列。
            AgentEvent::Error { .. } if self.running => {
                self.transcript.end_turn();
                self.finish_run(cx);
            }
            _ => {}
        }
    }

    // ---------- 回合收尾 / 队列 / 打断 / 权限 ----------

    /// 回合收尾：复位状态，队列非空则出队并按输入类别再解释（/ 与 ! 出队时
    /// 才本地消化，不进模型回合——CLI 队列 (display, payload) 的 UI 对应）。
    fn finish_run(&mut self, cx: &mut Context<Self>) {
        self.running = false;
        if let Some(next) = self.queue.first().cloned() {
            self.queue.remove(0);
            self.dispatch_input(next, cx);
        }
    }

    /// ESC：关补全 → 拒绝待裁决权限 → 打断回合 → 返回 Chat 视图
    pub(crate) fn interrupt(&mut self, _cx: &mut Context<Self>) {
        // 顶层浮层优先关闭：会话重命名弹窗 → 会话右键菜单 → 文件重命名弹窗 → 文件右键菜单
        if self.session_rename.is_some() {
            self.session_rename = None;
            return;
        }
        if self.session_menu.is_some() {
            self.session_menu = None;
            return;
        }
        if self.rename_state.is_some() {
            self.rename_state = None;
            return;
        }
        if self.context_menu.is_some() {
            self.context_menu = None;
            return;
        }
        if self.completion.is_open() {
            self.completion.close();
            return;
        }
        if let Some(p) = self.pending_permission.as_mut() {
            if let Some(tx) = p.respond.take() {
                let _ = tx.send(Decision::Deny);
            }
            self.pending_permission = None;
            return;
        }
        if self.running {
            if let Some(h) = &self.harness {
                let _ = h.deps.cancel_tx.send(true);
            }
            // transcript 打中断标记（Interrupted 块可见反馈）。不做"乐观收尾"：
            // 若在此处提前 finish_run，用户立刻再发消息会触发 start_turn 把
            // cancel 复位为 false，旧任务可能还没观察到 cancel=true，从此永远
            // 错过打断信号并持 conv 锁挂死（实测复现的"ESC 后再输入无响应"）。
            // Done(空回复)/Error 事件会随后到达并调用 finish_run 正常收尾。
            self.transcript.interrupt_current();
            return;
        }
        if self.view != View::Chat {
            self.view = View::Chat;
        }
    }

    /// 明细视图切换（ctrl+o）：CLI detail_view 同款——思考/工具内部行展开收起
    pub(crate) fn toggle_detail(&mut self) {
        self.transcript.detail_view = !self.transcript.detail_view;
    }

    /// 权限弹窗裁决：respond oneshot 回传 runner（AllowAlways 的沉淀在 runner 内）
    pub(crate) fn resolve_permission(&mut self, decision: Decision, cx: &mut Context<Self>) {
        if let Some(tx) = self.pending_permission.take().and_then(|p| p.respond) {
            let _ = tx.send(decision);
        }
        cx.notify();
    }

    // ---------- 补全状态机 ----------

    fn sync_completion(&mut self, text: &str) {
        self.completion.sync_from_input(text);
        // @ 引用 ↔ 文件树联动：正文里的 @path 同步给文件树做“@即选中”高亮
        self.mentioned = mcode_core::util::file_refs::extract_refs(text);
        if self.completion.is_open() {
            let q = self.completion.query.clone();
            self.candidates = match self.completion.trigger {
                Some(Trigger::Slash) => filter_commands(&q)
                    .into_iter()
                    .map(|c| Candidate {
                        name: c.name.into(),
                        // CLI 口径：补全提示 =「用法 — 作用」
                        desc: format!("{} — {}", c.usage, c.desc),
                        is_dir: false,
                    })
                    .chain(slash_skill_candidates(&self.ext_skills, &q))
                    .collect(),
                // 层级下钻（对齐 CLI）：按 query 末个 '/' 拆目录前缀，只列该目录
                // 的立即子项并前缀过滤——不再对整棵树做扁平 substring 全表扫
                Some(Trigger::At) => at_candidates_from_tree(&self.file_tree, &q),
                None => vec![],
            };
        } else {
            self.candidates.clear();
        }
    }
}

/// @ 补全候选：层级下钻口径。
/// query 按末个 '/' 拆成目录前缀 + 过滤词，只列该目录的立即子项（树内目录
/// 项带尾随 '/'），前缀匹配，最多 12 条。纯函数便于测试。
pub(crate) fn at_candidates_from_tree(tree: &[String], query: &str) -> Vec<Candidate> {
    let (dir_prefix, filter) = match query.rsplit_once('/') {
        Some((d, f)) => (format!("{d}/"), f),
        None => (String::new(), query),
    };
    let mut out: Vec<Candidate> = vec![];
    for p in tree {
        let Some(rest) = p.strip_prefix(dir_prefix.as_str()) else {
            continue;
        };
        if rest.is_empty() {
            continue;
        }
        // 树内目录项形如 "src/"；文件项形如 "src/api/x.ts"。先按首个 '/'
        // 切出立即子项名（嵌套路径归组到首段），再区分目录/文件
        let (name, is_dir) = match rest.split_once('/') {
            Some((first, _)) => (first, true),
            None => (rest, false),
        };
        if !filter.is_empty() && !name.starts_with(filter) {
            continue;
        }
        let full = format!("{dir_prefix}{name}");
        if out.iter().any(|c| c.name == full) {
            continue;
        }
        out.push(Candidate {
            name: full,
            desc: String::new(),
            is_dir,
        });
        if out.len() >= 12 {
            break;
        }
    }
    out
}

impl Focusable for App {
    fn focus_handle(&self, _: &GpuiApp) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for App {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::views::chat_view::render_app(self, window, cx)
    }
}
