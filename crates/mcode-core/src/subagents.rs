//! subagents：agent 定义/注册表 + `task` 工具族（同步/后台派发、递归护栏）。
//!
//! 对齐 CC 的 AgentTool/Task 核心语义：
//! - agent 定义 = 内置 `general-purpose` + `~/.mcode/agents/*.md`（用户）
//!   与 `.mcode/agents/*.md`（项目，优先）的 frontmatter 定义；
//! - `task` 工具派发子代理：独立会话、独立回合循环（复用
//!   [`run_agent_turn`]）、工具表按定义过滤；
//! - **递归护栏**：子代理的工具表永远剥离 task 系工具（子代理不能再
//!   派子代理），与 CC「subagents can't spawn subagents」一致；
//! - 子代理事件带 `scope = agent_type` 经 [`TaskEventBus`] 冒泡给 UI
//!   （TextDelta/Done 不冒泡——最终报告随工具结果返回，避免污染父
//!   会话文本流）；
//! - `run_in_background=true` 立即返回任务 id，`task_output` 收割，
//!   `task_stop` 终止（独立取消通道，不影响父会话）。

use crate::agent::runner::{run_agent_turn, AgentDeps, MakeBackend, RunOpts};
use crate::agent::{AgentEvent, EventSink};
use crate::hooks::HookRunner;
use crate::message::{Conversation, Message};
use crate::permission::PermissionManager;
use crate::tools::{Tool, ToolContext, ToolDefinition, ToolRegistry, ToolResult};
use anyhow::Result;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// 子代理工具表里永远剥离的父级派发工具（递归护栏的落地形式）。
const RESERVED_TASK_TOOLS: [&str; 3] = ["task", "task_output", "task_stop"];

// ---------------------------------------------------------------------------
// TaskEventBus — 父回合事件汇的注入点
// ---------------------------------------------------------------------------

/// 父回合事件汇槽位：runner 每回合开始时写入自己的 sink，`task` 工具
/// 执行时读取并把子代理事件（scope 化）冒泡上去。
///
/// 不清槽：回合结束后槽里留着上一回合的 sink（Arc 保活），后台子代理
/// 完成时的进度事件仍能送达 UI。嵌套无冲突——子代理拿到的 [`AgentDeps`]
/// 是新构造的，总线是独立的默认空槽。
#[derive(Clone, Default)]
pub struct TaskEventBus(Arc<Mutex<Option<EventSink>>>);

impl TaskEventBus {
    /// runner 在回合入口调用。
    pub fn set(&self, sink: EventSink) {
        *self.0.lock().expect("事件总线锁") = Some(sink);
    }

    /// task 工具执行时取当前父 sink（无 → None，调用方丢弃事件）。
    pub fn current(&self) -> Option<EventSink> {
        self.0.lock().expect("事件总线锁").clone()
    }
}

// ---------------------------------------------------------------------------
// AgentDefinition
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentSource {
    Builtin,
    User,
    Project,
}

/// 一个可派发 agent 的定义（markdown frontmatter 或内置）。
#[derive(Debug, Clone)]
pub struct AgentDefinition {
    /// 派发时的 `subagent_type`。
    pub agent_type: String,
    /// 何时使用（task 工具描述里列给模型选型）。
    pub when_to_use: String,
    /// 工具白名单；`None` = 全部（含 `["*"]` 写法）。
    pub tools: Option<Vec<String>>,
    /// 工具黑名单（优先于白名单）。
    pub disallowed_tools: Vec<String>,
    /// 覆盖模型；`None` = 继承父会话模型。
    pub model: Option<String>,
    /// 系统提示（frontmatter 缺省时用 [`DEFAULT_SUBAGENT_PROMPT`]）。
    pub system_prompt: String,
    /// 回合上限；`None` = 不限。
    pub max_turns: Option<usize>,
    pub source: AgentSource,
}

impl AgentDefinition {
    pub fn builtin_general_purpose() -> Self {
        AgentDefinition {
            agent_type: "general-purpose".into(),
            when_to_use: "通用研究/检索/多步执行代理：跨文件搜索代码与配置、\
                分析系统架构、调查需要浏览多个文件才能回答的问题。"
                .into(),
            tools: None,
            disallowed_tools: vec![],
            model: None,
            system_prompt: String::new(), // 缺省 → DEFAULT_SUBAGENT_PROMPT
            max_turns: None,
            source: AgentSource::Builtin,
        }
    }

    fn system_prompt_or_default(&self) -> String {
        if self.system_prompt.trim().is_empty() {
            DEFAULT_SUBAGENT_PROMPT.into()
        } else {
            self.system_prompt.clone()
        }
    }
}

/// 子代理默认系统提示（对齐 CC general-purpose 的职责说明）。
pub const DEFAULT_SUBAGENT_PROMPT: &str = "你是 mcode 的子代理。根据调用方给出的任务，\
    使用可用工具完整执行——做到位即可，不过度加工，也不留半成品。\n\n\
    强项：\n\
    - 在大型代码库里检索代码、配置与模式\n\
    - 分析多文件系统架构\n\
    - 执行需要探索多个文件的多步调查\n\n\
    准则：\n\
    - 不确定目标位置时广撒网搜索；已知具体文件时直接读取\n\
    - 先宽后窄：一种搜索策略无果就换角度，多试几种命名约定\n\
    - 非必要不创建文件；优先修改现有文件而非新建\n\
    - 完成后输出一份精炼报告：做了什么 + 关键发现。报告会被调用方转述给用户，\
    只保留要点。";

