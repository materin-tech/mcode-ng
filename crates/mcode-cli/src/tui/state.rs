//! TUI 状态机：输入缓冲、消息队列、模式循环、transcript 行、历史。
//!
//! 与渲染解耦——纯状态转换全部在这里单测；render 只读不写。

use mcode_core::agent::AgentEvent;
use std::collections::VecDeque;

/// transcript 一行。`detail` 存完整内容（ctrl+o 详情视图渲染；缺省 = text）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub kind: LineKind,
    pub text: String,
    pub detail: Option<String>,
}

impl Line {
    fn new(kind: LineKind, text: impl Into<String>) -> Self {
        Line {
            kind,
            text: text.into(),
            detail: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    /// 用户输入
    User,
    /// 助手文本
    Assistant,
    /// 工具活动（开始/结果摘要）
    Tool,
    /// 思考流（详情视图才显示）
    Thinking,
    /// 系统提示/错误/恢复动作
    System,
}

/// 提交输入后的分派动作。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// 发起回合（prompt 已按 skill 展开完毕）。
    Send(String),
    /// `!cmd`：绕过模型直接跑 shell。
    Bash(String),
    /// slash 命令已本地处理。
    Local(&'static str),
}

/// 一个回合检查点（rewind 目标）：回合入口的 user 消息 + 其 parent。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkpoint {
    pub id: String,
    /// 该 user 消息的 parent（rewind_before 目标）；None = 会话首条。
    pub parent: Option<String>,
    pub prompt: String,
    /// transcript 中该回合入口行的下标（rewind 落定后截断用）。
    pub line_index: usize,
}

/// 权限确认弹窗（显示态；oneshot 应答端在 run 循环里配对）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmPrompt {
    pub tool: String,
    pub args_summary: String,
}

/// @ 文件补全弹窗。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    /// '@' 在 input 中的字节下标（accept 时从这里替换）。
    pub at: usize,
    /// 候选（工作区相对路径，已按片段过滤，封顶 20 条）。
    pub candidates: Vec<String>,
    pub selected: usize,
}

/// 候选弹窗最多显示条数。
const MAX_COMPLETION_CANDIDATES: usize = 20;

/// 累计 token（Usage 事件）。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TokenCounter {
    pub input: u64,
    pub output: u64,
}

impl TokenCounter {
    /// 状态栏紧凑格式（1.2k / 86k）。
    pub fn render(&self) -> String {
        format!("↑{} ↓{}", compact(self.input), compact(self.output))
    }
}

