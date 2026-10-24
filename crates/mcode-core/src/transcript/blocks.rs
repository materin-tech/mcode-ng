//! 块模型：CC content-block 的 Rust 对应物 + AgentEvent 驱动的状态迁移。
//!
//! 每个块要么"已完成"（可冻结进 scrollback），要么"进行中"（留在动态区）。
//! core 事件缺口（无 ToolUseEnd / ToolResult 无 id / 无耗时）在 apply 里
//! 按 name+FIFO 对齐、用墙钟计时补，见各分支注释。

use std::collections::VecDeque;
use std::time::Instant;

use crate::agent::AgentEvent;
use crate::tools::DisplayBlock;

use super::style::{compact_tokens, format_duration};
use super::tools::{self, TodoItem};

/// 动态区流式四态（对齐 CC streamMode）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamMode {
    Idle,
    /// 等首包
    Requesting,
    /// 文本/thinking 流式
    Responding,
    /// 工具执行中
    ToolUse,
    /// 工具参数流式
    ToolInput,
}

/// 系统通知样式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeKind {
    Info,
    Success,
    Warning,
    Error,
}

/// 子代理内部行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InternalLine {
    Tool {
        name: String,
        summary: String,
        success: bool,
    },
    Thinking(String),
}

/// 子代理状态（task 工具块的扩展）。
#[derive(Debug)]
pub struct AgentState {
    pub scope: String,
    pub internal: Vec<InternalLine>,
    /// 打开中的内部工具（id → (name, args_json)），ToolResult 到达时闭合
    open_internal: Vec<(String, String, String)>,
    pub tool_uses: u32,
    pub est_tokens: u64,
    pub started_at: Instant,
    /// 后台 detached 块（scope 事件先于 task 工具到达时创建）
    pub detached: bool,
}

impl AgentState {
    fn new(scope: &str, detached: bool) -> Self {
        AgentState {
            scope: scope.to_string(),
            internal: Vec::new(),
            open_internal: Vec::new(),
            tool_uses: 0,
            est_tokens: 0,
            started_at: Instant::now(),
            detached,
        }
    }
}

#[derive(Debug)]
pub enum ToolStatus {
    /// ⏺ 闪烁（等待权限/执行）
    Pending,
    Resolved {
        success: bool,
        output: String,
        display: Option<DisplayBlock>,
    },
}

#[derive(Debug)]
pub struct ToolBlock {
    pub tool: String,
    pub id: String,
    pub args_json: String,
    pub status: ToolStatus,
    pub agent: Option<AgentState>,
}

impl ToolBlock {
    /// 子代理 Done 汇总行（CC：`Done (12 tool uses · 15.3k tokens · 1m 32s)`）。
    pub fn agent_done_summary(&self) -> String {
        let Some(a) = self.agent.as_ref() else {
            return String::new();
        };
        let secs = a.started_at.elapsed().as_secs();
        format!(
            "Done ({} tool uses · {} tokens · {})",
            a.tool_uses,
            compact_tokens(a.est_tokens),
            format_duration(secs)
        )
    }
}

// ToolBlock 携带 args_json/output 等长字符串；盒化要波及所有 match 点，CLI 无实测收益
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum Block {
    User { text: String },
    Assistant { text: String, streaming: bool },
    Thinking { text: String, streaming: bool },
    Tool(ToolBlock),
    Todo { items: Vec<TodoItem> },
    Notice { text: String, kind: NoticeKind },
    Interrupted,
}

impl Block {
    /// 是否已定稿（可冻结进 scrollback）。
    pub fn is_frozen(&self) -> bool {
        match self {
            Block::User { .. } | Block::Todo { .. } | Block::Notice { .. } | Block::Interrupted => {
                true
            }
            Block::Assistant { streaming, .. } | Block::Thinking { streaming, .. } => !streaming,
            Block::Tool(t) => matches!(t.status, ToolStatus::Resolved { .. }),
        }
    }
}

/// 流式渲染中打开的正文块索引（assistant 或 thinking 二选一）。
#[derive(Debug, Default)]
struct OpenStream {
    assistant: Option<usize>,
    thinking: Option<usize>,
}