// ---------------------------------------------------------------------------
// frontmatter 解析
// ---------------------------------------------------------------------------

/// 解析 agent markdown：`---` 包裹的 key: value frontmatter + 正文（系统提示）。
///
/// 支持键：name、description/when_to_use、tools（逗号分隔，`*` = 全部）、
/// disallowedTools、model（`inherit` = 继承）、maxTurns。
/// 无 frontmatter → 整个正文当系统提示，agent_type 取文件名。
pub fn parse_agent_markdown(src: &str, default_name: &str) -> AgentDefinition {
    let trimmed = src.trim_start();
    if !trimmed.starts_with("---") {
        return AgentDefinition {
            agent_type: default_name.to_string(),
            when_to_use: first_line_summary(src),
            tools: None,
            disallowed_tools: vec![],
            model: None,
            system_prompt: src.trim().to_string(),
            max_turns: None,
            source: AgentSource::User,
        };
    }

    let Some((fm, body)) = crate::util::frontmatter::split(trimmed) else {
        // `---` 未闭合等畸形 → 整体当正文
        return AgentDefinition {
            agent_type: default_name.to_string(),
            when_to_use: first_line_summary(src),
            tools: None,
            disallowed_tools: vec![],
            model: None,
            system_prompt: src.trim().to_string(),
            max_turns: None,
            source: AgentSource::User,
        };
    };
    let system_prompt = body.trim().to_string();
    let get =
        |k: &str| -> Option<String> { crate::util::frontmatter::get(&fm, k).map(str::to_string) };
    let list = |k: &str| -> Option<Vec<String>> { crate::util::frontmatter::get_list(&fm, k) };

    let tools = list("tools").filter(|l| !(l.len() == 1 && l[0] == "*")); // "*" → None = 全部
    let model = get("model").filter(|m| !m.is_empty() && !m.eq_ignore_ascii_case("inherit"));
    let max_turns = get("maxTurns").and_then(|v| v.parse::<usize>().ok());
    let when_to_use = get("description")
        .or_else(|| get("when_to_use"))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| first_line_summary(&system_prompt));

    AgentDefinition {
        agent_type: get("name")
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| default_name.to_string()),
        when_to_use,
        tools,
        disallowed_tools: list("disallowedTools").unwrap_or_default(),
        model,
        system_prompt,
        max_turns,
        source: AgentSource::User,
    }
}

/// 正文首行摘要（description 缺省时的 when_to_use 兜底）。
fn first_line_summary(body: &str) -> String {
    let line = body
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    truncate_chars(line, 80)
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{cut}…")
}

// ---------------------------------------------------------------------------
// AgentRegistry
// ---------------------------------------------------------------------------

/// agent 注册表：内置 + 用户/项目目录（后加载的同名覆盖前者）。
#[derive(Debug, Clone, Default)]
pub struct AgentRegistry {
    agents: BTreeMap<String, AgentDefinition>,
}

impl AgentRegistry {
    /// 仅内置（general-purpose）。
    pub fn with_builtins() -> Self {
        let mut r = Self::default();
        r.register(AgentDefinition::builtin_general_purpose());
        r
    }

    pub fn register(&mut self, def: AgentDefinition) {
        self.agents.insert(def.agent_type.clone(), def);
    }

    pub fn get(&self, name: &str) -> Option<&AgentDefinition> {
        self.agents.get(name)
    }

    pub fn names(&self) -> Vec<String> {
        self.agents.keys().cloned().collect()
    }

    pub fn all(&self) -> Vec<&AgentDefinition> {
        self.agents.values().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.agents.is_empty()
    }