/// token 数紧凑显示。
fn compact(n: u64) -> String {
    if n >= 1000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

/// 交互态状态机。
#[derive(Debug, Default)]
pub struct AppState {
    pub input: String,
    /// 当前模型（/model <name> 运行中切换；回合经 RunOpts.model 下发）。
    pub model: String,
    /// 权限模式（shift+tab 循环；与 PermissionManager 同步）。
    pub mode: mcode_core::permission::Mode,
    /// 回合进行中。
    pub busy: bool,
    /// busy 期间排队输入（回合结束自动发送）。
    pub queue: VecDeque<String>,
    pub lines: Vec<Line>,
    /// 已发送输入（↑ 回溯）。
    pub history: Vec<String>,
    pub history_pos: Option<usize>,
    /// 状态栏文本（busy 时为活动描述）。
    pub status: String,
    /// transcript 滚动偏移（None = 贴底自动滚动）。
    pub scroll_from_bottom: Option<u16>,
    pub should_quit: bool,
    /// 累计 token（Usage 事件；状态栏展示）。
    pub tokens: TokenCounter,
    /// 回合检查点（rewind 目标列表，时间序）。
    pub checkpoints: Vec<Checkpoint>,
    /// 权限确认弹窗（Some = 弹窗中，按键全被拦截）。
    pub confirm: Option<ConfirmPrompt>,
    /// rewind 列表（Some = 打开，值 = 当前选中下标，最新在前）。
    pub rewind: Option<usize>,
    /// ctrl+o 详情视图（工具完整输出 + 思考流）。
    pub show_details: bool,
    /// @ 文件补全弹窗（Some = 打开，按键被部分拦截）。
    pub completion: Option<Completion>,
    /// 工作区文件清单（@ 补全数据源；启动时由 run 循环填充）。
    pub workspace_files: Vec<String>,
}

impl AppState {
    pub fn push_line(&mut self, kind: LineKind, text: impl Into<String>) {
        let text = text.into();
        // 空行不占 transcript
        if text.trim().is_empty() {
            return;
        }
        self.lines.push(Line::new(kind, text));
        self.scroll_from_bottom = None; // 新内容 → 贴底
    }

    /// 带详情的行（ctrl+o 展开显示 detail）。
    fn push_line_with_detail(
        &mut self,
        kind: LineKind,
        text: impl Into<String>,
        detail: impl Into<String>,
    ) {
        let text = text.into();
        if text.trim().is_empty() {
            return;
        }
        self.lines.push(Line {
            kind,
            text,
            detail: Some(detail.into()),
        });
        self.scroll_from_bottom = None;
    }

    /// 回合事件 → transcript 行（Done 不产生行，由 run 循环收尾）。
    pub fn apply_event(&mut self, e: &AgentEvent) {
        match e {
            AgentEvent::TextDelta(t) => self.append_assistant_text(t),
            AgentEvent::ToolInputDelta { .. } => {
                // 流式参数不进 transcript（工具开始/结果已覆盖可见性）
            }
            AgentEvent::ToolUseStart { name, scope, .. } => {
                self.push_line(
                    LineKind::Tool,
                    match scope {
                        Some(s) => format!("→ [{s}] {name}"),
                        None => format!("→ {name}"),
                    },
                );
                self.status = format!("运行 {name}…");
            }
            AgentEvent::ToolResult {
                name,
                success,
                output,
                scope,
            } => {
                let preview: String = output.trim().chars().take(300).collect();
                let ell = if output.trim().chars().count() > 300 {
                    "…"
                } else {
                    ""
                };
                let head = match (success, scope) {
                    (true, Some(s)) => format!("✓ [{s}] {name} "),
                    (true, None) => format!("✓ {name} "),
                    (false, Some(s)) => format!("✗ [{s}] {name} "),
                    (false, None) => format!("✗ {name} "),
                };
                self.push_line_with_detail(
                    LineKind::Tool,
                    format!("{head}{preview}{ell}"),
                    format!("{head}\n{}", output.trim()),
                );
            }
            AgentEvent::Thinking { text, scope } => {
                self.append_thinking_text(text, scope.as_deref());
            }
            AgentEvent::Retrying {
                attempt,
                max,
                reason,
            } => {
                self.push_line(LineKind::System, format!("重试 {attempt}/{max}：{reason}"));
            }
            AgentEvent::Recovering { action } => {
                self.push_line(LineKind::System, format!("恢复：{action}"));
            }
            AgentEvent::Compacted { note } => {
                self.push_line(LineKind::System, format!("上下文已压缩：{note}"));
            }
            AgentEvent::HookNotice { message } => {
                self.push_line(LineKind::System, format!("hook：{message}"));
            }
            AgentEvent::Error { message } => {
                self.push_line(LineKind::System, format!("错误：{message}"));
            }
            AgentEvent::Usage(u) => {
                self.tokens.input += u.input_tokens;
                self.tokens.output += u.output_tokens;
            }
            AgentEvent::Confirm { .. } | AgentEvent::Done { .. } => {}
        }
    }

    /// 助手文本增量：与最后一行（同为 Assistant 且无工具插入）流式拼接。
    fn append_assistant_text(&mut self, t: &str) {
        if t.is_empty() {
            return;
        }
        if let Some(last) = self.lines.last_mut() {
            if last.kind == LineKind::Assistant {
                last.text.push_str(t);
                return;
            }
        }
        self.push_line(LineKind::Assistant, t);
    }

    /// 思考流增量：拼接进最后一个 Thinking 行（同 scope）；默认视图隐藏。
    fn append_thinking_text(&mut self, t: &str, scope: Option<&str>) {
        if t.is_empty() {
            return;
        }
        if let Some(last) = self.lines.last_mut() {
            if last.kind == LineKind::Thinking {
                last.text.push_str(t);
                last.detail = Some(last.text.clone());
                return;
            }
        }
        let head = match scope {
            Some(s) => format!("思考 [{s}] "),
            None => "思考 ".to_string(),
        };
        self.push_line_with_detail(
            LineKind::Thinking,
            format!("{head}{}", t.trim().chars().take(80).collect::<String>()),
            format!("{head}{t}"),
        );
    }

    /// shift+tab：权限模式循环。
    pub fn cycle_mode(&mut self) -> mcode_core::permission::Mode {
        self.mode = self.mode.next();
        self.push_line(
            LineKind::System,
            format!("权限模式 → {}", self.mode.label()),
        );
        self.mode
    }

    /// 提交输入：空串忽略；/开头走 slash 分派；其余 Send。
    /// busy 时入队（返回 Send 也照常，由 run 循环决定入队）。
    pub fn submit(&mut self) -> Option<Action> {
        let text = self.take_input();
        let text = text.trim().to_string();
        if text.is_empty() {
            return None;
        }
        self.history.push(text.clone());
        self.history_pos = None;
        if let Some(cmd) = text.strip_prefix('!') {
            return Some(Action::Bash(cmd.trim().to_string()));
        }
        if let Some(rest) = text.strip_prefix('/') {
            let (name, args) = match rest.split_once(' ') {
                Some((n, a)) => (n, a.trim()),
                None => (rest, ""),
            };
            return Some(self.dispatch_slash(name, args));
        }
        Some(Action::Send(text))
    }

    /// slash 命令分派。内置命令本地处理；已知技能 → 展开为 Send。
    fn dispatch_slash(&mut self, name: &str, args: &str) -> Action {
        match name {
            "help" => {
                self.push_line(
                    LineKind::System,
                    "命令：/help /clear /context /model /mode /quit  ·  shift+tab 切权限模式  ·  \
                     @文件 补全  ·  esc 打断 / 双 esc 回退  ·  ctrl+o 详情  ·  !cmd 跑 shell  ·  \
                     ctrl+c 退出",
                );
                Action::Local("help")
            }
            "clear" => {
                self.lines.clear();
                self.checkpoints.clear();
                Action::Local("clear")
            }
            "context" => Action::Local("context"),
            "mode" => {
                let m = self.cycle_mode();
                let _ = m;
                Action::Local("mode")
            }
            "model" => {
                if args.is_empty() {
                    let cur = if self.model.is_empty() {
                        "（未设置）"
                    } else {
                        &self.model
                    };
                    self.push_line(
                        LineKind::System,
                        format!("当前模型：{cur}（切换：/model <name>）"),
                    );
                } else {
                    self.model = args.to_string();
                    self.push_line(
                        LineKind::System,
                        format!("模型已切换 → {args}（下一回合生效）"),
                    );
                }
                Action::Local("model")
            }
            "quit" | "exit" => {
                self.should_quit = true;
                Action::Local("quit")
            }
            other => {
                // 技能分派交由 run 循环查 SkillRegistry（state 不持引用）。
                // 先占一行提示，Send 载荷 = "/name args" 原文。
                let _ = other;
                Action::Send(format!("/{name} {args}").trim_end().to_string())
            }
        }
    }

    /// 取走输入缓冲（清空 + 光标归零语义由 input 单字符串承载）。
    fn take_input(&mut self) -> String {
        std::mem::take(&mut self.input)
    }

    /// ↑/↓ 历史回溯。
    pub fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let pos = match self.history_pos {
            None => self.history.len() - 1,
            Some(p) => p.saturating_sub(1),
        };
        self.history_pos = Some(pos);
        self.input = self.history[pos].clone();
    }

    pub fn history_next(&mut self) {
        match self.history_pos {
            None => {}
            Some(p) => {
                if p + 1 >= self.history.len() {
                    self.history_pos = None;
                    self.input.clear();
                } else {
                    self.history_pos = Some(p + 1);
                    self.input = self.history[p + 1].clone();
                }
            }
        }
    }

    /// 回合收尾：出队下一条待发消息。
    pub fn finish_turn(&mut self) {
        self.busy = false;
        self.status.clear();
    }

    pub fn dequeue_next(&mut self) -> Option<String> {
        self.queue.pop_front()
    }

    /// 回合入口记录检查点（rewind 目标；由 run 循环在 push user 消息后调用）。
    pub fn record_checkpoint(&mut self, cp: Checkpoint) {
        self.checkpoints.push(cp);
    }

    /// 双 esc：打开 rewind 列表（最新在前）。空闲才允许。
    pub fn open_rewind(&mut self) -> bool {
        if self.busy || self.checkpoints.is_empty() {
            return false;
        }
        self.rewind = Some(0);
        true
    }

    /// rewind 列表上下移动（选中为最新在前下标）。
    pub fn rewind_move(&mut self, delta: i32) {
        if let Some(sel) = self.rewind.as_mut() {
            let len = self.checkpoints.len() as i32;
            *sel = (*sel as i32 + delta).clamp(0, len - 1) as usize;
        }
    }

    /// 当前选中检查点（转回时间序）。
    pub fn rewind_selected(&self) -> Option<Checkpoint> {
        let sel = self.rewind?;
        self.checkpoints
            .get(self.checkpoints.len() - 1 - sel)
            .cloned()
    }

    pub fn close_rewind(&mut self) {
        self.rewind = None;
    }

    /// rewind 落定后：截掉该回合入口行起的 transcript 与后续检查点。
    pub fn truncate_from(&mut self, line_index: usize) {
        self.lines.truncate(line_index);
        self.checkpoints.retain(|c| c.line_index < line_index);
    }

    /// 设置工作区文件清单（@ 补全数据源，启动时调用一次）。
    pub fn set_workspace_files(&mut self, files: Vec<String>) {
        self.workspace_files = files;
    }

    /// 输入了 '@'（已是 input 最后一字节）：打开补全并出候选。
    pub fn open_completion(&mut self) {
        if self.workspace_files.is_empty() {
            return;
        }
        self.completion = Some(Completion {
            at: self.input.len().saturating_sub(1),
            candidates: Vec::new(),
            selected: 0,
        });
        self.refresh_completion();
    }

    /// 输入变化后刷新候选：'@' 被删 / 片段含空格或新 '@' / 无匹配 → 关闭。
    pub fn refresh_completion(&mut self) {
        let Some(c) = self.completion.as_mut() else {
            return;
        };
        if !self
            .input
            .get(c.at..)
            .is_some_and(|rest| rest.starts_with('@'))
        {
            self.completion = None;
            return;
        }
        // '@' 是单字节 ASCII，at+1 必是字符边界。
        let fragment = self.input[c.at + 1..].to_lowercase();
        if fragment.contains(' ') || fragment.contains('@') {
            self.completion = None;
            return;
        }
        let matched: Vec<String> = self
            .workspace_files
            .iter()
            .filter(|p| p.to_lowercase().contains(&fragment))
            .take(MAX_COMPLETION_CANDIDATES)
            .cloned()
            .collect();
        if matched.is_empty() {
            self.completion = None;
        } else {
            c.candidates = matched;
            c.selected = 0;
        }
    }

    /// 候选上下移动（环绕）。
    pub fn completion_move(&mut self, delta: i32) {
        let Some(c) = self.completion.as_mut() else {
            return;
        };
        let len = c.candidates.len() as i32;
        if len == 0 {
            return;
        }
        c.selected = (c.selected as i32 + delta).rem_euclid(len) as usize;
    }

    /// 接受当前候选：`@frag` → `@path␣`。返回接受的路径。
    pub fn accept_completion(&mut self) -> Option<String> {
        let c = self.completion.take()?;
        let path = c.candidates.get(c.selected)?.clone();
        let head = self.input[..c.at].to_string();
        self.input = format!("{head}@{path} ");
        Some(path)
    }

    pub fn close_completion(&mut self) {
        self.completion = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn submit_dispatches_slash_and_plain() {
        let mut s = AppState {
            input: "你好".into(),
            ..Default::default()
        };
        assert_eq!(s.submit(), Some(Action::Send("你好".into())));

        s.input = "/help".into();
        assert_eq!(s.submit(), Some(Action::Local("help")));

        s.input = "   ".into();
        assert_eq!(s.submit(), None, "空白输入忽略");

        // 历史回溯
        s.input = "第二条".into();
        s.submit();
        s.history_prev();
        assert_eq!(s.input, "第二条");
        s.history_next();
        assert_eq!(s.input, "");
    }

    #[test]
    fn mode_cycles_through_all() {
        let mut s = AppState::default();
        assert_eq!(s.mode, mcode_core::permission::Mode::Passon);
        let mut seen = vec![s.mode];
        for _ in 0..3 {
            seen.push(s.cycle_mode());
        }
        assert_eq!(seen.len(), 4, "四种模式各出现一次");
        assert_eq!(
            s.cycle_mode(),
            mcode_core::permission::Mode::Passon,
            "循环回到起点"
        );
    }

    #[test]
    fn events_render_as_lines_and_text_streams_merge() {
        let mut s = AppState::default();
        s.apply_event(&AgentEvent::TextDelta("你".into()));
        s.apply_event(&AgentEvent::TextDelta("好".into()));
        s.apply_event(&AgentEvent::ToolUseStart {
            id: "1".into(),
            name: "bash".into(),
            scope: None,
        });
        s.apply_event(&AgentEvent::ToolResult {
            name: "bash".into(),
            success: true,
            output: "ok".into(),
            scope: None,
        });
        let kinds: Vec<LineKind> = s.lines.iter().map(|l| l.kind).collect();
        assert_eq!(
            kinds,
            vec![LineKind::Assistant, LineKind::Tool, LineKind::Tool]
        );
        assert_eq!(s.lines[0].text, "你好", "增量拼接进同一行");
        assert!(s.lines[1].text.contains("→ bash"));
        assert!(s.lines[2].text.starts_with("✓ bash"));

        // 子代理 scope 前缀
        s.apply_event(&AgentEvent::ToolResult {
            name: "read_file".into(),
            success: false,
            output: "not found".into(),
            scope: Some("explorer".into()),
        });
        assert!(s.lines[3].text.contains("[explorer]"));
        assert!(s.lines[3].text.starts_with("✗"));
    }

    #[test]
    fn queue_fifo_drain() {
        let mut s = AppState::default();
        s.queue.push_back("一".into());
        s.queue.push_back("二".into());
        s.busy = true;
        s.finish_turn();
        assert!(!s.busy);
        assert_eq!(s.dequeue_next().as_deref(), Some("一"));
        assert_eq!(s.dequeue_next().as_deref(), Some("二"));
        assert_eq!(s.dequeue_next(), None);
    }

    #[test]
    fn bash_and_context_dispatch() {
        let mut s = AppState {
            input: "!git status".into(),
            ..Default::default()
        };
        assert_eq!(s.submit(), Some(Action::Bash("git status".into())));

        s.input = "/context".into();
        assert_eq!(s.submit(), Some(Action::Local("context")));
    }

    #[test]
    fn model_switch_via_slash() {
        let mut s = AppState {
            model: "glm-5.3".into(),
            ..Default::default()
        };
        // 无参：只显示当前模型，不改
        s.input = "/model".into();
        assert_eq!(s.submit(), Some(Action::Local("model")));
        assert_eq!(s.model, "glm-5.3");
        // 带参：切换
        s.input = "/model deepseek-chat".into();
        s.submit();
        assert_eq!(s.model, "deepseek-chat");
    }

    #[test]
    fn usage_accumulates_and_compacts() {
        let mut s = AppState::default();
        s.apply_event(&AgentEvent::Usage(mcode_core::message::Usage {
            input_tokens: 1200,
            output_tokens: 30,
            ..Default::default()
        }));
        s.apply_event(&AgentEvent::Usage(mcode_core::message::Usage {
            input_tokens: 800,
            output_tokens: 70,
            ..Default::default()
        }));
        assert_eq!(s.tokens.input, 2000);
        assert_eq!(s.tokens.output, 100);
        assert_eq!(s.tokens.render(), "↑2.0k ↓100");
    }

    #[test]
    fn thinking_goes_to_hidden_line_with_detail() {
        let mut s = AppState::default();
        s.apply_event(&AgentEvent::Thinking {
            text: "先看目录".into(),
            scope: None,
        });
        s.apply_event(&AgentEvent::Thinking {
            text: "再读文件".into(),
            scope: None,
        });
        assert_eq!(s.lines.len(), 1, "增量拼进同一行");
        assert_eq!(s.lines[0].kind, LineKind::Thinking);
        assert_eq!(s.lines[0].detail.as_deref(), Some("思考 先看目录再读文件"));

        // 工具活动插入后，思考另起一行
        s.apply_event(&AgentEvent::ToolUseStart {
            id: "1".into(),
            name: "bash".into(),
            scope: None,
        });
        s.apply_event(&AgentEvent::Thinking {
            text: "下一步".into(),
            scope: Some("main".into()),
        });
        assert_eq!(s.lines.len(), 3);
        assert!(s.lines[2].detail.as_deref().unwrap().contains("[main]"));
    }

    #[test]
    fn rewind_modal_selection_and_truncation() {
        let mut s = AppState::default();
        s.push_line(LineKind::User, "第一问");
        s.record_checkpoint(Checkpoint {
            id: "m1".into(),
            parent: None,
            prompt: "第一问".into(),
            line_index: 0,
        });
        s.push_line(LineKind::User, "第二问");
        s.record_checkpoint(Checkpoint {
            id: "m2".into(),
            parent: Some("m1".into()),
            prompt: "第二问".into(),
            line_index: 1,
        });

        // busy 拦截
        let mut b = AppState {
            busy: true,
            ..Default::default()
        };
        b.record_checkpoint(Checkpoint {
            id: "x".into(),
            parent: None,
            prompt: "x".into(),
            line_index: 0,
        });
        assert!(!b.open_rewind());

        assert!(s.open_rewind());
        assert_eq!(s.rewind_selected().unwrap().id, "m2", "默认选中最新");
        s.rewind_move(1);
        assert_eq!(s.rewind_selected().unwrap().id, "m1");
        s.rewind_move(-5);
        assert_eq!(s.rewind_selected().unwrap().id, "m2", "最新端钳制");
        s.rewind_move(9);
        assert_eq!(s.rewind_selected().unwrap().id, "m1", "最旧端钳制");

        // 回到最新，选中 m2 回退：transcript 截到 line_index 1，检查点只剩 m1
        s.rewind_move(-9);
        let cp = s.rewind_selected().unwrap();
        assert_eq!(cp.id, "m2");
        s.close_rewind();
        s.truncate_from(cp.line_index);
        assert_eq!(s.lines.len(), 1);
        assert_eq!(s.checkpoints.len(), 1);
        assert_eq!(s.checkpoints[0].id, "m1");
    }

    fn completion_state() -> AppState {
        let mut s = AppState::default();
        // 生产路径 collect_workspace_files 有序，这里同序构造
        let mut files = vec![
            "src/main.rs".to_string(),
            "src/lib.rs".to_string(),
            "docs/guide.md".to_string(),
            "README.md".to_string(),
        ];
        files.sort();
        s.set_workspace_files(files);
        s
    }

    #[test]
    fn completion_opens_on_at_and_filters_by_fragment() {
        let mut s = completion_state();
        // 忠实按键序：先敲 '@' 打开，再敲片段刷新
        s.input = "@".into();
        s.open_completion();
        let c = s.completion.as_ref().expect("打开");
        assert_eq!(c.at, 0);

        s.input = "@ma".into();
        s.refresh_completion();
        assert_eq!(
            s.completion.as_ref().unwrap().candidates,
            vec!["src/main.rs".to_string()],
            "只中 main"
        );

        // 大小写不敏感
        s.input = "@READ".into();
        s.refresh_completion();
        assert_eq!(s.completion.as_ref().unwrap().candidates, vec!["README.md"]);

        // 无匹配 → 关闭
        s.input = "@zzz".into();
        s.refresh_completion();
        assert!(s.completion.is_none());

        // 空文件清单不开
        let mut e = AppState {
            input: "@".into(),
            ..Default::default()
        };
        e.open_completion();
        assert!(e.completion.is_none());
    }

    #[test]
    fn completion_refresh_closes_on_space_or_deleted_at() {
        let mut s = completion_state();
        s.input = "@".into();
        s.open_completion();
        assert!(s.completion.is_some());

        // 片段里出现空格 → 关闭
        s.input = "@sr ".into();
        s.refresh_completion();
        assert!(s.completion.is_none(), "片段含空格关闭");

        // '@' 被删光 → 关闭（at 处不再是 '@'）
        s.input = "@".into();
        s.open_completion();
        s.input = "sr".into();
        s.refresh_completion();
        assert!(s.completion.is_none(), "'@' 不在 at 处关闭");
    }

    #[test]
    fn completion_move_wraps_and_accept_appends_space() {
        let mut s = completion_state();
        s.input = "@".into();
        s.open_completion();
        let n = s.completion.as_ref().unwrap().candidates.len();
        assert_eq!(n, 4);

        s.completion_move(-1);
        assert_eq!(s.completion.as_ref().unwrap().selected, n - 1, "向上环绕");
        s.completion_move(1);
        assert_eq!(s.completion.as_ref().unwrap().selected, 0, "向下回绕");
        s.completion_move(2);
        assert_eq!(s.completion.as_ref().unwrap().selected, 2);

        // 排序后候选：README.md, docs/guide.md, src/lib.rs, src/main.rs → 下标 2
        let accepted = s.accept_completion().unwrap();
        assert_eq!(accepted, "src/lib.rs");
        assert!(s.completion.is_none(), "accept 消费弹窗");
        assert_eq!(s.input, "@src/lib.rs ", "替换并补尾随空格");

        // 输入中段的 '@'：替换只动 @frag，前缀保留
        let mut t = completion_state();
        t.input = "看下 @".into();
        t.open_completion();
        t.input = "看下 @gu".into();
        t.refresh_completion();
        let picked = t.accept_completion().unwrap();
        assert_eq!(picked, "docs/guide.md");
        assert_eq!(t.input, "看下 @docs/guide.md ");
        assert!(t.completion.is_none());
    }
}