/// UI 状态（纯逻辑；渲染与终端 IO 在 render/term 层）。
pub struct TranscriptState {
    pub blocks: Vec<Block>,
    open: OpenStream,
    pub model: String,
    pub busy: bool,
    pub stream_mode: StreamMode,
    pub turn_started: Option<Instant>,
    pub turn_tokens: u64,
    pub spinner_verb: &'static str,
    pub detail_view: bool,
    pub queue: VecDeque<(String, String)>,
    pub interrupt_requested: bool,
    pub submitted_once: bool,
}

/// shell 旁路用的 scope 标记（spawn_shell 发 scope=Some("shell")）。
pub const SHELL_SCOPE: &str = "shell";

impl TranscriptState {
    pub fn new(model: &str) -> Self {
        TranscriptState {
            blocks: Vec::new(),
            open: OpenStream::default(),
            model: model.to_string(),
            busy: false,
            stream_mode: StreamMode::Idle,
            turn_started: None,
            turn_tokens: 0,
            spinner_verb: super::spinner::sample_verb(),
            detail_view: false,
            queue: VecDeque::new(),
            interrupt_requested: false,
            submitted_once: false,
        }
    }

    // ---- 回合控制 ----

    pub fn begin_turn(&mut self) {
        self.busy = true;
        self.stream_mode = StreamMode::Requesting;
        self.turn_started = Some(Instant::now());
        self.turn_tokens = 0;
        self.interrupt_requested = false;
        self.spinner_verb = super::spinner::sample_verb();
    }

    /// Esc 中断：定稿流式块 + 打中断标记。
    pub fn interrupt_current(&mut self) {
        self.finalize_streams();
        if self.busy {
            self.blocks.push(Block::Interrupted);
        }
        self.interrupt_requested = true;
    }

    /// 回合收尾（Done/Error 前后调用）：全流式块定稿、状态复位。
    pub fn end_turn(&mut self) {
        self.finalize_streams();
        self.busy = false;
        self.stream_mode = StreamMode::Idle;
        self.turn_started = None;
    }

    fn finalize_streams(&mut self) {
        for i in [self.open.assistant, self.open.thinking]
            .into_iter()
            .flatten()
        {
            if let Some(Block::Assistant { streaming, .. } | Block::Thinking { streaming, .. }) =
                self.blocks.get_mut(i)
            {
                *streaming = false;
            }
        }
        self.open = OpenStream::default();
    }

    // ---- 块构造 ----

    pub fn push_user(&mut self, text: &str) {
        self.finalize_streams();
        self.blocks.push(Block::User {
            text: text.to_string(),
        });
        self.submitted_once = true;
    }

    pub fn push_notice(&mut self, text: impl Into<String>, kind: NoticeKind) {
        self.blocks.push(Block::Notice {
            text: text.into(),
            kind,
        });
    }

    fn assistant_block(&mut self) -> usize {
        if let Some(i) = self.open.assistant {
            return i;
        }
        self.blocks.push(Block::Assistant {
            text: String::new(),
            streaming: true,
        });
        let i = self.blocks.len() - 1;
        self.open.assistant = Some(i);
        i
    }

    fn thinking_block(&mut self) -> usize {
        if let Some(i) = self.open.thinking {
            return i;
        }
        self.blocks.push(Block::Thinking {
            text: String::new(),
            streaming: true,
        });
        let i = self.blocks.len() - 1;
        self.open.thinking = Some(i);
        i
    }

    fn open_tool_block(&mut self, id: &str, name: &str, scope: Option<&str>) {
        self.finalize_streams();
        let agent = match scope {
            None => None,
            Some(s) if s == SHELL_SCOPE => None,
            Some(s) => Some(AgentState::new(s, false)),
        };
        self.blocks.push(Block::Tool(ToolBlock {
            tool: name.to_string(),
            id: id.to_string(),
            args_json: String::new(),
            status: ToolStatus::Pending,
            agent,
        }));
    }