    /// task 工具描述里的选型清单：`- name: when_to_use`。
    pub fn list_for_prompt(&self) -> String {
        self.agents
            .values()
            .map(|d| format!("- {}: {}", d.agent_type, d.when_to_use))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// 从目录加载 `*.md`（`dirs` 顺序即优先级，后者覆盖前者；内置永远先注册）。
    /// 目录不存在 → 跳过；坏文件跳过（不拖垮整体）。
    pub fn load_dirs(dirs: &[&Path]) -> Self {
        let mut r = Self::with_builtins();
        for dir in dirs {
            let Ok(rd) = std::fs::read_dir(dir) else {
                continue;
            };
            let mut files: Vec<_> = rd.filter_map(|e| e.ok()).map(|e| e.path()).collect();
            files.sort();
            for path in files {
                if !path.extension().is_some_and(|x| x == "md") {
                    continue;
                }
                let default_name = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("agent")
                    .to_string();
                let Ok(src) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let mut def = parse_agent_markdown(&src, &default_name);
                def.source = AgentSource::User;
                r.register(def);
            }
        }
        r
    }
}

// ---------------------------------------------------------------------------
// task 工具族
// ---------------------------------------------------------------------------

/// 后台子代理任务表（task / task_output / task_stop 共享）。
pub type TaskTable = Arc<Mutex<BTreeMap<String, BackgroundAgent>>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentTaskStatus {
    Running,
    Completed,
    Failed,
    Killed,
}

/// 后台子代理任务条目。
pub struct BackgroundAgent {
    pub id: String,
    pub agent_type: String,
    pub description: String,
    pub status: AgentTaskStatus,
    /// 终态文本：Completed = 最终报告；Failed = 原因。
    pub result: Option<String>,
    /// 取消信号（task_stop 触发；后台循环的 ToolContext 持对应 receiver）。
    pub cancel: Arc<tokio::sync::watch::Sender<bool>>,
}

/// task 工具共有的运行时部件（从父 [`AgentDeps`] 摘出）。
struct TaskRuntime {
    agents: Arc<AgentRegistry>,
    make_backend: MakeBackend,
    fallback_models: Vec<String>,
    base_registry: Arc<ToolRegistry>,
    model: String,
    perms: Arc<tokio::sync::Mutex<PermissionManager>>,
    hooks: Option<Arc<HookRunner>>,
    bus: TaskEventBus,
}

/// `task`：派发子代理执行一段独立任务，返回最终报告。
pub struct TaskTool {
    rt: Arc<TaskRuntime>,
    tasks: TaskTable,
    seq: AtomicU64,
}

/// `task_output`：读取/收割后台子代理任务。
pub struct TaskOutputTool {
    tasks: TaskTable,
}

/// `task_stop`：终止后台子代理任务。
pub struct TaskStopTool {
    tasks: TaskTable,
}

/// 把 task 工具族装进 deps.registry。返回任务表（TUI/测试轮询用）。
pub fn install_task_tools(deps: &AgentDeps, agents: AgentRegistry) -> TaskTable {
    let tasks: TaskTable = Arc::new(Mutex::new(BTreeMap::new()));
    let rt = Arc::new(TaskRuntime {
        agents: Arc::new(agents),
        make_backend: deps.make_backend.clone(),
        fallback_models: deps.fallback_models.clone(),
        base_registry: deps.registry.clone(),
        model: deps.model.clone(),
        perms: deps.perms.clone(),
        hooks: deps.hooks.clone(),
        bus: deps.task_bus.clone(),
    });
    deps.registry.register(Arc::new(TaskTool {
        rt,
        tasks: tasks.clone(),
        seq: AtomicU64::new(0),
    }));
    deps.registry.register(Arc::new(TaskOutputTool {
        tasks: tasks.clone(),
    }));
    deps.registry.register(Arc::new(TaskStopTool {
        tasks: tasks.clone(),
    }));
    tasks
}

/// 组装子代理的工具表：基础表剥掉 task 系（递归护栏），再按定义
/// 白/黑名单过滤。
pub fn build_sub_registry(base: &ToolRegistry, def: &AgentDefinition) -> ToolRegistry {
    let reg = ToolRegistry::new_empty();
    for name in base.names() {
        if RESERVED_TASK_TOOLS.contains(&name.as_str()) {
            continue; // 递归护栏：子代理不能再派子代理
        }
        if def.disallowed_tools.iter().any(|d| d == &name) {
            continue;
        }
        if let Some(allow) = &def.tools {
            if !allow.iter().any(|a| a == &name) {
                continue;
            }
        }
        if let Some(t) = base.get(&name) {
            reg.register(t);
        }
    }
    reg
}

/// 子代理事件冒泡包装：scope = agent_type；文本流/终态不冒泡。
fn scoped_sink(scope: &str, parent: EventSink) -> EventSink {
    let scope = scope.to_string();
    Arc::new(move |e: AgentEvent| {
        let forwarded = match e {
            AgentEvent::Thinking { text, .. } => AgentEvent::Thinking {
                text,
                scope: Some(scope.clone()),
            },
            AgentEvent::ToolUseStart { id, name, .. } => AgentEvent::ToolUseStart {
                id,
                name,
                scope: Some(scope.clone()),
            },
            AgentEvent::ToolInputDelta {
                id, partial_json, ..
            } => AgentEvent::ToolInputDelta {
                id,
                partial_json,
                scope: Some(scope.clone()),
            },
            AgentEvent::ToolResult {
                name,
                success,
                output,
                display,
                ..
            } => AgentEvent::ToolResult {
                name,
                success,
                output,
                display,
                scope: Some(scope.clone()),
            },
            // Usage / HookNotice 原样冒泡（记费用、提示均可跨代理）
            AgentEvent::Usage(u) => AgentEvent::Usage(u),
            AgentEvent::HookNotice { message } => AgentEvent::HookNotice { message },
            // TextDelta / Done / Retrying / Recovering / Compacted / Error /
            // Confirm：子代理的文本流与终态不进父会话事件流
            _ => return,
        };
        parent(forwarded);
    })
}

impl TaskTool {
    /// 执行一次子代理回合循环，返回 (报告, 成功?, 实际模型)。
    /// 全部收 owned 值：tokio::spawn 要求 'static，也避免引用型 async fn
    /// 的高阶生命周期推断问题。
    async fn run_sync_agent(
        rt: Arc<TaskRuntime>,
        def: AgentDefinition,
        prompt: String,
        model_override: Option<String>,
        ctx: ToolContext,
        sink: EventSink,
    ) -> (String, bool, String) {
        let registry = build_sub_registry(&rt.base_registry, &def);
        let model = model_override
            .clone()
            .or_else(|| def.model.clone())
            .unwrap_or_else(|| rt.model.clone());
        let fallbacks = if model_override.is_some() || def.model.is_some() {
            vec![] // 显式指定模型时不走 fallback 链
        } else {
            rt.fallback_models.clone()
        };

        let sub_deps = AgentDeps {
            model: model.clone(),
            fallback_models: fallbacks,
            make_backend: rt.make_backend.clone(),
            registry: Arc::new(registry),
            perms: rt.perms.clone(),
            confirm: None,
            // 前台继承父 ctx.cancel（父打断 → 子停）；后台传独立 receiver
            tool_ctx: ctx.clone(),
            cancel_tx: tokio::sync::watch::channel(false).0,
            token_estimator: Box::new(crate::token::raw_estimate_messages),
            hooks: rt.hooks.clone(),
            session: None,
            task_bus: TaskEventBus::default(),
        };

        let mut sub_conv = Conversation::new();
        sub_conv.push(Message::user_text(prompt));
        let opts = RunOpts::new(vec![def.system_prompt_or_default()]).with_tools(
            sub_deps
                .registry
                .definitions()
                .iter()
                .map(|d| d.to_spec())
                .collect(),
        );
        // max_turns: 0 = 不限（对齐 CC 默认）
        let opts = if def.max_turns.unwrap_or(0) > 0 {
            opts.with_max_turns(def.max_turns.unwrap())
        } else {
            opts
        };
        let outcome = run_agent_turn(&sub_deps, &mut sub_conv, &opts, &sink).await;
        if outcome.interrupted {
            return ("子代理被打断".into(), false, outcome.final_model);
        }
        if let Some(err) = outcome.error {
            return (format!("子代理执行失败: {err}"), false, outcome.final_model);
        }
        if outcome.reply.trim().is_empty() {
            return ("子代理未返回文本报告".into(), false, outcome.final_model);
        }
        (
            format!(
                "{}\n\n[{} | {} 轮 | model: {}]",
                outcome.reply, def.agent_type, outcome.turns, outcome.final_model
            ),
            true,
            outcome.final_model,
        )
    }
}

#[async_trait::async_trait]
impl Tool for TaskTool {
    fn def(&self) -> ToolDefinition {
        let agents = self.rt.agents.list_for_prompt();
        ToolDefinition {
            name: "task".into(),
            description: format!(
                "派发一个子代理独立执行任务，完成后返回其最终报告。\
                适合：跨多文件的检索/调查、可与主线并行的大段执行。\
                可用 agent 类型：\n{agents}\n\
                不指定 subagent_type 时用 general-purpose。"
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "description": { "type": "string", "description": "任务简述（3-5 个词）" },
                    "prompt": { "type": "string", "description": "交给子代理的完整任务说明" },
                    "subagent_type": { "type": "string", "description": "agent 类型（省略 = general-purpose）" },
                    "model": { "type": "string", "description": "覆盖模型（省略 = 继承父会话）" },
                    "run_in_background": { "type": "boolean", "description": "后台执行（默认 false）" }
                },
                "required": ["description", "prompt"]
            }),
            is_read_only: false,
        }
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> Result<ToolResult> {
        let description = input["description"].as_str().unwrap_or_default().trim();
        let prompt = input["prompt"].as_str().unwrap_or_default().trim();
        if description.is_empty() || prompt.is_empty() {
            return Ok(ToolResult::err("task: description 与 prompt 均不能为空"));
        }
        let agent_type = input["subagent_type"].as_str().unwrap_or("general-purpose");
        let Some(def) = self.rt.agents.get(agent_type).cloned() else {
            return Ok(ToolResult::err(format!(
                "task: 未知 agent 类型 {agent_type}。可用: {}",
                self.rt.agents.names().join(", ")
            )));
        };
        let model_override = input["model"].as_str().filter(|s| !s.is_empty());
        let sink = self
            .rt
            .bus
            .current()
            .map(|parent| scoped_sink(agent_type, parent));

        if input["run_in_background"].as_bool().unwrap_or(false) {
            self.seq.fetch_add(1, Ordering::Relaxed);
            let id = format!("agent_{}", self.seq.load(Ordering::Relaxed));
            // 独立取消通道：task_stop 只停目标任务，不影响父会话
            let (stx, srx) = tokio::sync::watch::channel(false);
            let sub_ctx = ToolContext {
                cwd: ctx.cwd.clone(),
                cancel: srx,
                shared: ctx.shared.clone(),
                extra: ctx.extra.clone(),
            };
            self.tasks.lock().expect("任务表锁").insert(
                id.clone(),
                BackgroundAgent {
                    id: id.clone(),
                    agent_type: def.agent_type.clone(),
                    description: description.to_string(),
                    status: AgentTaskStatus::Running,
                    result: None,
                    cancel: Arc::new(stx),
                },
            );
            let rt = self.rt.clone();
            let prompt = prompt.to_string();
            let model_override = model_override.map(str::to_string);
            let tasks = self.tasks.clone();
            let id_for_task = id.clone();
            let sink = sink.unwrap_or_else(|| Arc::new(|_| {}));
            tokio::spawn(async move {
                let (report, ok, _model) =
                    TaskTool::run_sync_agent(rt, def, prompt, model_override, sub_ctx, sink).await;
                let mut t = tasks.lock().expect("任务表锁");
                if let Some(b) = t.get_mut(&id_for_task) {
                    // task_stop 先到（Killed）→ 不覆盖终态
                    if b.status == AgentTaskStatus::Running {
                        b.status = if ok {
                            AgentTaskStatus::Completed
                        } else {
                            AgentTaskStatus::Failed
                        };
                        b.result = Some(report);
                    }
                }
            });
            return Ok(ToolResult::ok(format!(
                "后台子代理已启动: {id}（{agent_type}）。\n用 task_output 收割结果，task_stop 终止。"
            )));
        }

        let sink = sink.unwrap_or_else(|| Arc::new(|_| {}));
        let (report, ok, _model) = TaskTool::run_sync_agent(
            self.rt.clone(),
            def,
            prompt.to_string(),
            model_override.map(str::to_string),
            ctx.clone(),
            sink,
        )
        .await;
        if ok {
            Ok(ToolResult::ok(report))
        } else {
            Ok(ToolResult::err(report))
        }
    }
}

#[async_trait::async_trait]
impl Tool for TaskOutputTool {
    fn def(&self) -> ToolDefinition {
        ToolDefinition {
            name: "task_output".into(),
            description: "读取后台子代理任务的输出。任务已结束时返回最终报告并释放任务槽；\
                仍在运行时返回当前状态。"
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "task_id": { "type": "string", "description": "task 返回的后台任务 id（如 agent_1）" }
                },
                "required": ["task_id"]
            }),
            is_read_only: true,
        }
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> Result<ToolResult> {
        let id = input["task_id"].as_str().unwrap_or_default();
        let mut t = self.tasks.lock().expect("任务表锁");
        let Some(b) = t.get(id) else {
            return Ok(ToolResult::err(format!(
                "task_output: 任务 {id} 不存在（可能已被收割）。"
            )));
        };
        match b.status {
            AgentTaskStatus::Running => Ok(ToolResult::ok(format!(
                "[任务 {id}（{}）运行中] {}",
                b.agent_type, b.description
            ))),
            AgentTaskStatus::Completed => {
                let b = t.remove(id).expect("刚检查过存在");
                Ok(ToolResult::ok(format!(
                    "[任务 {id} 已完成]\n{}",
                    b.result.unwrap_or_default()
                )))
            }
            AgentTaskStatus::Failed => {
                let b = t.remove(id).expect("刚检查过存在");
                Ok(ToolResult::err(format!(
                    "[任务 {id} 失败]\n{}",
                    b.result.unwrap_or_default()
                )))
            }
            AgentTaskStatus::Killed => {
                t.remove(id);
                Ok(ToolResult::ok(format!("[任务 {id} 已被终止]")))
            }
        }
    }
}