    /// 事件分派（唯一入口）。
    pub fn apply(&mut self, ev: AgentEvent) {
        match ev {
            AgentEvent::TextDelta(t) => {
                let i = self.assistant_block();
                if let Some(Block::Assistant { text, .. }) = self.blocks.get_mut(i) {
                    text.push_str(&t);
                }
                self.stream_mode = StreamMode::Responding;
            }
            AgentEvent::Thinking { text, scope } => match scope.as_deref() {
                None => {
                    let i = self.thinking_block();
                    if let Some(Block::Thinking { text: t, .. }) = self.blocks.get_mut(i) {
                        t.push_str(&text);
                    }
                    self.stream_mode = StreamMode::Responding;
                }
                Some(s) if s == SHELL_SCOPE => {}
                Some(s) => {
                    if let Some(a) = self.agent_for_scope_mut(s) {
                        a.est_tokens += text.len() as u64 / 4;
                        a.internal.push(InternalLine::Thinking(text));
                    }
                }
            },
            AgentEvent::ToolUseStart { id, name, scope } => match scope.as_deref() {
                Some(s) if s != SHELL_SCOPE => {
                    if let Some(a) = self.agent_for_scope_mut(s) {
                        a.open_internal.push((id, name, String::new()));
                    } else {
                        // 后台 agent：scope 事件先于任何 task 块 → detached 块
                        self.blocks.push(Block::Tool(ToolBlock {
                            tool: "task".into(),
                            id: String::new(),
                            args_json: String::new(),
                            status: ToolStatus::Pending,
                            agent: Some(AgentState::new(s, true)),
                        }));
                        if let Some(Block::Tool(t)) = self.blocks.last_mut() {
                            if let Some(a) = t.agent.as_mut() {
                                a.open_internal.push((id, name, String::new()));
                            }
                        }
                    }
                }
                _ => {
                    // 主循环工具（含 shell 旁路）
                    let effective = if scope.as_deref() == Some(SHELL_SCOPE) {
                        "bash"
                    } else {
                        name.as_str()
                    };
                    let id = if scope.as_deref() == Some(SHELL_SCOPE) {
                        format!("shell-{id}")
                    } else {
                        id
                    };
                    // task 工具预开 agent 槽（scope 未知 → 空 scope，首个 scoped 事件认领）
                    let agent_scope = if effective == "task" { Some("") } else { None };
                    self.open_tool_block(&id, effective, agent_scope);
                    self.stream_mode = StreamMode::ToolUse;
                }
            },
            AgentEvent::ToolInputDelta {
                id,
                partial_json,
                scope,
            } => match scope.as_deref() {
                Some(s) if s != SHELL_SCOPE => {
                    if let Some(a) = self.agent_for_scope_mut(s) {
                        if let Some((_, _, args)) =
                            a.open_internal.iter_mut().find(|(i, _, _)| *i == id)
                        {
                            args.push_str(&partial_json);
                        }
                    }
                }
                _ => {
                    if let Some(Block::Tool(t)) = self
                            .blocks
                            .iter_mut()
                            .rev()
                            .find(|b| matches!(b, Block::Tool(t) if t.id == id && matches!(t.status, ToolStatus::Pending)))
                        {
                            t.args_json.push_str(&partial_json);
                        }
                    self.stream_mode = StreamMode::ToolInput;
                }
            },
            AgentEvent::ToolResult {
                name,
                success,
                output,
                display,
                scope,
            } => {
                let out = output;
                match scope.as_deref() {
                    Some(s) if s != SHELL_SCOPE => {
                        if let Some(a) = self.agent_for_scope_mut(s) {
                            a.est_tokens += out.len() as u64 / 4;
                            let idx = a.open_internal.iter().rposition(|(_, n, _)| *n == name);
                            let args = idx.map(|i| a.open_internal.remove(i).2).unwrap_or_default();
                            a.tool_uses += 1;
                            a.internal.push(InternalLine::Tool {
                                summary: internal_summary(&name, &args),
                                name: name.clone(),
                                success,
                            });
                        }
                    }
                    _ => {
                        // 主工具：按 name 取最后一个 Pending 块（串行执行，FIFO 安全）
                        let idx = self.blocks.iter().rposition(|b| {
                            matches!(
                                b,
                                Block::Tool(t)
                                    if t.tool == name
                                        && matches!(t.status, ToolStatus::Pending)
                            )
                        });
                        let effective_name = name.clone();
                        if let Some(i) = idx {
                            // 先落定块（借用即释放）；todo 清单随后独立入列
                            let todo_items = if let Some(Block::Tool(t)) = self.blocks.get_mut(i) {
                                let args = std::mem::take(&mut t.args_json);
                                if t.tool == "task" {
                                    if let Some(a) = t.agent.as_mut() {
                                        a.detached = false;
                                    }
                                }
                                let items = if t.tool == "todo_write" {
                                    let items = tools::parse_todos(&args);
                                    (!items.is_empty()).then_some(items)
                                } else {
                                    None
                                };
                                t.args_json = args;
                                t.status = ToolStatus::Resolved {
                                    success,
                                    output: out,
                                    display,
                                };
                                items
                            } else {
                                None
                            };
                            if let Some(items) = todo_items {
                                self.blocks.push(Block::Todo { items });
                            }
                        } else {
                            // 无配对 start（如 resume 场景）：直接落定稿块
                            if effective_name == "todo_write" {
                                let items = tools::parse_todos("");
                                let _ = items;
                            }
                            self.blocks.push(Block::Tool(ToolBlock {
                                tool: effective_name,
                                id: String::new(),
                                args_json: String::new(),
                                status: ToolStatus::Resolved {
                                    success,
                                    output: out,
                                    display,
                                },
                                agent: None,
                            }));
                        }
                        self.stream_mode = StreamMode::ToolUse;
                    }
                }
            }
            AgentEvent::Usage(u) => {
                self.turn_tokens += u.output_tokens;
            }
            AgentEvent::Retrying {
                attempt,
                max,
                reason,
            } => {
                self.push_notice(
                    format!("请求失败（{reason}），第 {attempt}/{max} 次重试…"),
                    NoticeKind::Warning,
                );
            }
            AgentEvent::Recovering { action } => {
                self.push_notice(format!("正在恢复：{action}"), NoticeKind::Warning);
            }
            AgentEvent::Compacted { note } => {
                self.push_notice(format!("上下文已压缩：{note}"), NoticeKind::Info);
            }
            AgentEvent::HookNotice { message } => {
                self.push_notice(message, NoticeKind::Info);
            }
            AgentEvent::Confirm { .. } => {
                // 权限对话框走 confirm_rx 通道（mod.rs），事件仅作占位
            }
            AgentEvent::Error { message } => {
                self.push_notice(message, NoticeKind::Error);
                self.finalize_streams();
            }
            AgentEvent::Done { reply } => {
                // 已流式渲染过正文则忽略 reply，否则落一条定稿正文
                let has_streamed = self
                    .open
                    .assistant
                    .is_some_and(|i| matches!(self.blocks.get(i), Some(Block::Assistant { text, .. }) if !text.is_empty()));
                if !has_streamed && !reply.trim().is_empty() {
                    self.finalize_streams();
                    self.blocks.push(Block::Assistant {
                        text: reply,
                        streaming: false,
                    });
                }
                self.end_turn();
            }
        }
    }

    /// 找最后一个打开中的 agent 块（scope 归属：task 工具块开闭 + scope 名）。
    fn agent_for_scope_mut(&mut self, scope: &str) -> Option<&mut AgentState> {
        // 先找已挂该 scope 的 agent；未命中则认领 scope 为空的 pending task 块
        let exact = self.blocks.iter().rposition(|b| {
            matches!(
                b,
                Block::Tool(t)
                    if matches!(t.status, ToolStatus::Pending)
                        && t.agent.as_ref().is_some_and(|a| a.scope == scope)
            )
        });
        let idx = match exact {
            Some(i) => Some(i),
            None => self.blocks.iter().rposition(|b| {
                matches!(
                    b,
                    Block::Tool(t)
                        if matches!(t.status, ToolStatus::Pending)
                            && t.tool == "task"
                            && t.agent.as_ref().is_some_and(|a| a.scope.is_empty())
                )
            }),
        };
        let idx = idx?;
        match self.blocks.get_mut(idx) {
            Some(Block::Tool(t)) => {
                let a = t.agent.as_mut()?;
                if a.scope.is_empty() {
                    a.scope = scope.to_string();
                }
                Some(a)
            }
            _ => None,
        }
    }

    /// 冻结边界：blocks[..n] 已定稿（可打印进 scrollback）。
    pub fn frozen_boundary(&self) -> usize {
        self.blocks
            .iter()
            .position(|b| !b.is_frozen())
            .unwrap_or(self.blocks.len())
    }

    /// 取走动态区的进行中块（渲染用；所有权仍留在 state，渲染走引用即可）。
    pub fn dynamic_blocks(&self) -> &[Block] {
        &self.blocks[self.frozen_boundary()..]
    }
}