#[async_trait::async_trait]
impl Tool for TaskStopTool {
    fn def(&self) -> ToolDefinition {
        ToolDefinition {
            name: "task_stop".into(),
            description: "终止一个后台子代理任务。".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "task_id": { "type": "string" }
                },
                "required": ["task_id"]
            }),
            is_read_only: false,
        }
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> Result<ToolResult> {
        let id = input["task_id"].as_str().unwrap_or_default();
        let mut t = self.tasks.lock().expect("任务表锁");
        let Some(b) = t.get_mut(id) else {
            return Ok(ToolResult::err(format!("task_stop: 任务 {id} 不存在")));
        };
        b.status = AgentTaskStatus::Killed;
        let _ = b.cancel.send(true);
        Ok(ToolResult::ok(format!("已终止 {id}")))
    }
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::runner::run_agent_turn;
    use crate::api::{ApiError, StreamEvent};
    use crate::message::StopReason;
    use std::collections::VecDeque;

    // 与 runner 测试一致的脚本化后端（两个消费方共享同一条脚本队列）。
    type ScriptQueue = Arc<Mutex<VecDeque<Result<Vec<StreamEvent>, ApiError>>>>;

    struct ScriptBackend {
        script: ScriptQueue,
    }

    #[async_trait::async_trait]
    impl crate::api::ModelBackend for ScriptBackend {
        fn backend_name(&self) -> &'static str {
            "script"
        }
        async fn stream(
            &self,
            _req: crate::api::StreamRequest,
        ) -> Result<futures::stream::BoxStream<'static, Result<StreamEvent, ApiError>>, ApiError>
        {
            let next = self.script.lock().unwrap().pop_front().unwrap_or_else(|| {
                Ok(vec![StreamEvent::Done {
                    stop_reason: StopReason::EndTurn,
                }])
            });
            match next {
                Ok(events) => Ok(Box::pin(futures::stream::iter(events.into_iter().map(Ok)))),
                Err(e) => Err(e),
            }
        }
    }

    struct EchoTool;

    #[async_trait::async_trait]
    impl Tool for EchoTool {
        fn def(&self) -> ToolDefinition {
            ToolDefinition {
                name: "echo".into(),
                description: "回声".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {"text": {"type": "string"}},
                    "required": ["text"]
                }),
                is_read_only: false,
            }
        }
        async fn execute(&self, input: Value, _ctx: &ToolContext) -> Result<ToolResult> {
            Ok(ToolResult::ok(format!(
                "echo: {}",
                input["text"].as_str().unwrap_or("")
            )))
        }
    }

    fn tool_turn(id: &str, name: &str, args: Value) -> Result<Vec<StreamEvent>, ApiError> {
        Ok(vec![
            StreamEvent::ToolUseStart {
                id: id.into(),
                name: name.into(),
            },
            StreamEvent::ToolInputDelta {
                id: id.into(),
                partial_json: serde_json::to_string(&args).unwrap(),
            },
            StreamEvent::Done {
                stop_reason: StopReason::ToolUse,
            },
        ])
    }

    fn text_turn(text: &str) -> Result<Vec<StreamEvent>, ApiError> {
        Ok(vec![
            StreamEvent::TextDelta(text.to_string()),
            StreamEvent::Done {
                stop_reason: StopReason::EndTurn,
            },
        ])
    }

    fn harness(script: ScriptQueue, registry: ToolRegistry) -> AgentDeps {
        let (ctx, _tx) = ToolContext::for_tests(std::env::temp_dir());
        let (cancel_tx, _) = tokio::sync::watch::channel(false);
        let script_for_backend = script.clone();
        AgentDeps {
            model: "main-model".into(),
            fallback_models: vec![],
            make_backend: Arc::new(move |_: &str| {
                Ok(Arc::new(ScriptBackend {
                    script: script_for_backend.clone(),
                }) as Arc<dyn crate::api::ModelBackend>)
            }),
            registry: Arc::new(registry),
            perms: Arc::new(tokio::sync::Mutex::new(PermissionManager::new(
                crate::permission::Mode::Passon,
                vec![],
            ))),
            confirm: None,
            tool_ctx: ctx,
            cancel_tx,
            token_estimator: Box::new(crate::token::raw_estimate_messages),
            hooks: None,
            session: None,
            task_bus: TaskEventBus::default(),
        }
    }

    // ---- frontmatter ----

    #[test]
    fn parses_full_frontmatter() {
        let src = "---\nname: researcher\ndescription: 检索专家\ntools: read_file, grep_file\nmodel: glm-5.3\ndisallowedTools: bash, write_file\nmaxTurns: 12\n---\n你是检索专家。";
        let d = parse_agent_markdown(src, "file_name");
        assert_eq!(d.agent_type, "researcher");
        assert_eq!(d.when_to_use, "检索专家");
        assert_eq!(
            d.tools,
            Some(vec!["read_file".to_string(), "grep_file".to_string()])
        );
        assert_eq!(d.model.as_deref(), Some("glm-5.3"));
        assert_eq!(d.disallowed_tools, vec!["bash", "write_file"]);
        assert_eq!(d.max_turns, Some(12));
        assert_eq!(d.system_prompt, "你是检索专家。");
    }

    #[test]
    fn no_frontmatter_uses_body_as_prompt() {
        let d = parse_agent_markdown("第一行说明\n\n正文", "myagent");
        assert_eq!(d.agent_type, "myagent");
        assert_eq!(d.when_to_use, "第一行说明");
        assert_eq!(d.system_prompt, "第一行说明\n\n正文");
        assert!(d.tools.is_none());
    }

    #[test]
    fn wildcard_tools_and_inherit_model() {
        let src = "---\nname: a\ndescription: x\ntools: \"*\"\nmodel: inherit\n---\nprompt";
        let d = parse_agent_markdown(src, "a");
        assert!(d.tools.is_none(), "* → 全部工具");
        assert!(d.model.is_none(), "inherit → 继承");
    }

    #[test]
    fn builtin_registry_has_general_purpose() {
        let r = AgentRegistry::with_builtins();
        assert!(r.get("general-purpose").is_some());
        assert_eq!(r.names(), vec!["general-purpose"]);
        assert!(r.list_for_prompt().contains("general-purpose:"));
    }

    #[test]
    fn load_dirs_project_overrides_user() {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!("mcode-agents-{}-{n}", std::process::id()));
        let user = base.join("user");
        let project = base.join("project");
        std::fs::create_dir_all(&user).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            user.join("scout.md"),
            "---\nname: scout\ndescription: 用户版\n---\nuser prompt",
        )
        .unwrap();
        std::fs::write(
            project.join("scout.md"),
            "---\nname: scout\ndescription: 项目版\n---\nproject prompt",
        )
        .unwrap();
        std::fs::write(project.join("broken.txt"), "not md").unwrap();

        let r = AgentRegistry::load_dirs(&[&user, &project]);
        let scout = r.get("scout").unwrap();
        assert_eq!(scout.when_to_use, "项目版", "项目覆盖用户");
        assert_eq!(scout.system_prompt, "project prompt");
        let _ = std::fs::remove_dir_all(&base);
    }

    // ---- 工具表过滤 ----

    #[test]
    fn sub_registry_strips_task_tools_and_applies_filters() {
        let base = ToolRegistry::new_empty();
        base.register(Arc::new(TaskTool {
            rt: Arc::new(TaskRuntime {
                agents: Arc::new(AgentRegistry::with_builtins()),
                make_backend: Arc::new(|_| Err(anyhow::anyhow!("unused"))),
                fallback_models: vec![],
                base_registry: Arc::new(ToolRegistry::new_empty()),
                model: "m".into(),
                perms: Arc::new(tokio::sync::Mutex::new(PermissionManager::new(
                    crate::permission::Mode::Passon,
                    vec![],
                ))),
                hooks: None,
                bus: TaskEventBus::default(),
            }),
            tasks: Arc::new(Mutex::new(BTreeMap::new())),
            seq: AtomicU64::new(0),
        }));
        base.register(Arc::new(EchoTool));

        // 全部工具：task 系剥离，echo 保留
        let def = AgentDefinition::builtin_general_purpose();
        let sub = build_sub_registry(&base, &def);
        let names = sub.names();
        assert!(!names.contains(&"task".into()), "递归护栏：{names:?}");
        assert!(!names.contains(&"task_output".into()));
        assert!(!names.contains(&"task_stop".into()));
        assert!(names.contains(&"echo".into()));

        // 白名单 + 黑名单
        let mut scoped = def.clone();
        scoped.tools = Some(vec!["echo".into()]);
        assert_eq!(build_sub_registry(&base, &scoped).names(), vec!["echo"]);

        let mut banned = def.clone();
        banned.disallowed_tools = vec!["echo".into()];
        assert!(build_sub_registry(&base, &banned).names().is_empty());
    }

    // ---- 端到端派发 ----

    #[tokio::test]
    async fn task_dispatch_runs_subagent_and_returns_report() {
        let script: ScriptQueue = Arc::new(Mutex::new(VecDeque::new()));
        // 顺序：父回合1(调 task) → 子代理回合(报告) → 父回合2(收尾)
        script.lock().unwrap().push_back(tool_turn(
            "t1",
            "task",
            json!({
                "description": "调研",
                "prompt": "帮我调研"
            }),
        ));
        script.lock().unwrap().push_back(text_turn("子代理报告"));
        script.lock().unwrap().push_back(text_turn("父收尾"));

        let reg = ToolRegistry::new_empty(); // 父表只有 task（install 注入）
        let deps = harness(script, reg);
        let install_tasks = install_task_tools(&deps, AgentRegistry::with_builtins());
        drop(install_tasks);

        // runner 把父 sink 写进总线（与 run_agent_turn 入口一致）
        let events_arc: Arc<Mutex<Vec<AgentEvent>>> = Arc::new(Mutex::new(vec![]));
        {
            let ev = events_arc.clone();
            deps.task_bus
                .set(Arc::new(move |e| ev.lock().unwrap().push(e)));
        }

        let mut conv = Conversation::new();
        conv.push(Message::user_text("派个任务"));
        let opts = crate::agent::runner::RunOpts::new(vec![]).with_tools(
            deps.registry
                .definitions()
                .iter()
                .map(|d| d.to_spec())
                .collect(),
        );
        let outcome = run_agent_turn(&deps, &mut conv, &opts, &{
            let ev = events_arc.clone();
            Arc::new(move |e| ev.lock().unwrap().push(e)) as EventSink
        })
        .await;
        assert_eq!(outcome.error, None, "{}", outcome.reply);
        assert_eq!(outcome.reply, "父收尾");

        // task 结果包含子代理报告
        let msgs = conv.active_messages();
        let result_msg = msgs
            .iter()
            .find(|m| !m.tool_results().is_empty())
            .expect("有 tool_result 消息");
        let tr = result_msg
            .tool_results()
            .iter()
            .find_map(|b| match b {
                crate::message::ContentBlock::ToolResult { content, .. } => Some(content.clone()),
                _ => None,
            })
            .unwrap();
        assert!(tr.contains("子代理报告"), "{tr}");
        assert!(tr.contains("general-purpose"), "报告带 agent 元信息: {tr}");

        // 子代理工具事件带 scope 冒泡（本例无工具 → 只验证事件链不炸 + 父 Done 正常）
        let events = events_arc.lock().unwrap();
        assert!(
            matches!(events.last(), Some(AgentEvent::Done { reply }) if reply == "父收尾"),
            "最后事件是父 Done"
        );
    }

    #[tokio::test]
    async fn unknown_agent_type_is_rejected() {
        let script: ScriptQueue = Arc::new(Mutex::new(VecDeque::new()));
        script.lock().unwrap().push_back(tool_turn(
            "t1",
            "task",
            json!({"description": "x", "prompt": "y", "subagent_type": "nope"}),
        ));
        script.lock().unwrap().push_back(text_turn("done"));

        let deps = harness(script, ToolRegistry::new_empty());
        let _tasks = install_task_tools(&deps, AgentRegistry::with_builtins());
        let sink: EventSink = Arc::new(|_| {});
        deps.task_bus.set(sink.clone());

        let mut conv = Conversation::new();
        conv.push(Message::user_text("go"));
        let opts = crate::agent::runner::RunOpts::new(vec![]).with_tools(
            deps.registry
                .definitions()
                .iter()
                .map(|d| d.to_spec())
                .collect(),
        );
        let outcome = run_agent_turn(&deps, &mut conv, &opts, &sink).await;
        assert_eq!(outcome.error, None);
        let tr = conv
            .active_messages()
            .iter()
            .find_map(|m| {
                m.tool_results().into_iter().find_map(|b| match b {
                    crate::message::ContentBlock::ToolResult { content, .. } => {
                        Some(content.clone())
                    }
                    _ => None,
                })
            })
            .unwrap();
        assert!(tr.contains("未知 agent 类型"), "{tr}");
    }

    #[tokio::test]
    async fn background_task_lifecycle_via_tools() {
        let script: ScriptQueue = Arc::new(Mutex::new(VecDeque::new()));
        script.lock().unwrap().push_back(tool_turn(
            "t1",
            "task",
            json!({
                "description": "后台跑",
                "prompt": "慢慢来",
                "run_in_background": true
            }),
        ));
        script.lock().unwrap().push_back(text_turn("ok"));
        // 后台子代理的报告回合
        script.lock().unwrap().push_back(text_turn("后台报告"));

        let deps = harness(script, ToolRegistry::new_empty());
        let tasks = install_task_tools(&deps, AgentRegistry::with_builtins());
        let sink: EventSink = Arc::new(|_| {});
        deps.task_bus.set(sink.clone());

        let mut conv = Conversation::new();
        conv.push(Message::user_text("go bg"));
        let opts = crate::agent::runner::RunOpts::new(vec![]).with_tools(
            deps.registry
                .definitions()
                .iter()
                .map(|d| d.to_spec())
                .collect(),
        );
        let outcome = run_agent_turn(&deps, &mut conv, &opts, &sink).await;
        assert_eq!(outcome.error, None);

        // 后台任务启动成功
        let tr = conv
            .active_messages()
            .iter()
            .find_map(|m| {
                m.tool_results().into_iter().find_map(|b| match b {
                    crate::message::ContentBlock::ToolResult { content, .. } => {
                        Some(content.clone())
                    }
                    _ => None,
                })
            })
            .unwrap();
        assert!(tr.contains("agent_1"), "{tr}");

        // 轮询收割
        let tool = deps.registry.get("task_output").unwrap();
        let mut result = None;
        for _ in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            let r = tool
                .execute(json!({"task_id": "agent_1"}), &deps.tool_ctx)
                .await
                .unwrap();
            if r.output.contains("已完成") {
                result = Some(r);
                break;
            }
        }
        let r = result.expect("后台任务未在 5s 内完成");
        assert!(r.output.contains("后台报告"), "{}", r.output);
        // 收割后再查 → 不存在
        let r = tool
            .execute(json!({"task_id": "agent_1"}), &deps.tool_ctx)
            .await
            .unwrap();
        assert!(!r.success);
        let _ = tasks.lock().unwrap().remove("agent_1");
    }

    #[tokio::test]
    async fn task_stop_kills_running_task() {
        let script: ScriptQueue = Arc::new(Mutex::new(VecDeque::new()));
        script.lock().unwrap().push_back(tool_turn(
            "t1",
            "task",
            json!({
                "description": "会被停",
                "prompt": "长任务",
                "run_in_background": true
            }),
        ));
        script.lock().unwrap().push_back(text_turn("ok"));
        // 后台子代理：先 sleep 再报告（给 task_stop 留窗口）
        // 脚本后端的子回合不 sleep —— 这里只验证 stop 状态机

        let deps = harness(script, ToolRegistry::new_empty());
        let tasks = install_task_tools(&deps, AgentRegistry::with_builtins());
        let sink: EventSink = Arc::new(|_| {});
        deps.task_bus.set(sink.clone());

        // 直接往表里塞一个 Running 任务（隔离测试 stop 语义）
        let (stx, _srx) = tokio::sync::watch::channel(false);
        tasks.lock().unwrap().insert(
            "agent_9".into(),
            BackgroundAgent {
                id: "agent_9".into(),
                agent_type: "general-purpose".into(),
                description: "test".into(),
                status: AgentTaskStatus::Running,
                result: None,
                cancel: Arc::new(stx),
            },
        );
        let stop = deps.registry.get("task_stop").unwrap();
        let r = stop
            .execute(json!({"task_id": "agent_9"}), &deps.tool_ctx)
            .await
            .unwrap();
        assert!(r.success, "{}", r.output);
        assert_eq!(
            tasks.lock().unwrap().get("agent_9").unwrap().status,
            AgentTaskStatus::Killed
        );
        // 输出收割：Killed → 提示已终止
        let out = deps.registry.get("task_output").unwrap();
        let r = out
            .execute(json!({"task_id": "agent_9"}), &deps.tool_ctx)
            .await
            .unwrap();
        assert!(r.output.contains("已被终止"));
        assert!(tasks.lock().unwrap().get("agent_9").is_none());
    }
}