/// 子代理内部工具 → CC 过去式摘要行。
fn internal_summary(tool: &str, args_json: &str) -> String {
    let summary = tools::tool_args_summary(tool, args_json);
    match tool {
        "bash" => format!("Ran `{}`", truncate_1line(&summary, 80)),
        "read_file" => format!("Read {}", summary),
        "write_file" => format!("Wrote {}", summary),
        "edit_file" => format!("Updated {}", summary),
        "grep" | "glob" => format!("Searched {}", summary),
        "web_fetch" => format!("Fetched {}", summary),
        _ => format!("{} {}", tools::tool_display_name(tool), summary),
    }
}

fn truncate_1line(s: &str, max: usize) -> String {
    let first = s.lines().next().unwrap_or("");
    super::style::truncate_chars(first, max)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev_text(t: &str) -> AgentEvent {
        AgentEvent::TextDelta(t.to_string())
    }

    fn start(id: &str, name: &str) -> AgentEvent {
        AgentEvent::ToolUseStart {
            id: id.into(),
            name: name.into(),
            scope: None,
        }
    }

    fn delta(id: &str, s: &str) -> AgentEvent {
        AgentEvent::ToolInputDelta {
            id: id.into(),
            partial_json: s.into(),
            scope: None,
        }
    }

    fn result(name: &str, ok: bool, out: &str) -> AgentEvent {
        AgentEvent::ToolResult {
            name: name.into(),
            success: ok,
            output: out.into(),
            display: None,
            scope: None,
        }
    }

    fn state() -> TranscriptState {
        let mut s = TranscriptState::new("test-model");
        s.begin_turn();
        s
    }

    #[test]
    fn text_deltas_stream_into_assistant_and_freeze_on_done() {
        let mut s = state();
        s.apply(ev_text("你"));
        s.apply(ev_text("好"));
        assert_eq!(s.frozen_boundary(), 0, "流式块不许冻结");
        assert_eq!(s.stream_mode, StreamMode::Responding);
        let reply = "你好！".to_string();
        s.apply(AgentEvent::Done { reply });
        assert_eq!(s.frozen_boundary(), 1, "Done 后定稿");
        assert!(!s.busy);
        assert_eq!(s.stream_mode, StreamMode::Idle);
        match &s.blocks[0] {
            Block::Assistant { text, streaming } => {
                assert_eq!(text, "你好");
                assert!(!streaming);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn done_reply_used_when_no_streamed_text() {
        let mut s = state();
        s.apply(AgentEvent::Done {
            reply: "最终回复".into(),
        });
        match &s.blocks[0] {
            Block::Assistant { text, .. } => assert_eq!(text, "最终回复"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn tool_lifecycle_pending_to_resolved() {
        let mut s = state();
        s.apply(start("t1", "bash"));
        s.apply(delta("t1", r#"{"command":"cargo tes"#));
        s.apply(delta("t1", r#"t"}"#));
        assert_eq!(s.stream_mode, StreamMode::ToolInput);
        match &s.blocks[0] {
            Block::Tool(t) => {
                assert!(matches!(t.status, ToolStatus::Pending));
                assert_eq!(t.args_json, r#"{"command":"cargo test"}"#);
            }
            other => panic!("{other:?}"),
        }
        s.apply(result("bash", true, ""));
        match &s.blocks[0] {
            Block::Tool(t) => {
                assert!(matches!(
                    t.status,
                    ToolStatus::Resolved { success: true, .. }
                ));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(s.frozen_boundary(), 1);
    }

    #[test]
    fn orphan_result_becomes_resolved_block() {
        let mut s = state();
        s.apply(result("bash", true, "out"));
        assert_eq!(s.blocks.len(), 1);
        assert!(s.blocks[0].is_frozen());
    }

    #[test]
    fn task_block_routes_scoped_events_and_reports_done_summary() {
        let mut s = state();
        s.apply(start("t1", "task"));
        s.apply(delta("t1", r#"{"description":"搜索调用点"}"#));
        // 子代理内部事件（scope 化）
        s.apply(AgentEvent::ToolUseStart {
            id: "i1".into(),
            name: "grep".into(),
            scope: Some("explore".into()),
        });
        s.apply(AgentEvent::ToolInputDelta {
            id: "i1".into(),
            partial_json: r#"{"pattern":"foo"}"#.into(),
            scope: Some("explore".into()),
        });
        s.apply(AgentEvent::ToolResult {
            name: "grep".into(),
            success: true,
            output: "a.rs:1".into(),
            display: None,
            scope: Some("explore".into()),
        });
        s.apply(AgentEvent::Thinking {
            text: "分析中".into(),
            scope: Some("explore".into()),
        });
        match &s.blocks[0] {
            Block::Tool(t) => {
                let a = t.agent.as_ref().unwrap();
                assert_eq!(a.scope, "explore");
                assert_eq!(a.tool_uses, 1);
                assert_eq!(
                    a.internal,
                    vec![
                        InternalLine::Tool {
                            name: "grep".into(),
                            summary: "Searched pattern: \"foo\"".into(),
                            success: true
                        },
                        InternalLine::Thinking("分析中".into())
                    ]
                );
                assert!(a.est_tokens > 0, "内部内容计估算 tokens");
            }
            other => panic!("{other:?}"),
        }
        // task 工具结果 → agent 块定稿 + Done 汇总
        s.apply(result("task", true, "报告内容"));
        match &s.blocks[0] {
            Block::Tool(t) => {
                let summary = t.agent_done_summary();
                assert!(summary.starts_with("Done (1 tool uses · "), "{summary}");
                assert!(summary.ends_with(')'), "{summary}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn shell_scope_is_main_tool_not_agent() {
        let mut s = state();
        s.apply(AgentEvent::ToolUseStart {
            id: "s1".into(),
            name: "bash".into(),
            scope: Some(SHELL_SCOPE.into()),
        });
        s.apply(AgentEvent::ToolResult {
            name: "bash".into(),
            success: true,
            output: String::new(),
            display: None,
            scope: Some(SHELL_SCOPE.into()),
        });
        match &s.blocks[0] {
            Block::Tool(t) => {
                assert!(t.agent.is_none(), "shell 旁路不是子代理");
                assert_eq!(t.tool, "bash");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn background_scope_without_open_agent_creates_detached_block() {
        let mut s = state();
        s.apply(AgentEvent::ToolUseStart {
            id: "b1".into(),
            name: "grep".into(),
            scope: Some("agent_1".into()),
        });
        assert_eq!(s.blocks.len(), 1);
        match &s.blocks[0] {
            Block::Tool(t) => {
                let a = t.agent.as_ref().unwrap();
                assert!(a.detached);
                assert_eq!(a.scope, "agent_1");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn interrupt_finalizes_and_appends_marker() {
        let mut s = state();
        s.apply(ev_text("写到一半"));
        s.interrupt_current();
        assert_eq!(s.blocks.len(), 2);
        assert!(matches!(
            s.blocks[0],
            Block::Assistant {
                streaming: false,
                ..
            }
        ));
        assert!(matches!(s.blocks[1], Block::Interrupted));
        assert_eq!(s.frozen_boundary(), 2);
    }

    #[test]
    fn usage_accumulates_into_turn_tokens() {
        use crate::message::Usage;
        let mut s = state();
        s.apply(AgentEvent::Usage(Usage {
            input_tokens: 100,
            output_tokens: 50,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        }));
        s.apply(AgentEvent::Usage(Usage {
            input_tokens: 100,
            output_tokens: 70,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        }));
        assert_eq!(s.turn_tokens, 120);
    }

    #[test]
    fn todo_result_expands_into_todo_block() {
        let mut s = state();
        s.apply(start("t1", "todo_write"));
        let args = serde_json::json!({ "todos": [
            {"content":"a","status":"completed","active_form":"a"},
            {"content":"b","status":"in_progress","active_form":"b"}
        ]})
        .to_string();
        s.apply(delta("t1", &args));
        s.apply(result("todo_write", true, "ok"));
        assert!(
            matches!(&s.blocks[1], Block::Todo { items } if items.len() == 2),
            "todo 结果展开为清单块"
        );
    }

    #[test]
    fn retry_and_error_notices() {
        let mut s = state();
        s.apply(AgentEvent::Retrying {
            attempt: 2,
            max: 5,
            reason: "502".into(),
        });
        assert!(matches!(
            &s.blocks[0],
            Block::Notice {
                kind: NoticeKind::Warning,
                ..
            }
        ));
        s.apply(AgentEvent::Error {
            message: "boom".into(),
        });
        assert!(matches!(
            &s.blocks[1],
            Block::Notice {
                kind: NoticeKind::Error,
                ..
            }
        ));
    }
}
