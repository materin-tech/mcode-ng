//! TUI 状态机：输入缓冲、消息队列、模式循环、transcript 行、历史。
//!
//! 与渲染解耦——纯状态转换全部在这里单测；render 只读不写。

use mcode_core::agent::AgentEvent;
use std::collections::VecDeque;
use std::path::PathBuf;

use super::complete::CompletionCandidate;

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

/// 补全弹窗（'@' 文件层级浏览 / '/' 命令共用骨架）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    /// 触发字符在 input 中的字节下标（accept 时从这里替换）。
    pub at: usize,
    /// 触发字符：'@' 文件补全 / '/' 命令补全。
    pub trigger: char,
    /// '@' 已确认的目录层（'/' 结尾；空 = 工作区根）。随下钻刷新。
    pub dir_part: String,
    /// 候选（已按当前层片段前缀过滤；封顶 20 条）。
    pub candidates: Vec<CompletionCandidate>,
    pub selected: usize,
    /// '@' 层级浏览的根（工作区 cwd；'/' 命令补全时为空路径）。
    pub root: PathBuf,
}

/// 候选弹窗最多显示条数。
const MAX_COMPLETION_CANDIDATES: usize = 20;

/// 弹窗一屏最多可视候选行（超出滚动跟随选中项）。
pub(crate) const MAX_COMPLETION_VISIBLE: usize = 8;

/// 内置斜杠命令（技能名由 run 层追加成完整命令表）。
pub const BUILTIN_SLASH_COMMANDS: &[&str] = &["help", "clear", "context", "mode", "model", "quit"];

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
    /// 输入光标（input 的字节偏移，恒在字符边界上；输入框可见光标由它驱动）。
    pub cursor: usize,
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
    /// 状态栏文本（busy 时为活动描述：思考中/运行 bash/回复中）。
    pub status: String,
    /// 当前回合起点（busy 态状态行计时/转圈用）。
    pub turn_started: Option<std::time::Instant>,
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
    pub workspace_root: PathBuf,
    /// 斜杠命令表（内置 + 技能名；启动时由 run 循环填充）。
    pub slash_commands: Vec<String>,
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
            AgentEvent::TextDelta(t) => {
                self.status = "回复中".into();
                self.append_assistant_text(t);
            }
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
                self.status = format!("运行 {name}");
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
                self.status = "思考中".into();
            }
            AgentEvent::Thinking { text, scope } => {
                self.status = "思考中".into();
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
        self.cursor = 0;
        std::mem::take(&mut self.input)
    }

    // -----------------------------------------------------------------------
    // 输入光标编辑（字节偏移 + 字符边界安全，CJK 不撕裂）
    // -----------------------------------------------------------------------

    /// 把光标收回到合法位置（≤ len 且落在字符边界上）。
    fn clamp_cursor(&mut self) {
        let mut c = self.cursor.min(self.input.len());
        while c > 0 && !self.input.is_char_boundary(c) {
            c -= 1;
        }
        self.cursor = c;
    }

    /// 整体替换输入（历史回溯/补全重写后调用），光标归末尾。
    fn set_input(&mut self, s: String) {
        self.cursor = s.len();
        self.input = s;
        self.clamp_cursor();
    }

    pub fn clear_input(&mut self) {
        self.input.clear();
        self.cursor = 0;
    }

    pub fn cursor_left(&mut self) {
        let mut c = self.cursor.saturating_sub(1);
        while c > 0 && !self.input.is_char_boundary(c) {
            c -= 1;
        }
        self.cursor = c;
    }

    pub fn cursor_right(&mut self) {
        let mut c = (self.cursor + 1).min(self.input.len());
        while c < self.input.len() && !self.input.is_char_boundary(c) {
            c += 1;
        }
        self.cursor = c;
    }

    pub fn cursor_home(&mut self) {
        self.cursor = 0;
    }

    pub fn cursor_end(&mut self) {
        self.cursor = self.input.len();
    }

    /// 光标处插入字符。
    pub fn insert_char(&mut self, ch: char) {
        let at = self.cursor;
        self.input.insert(at, ch);
        self.cursor = at + ch.len_utf8();
    }

    /// 退格：删光标前一字符。
    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.cursor_left();
        self.input.remove(self.cursor);
    }

    /// delete：删光标处字符。
    pub fn delete(&mut self) {
        if self.cursor < self.input.len() {
            self.input.remove(self.cursor);
            self.clamp_cursor();
        }
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
        self.set_input(self.history[pos].clone());
    }

    pub fn history_next(&mut self) {
        match self.history_pos {
            None => {}
            Some(p) => {
                if p + 1 >= self.history.len() {
                    self.history_pos = None;
                    self.clear_input();
                } else {
                    self.history_pos = Some(p + 1);
                    self.set_input(self.history[p + 1].clone());
                }
            }
        }
    }

    /// 回合收尾：出队下一条待发消息。
    pub fn finish_turn(&mut self) {
        self.busy = false;
        self.status.clear();
        self.turn_started = None;
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
    /// 设置补全浏览根（工作区 cwd，启动时调用一次）。
    pub fn set_workspace_root(&mut self, root: PathBuf) {
        self.workspace_root = root;
    }

    /// 设置斜杠命令表（内置 + 技能名，启动时调用一次）。
    pub fn set_slash_commands(&mut self, cmds: Vec<String>) {
        self.slash_commands = cmds;
    }

    /// 输入了 '@'（已是 input 最后一字节）：打开文件层级浏览（根目录层）。
    pub fn open_completion(&mut self) {
        if self.workspace_root.as_os_str().is_empty() {
            return;
        }
        self.completion = Some(Completion {
            at: self.input.len().saturating_sub(1),
            trigger: '@',
            dir_part: String::new(),
            candidates: Vec::new(),
            selected: 0,
            root: self.workspace_root.clone(),
        });
        self.refresh_completion();
    }

    /// 输入了 '/'（位于行首）：打开命令补全弹窗。
    pub fn open_slash_completion(&mut self) {
        if self.slash_commands.is_empty() || self.input != "/" {
            return;
        }
        self.completion = Some(Completion {
            at: 0,
            trigger: '/',
            dir_part: String::new(),
            candidates: Vec::new(),
            selected: 0,
            root: std::path::PathBuf::new(),
        });
        self.refresh_completion();
    }

    /// 输入变化后刷新候选：触发字符被删 / 片段含空格或再次出现触发类字符 /
    /// 无匹配 → 关闭。'@' 走层级浏览：片段最后一段是当前层过滤词，其余是
    /// 已确认目录前缀，每次实时 read_dir 一层（动态反映文件系统）。
    pub fn refresh_completion(&mut self) {
        let Some(c) = self.completion.as_mut() else {
            return;
        };
        if !self
            .input
            .get(c.at..)
            .is_some_and(|rest| rest.starts_with(c.trigger))
        {
            self.completion = None;
            return;
        }
        // 触发字符均为单字节 ASCII，at+1 必是字符边界。
        let fragment = self.input[c.at + 1..].to_lowercase();
        if fragment.contains(' ') || fragment.contains('@') {
            self.completion = None;
            return;
        }
        let matched: Vec<CompletionCandidate> = if c.trigger == '/' {
            // 命令名不含 '/'：片段出现 '/' 说明已进入路径式输入，关闭
            if fragment.contains('/') {
                self.completion = None;
                return;
            }
            self.slash_commands
                .iter()
                .filter(|n| n.starts_with(&fragment))
                .map(|n| CompletionCandidate {
                    insert: n.clone(),
                    display: n.clone(),
                    is_dir: false,
                })
                .collect()
        } else {
            // '@'：dir_part 原样用于路径解析，base 才大小写不敏感过滤
            let frag = &self.input[c.at + 1..];
            let (dir_part, base) = match frag.rfind('/') {
                Some(i) => (&frag[..=i], &frag[i + 1..]),
                None => ("", frag),
            };
            let base = base.to_lowercase();
            c.dir_part = dir_part.to_string();
            super::complete::list_dir(&c.root, dir_part)
                .into_iter()
                .filter(|e| e.display.to_lowercase().starts_with(&base))
                .take(MAX_COMPLETION_CANDIDATES)
                .map(|e| CompletionCandidate {
                    insert: format!("{dir_part}{}", e.insert),
                    display: e.display,
                    is_dir: e.is_dir,
                })
                .collect()
        };
        if matched.is_empty() {
            self.completion = None;
        } else {
            c.candidates = matched;
            c.selected = 0;
        }
    }

    /// 当前滚动窗口 (start, len)：候选超过一屏时窗口跟随选中项。
    pub fn completion_window(&self) -> (usize, usize) {
        let Some(c) = &self.completion else {
            return (0, 0);
        };
        let len = c.candidates.len().min(MAX_COMPLETION_VISIBLE);
        if len == 0 {
            return (0, 0);
        }
        let start = if c.selected + 1 > len {
            (c.selected + 1 - len).min(c.candidates.len() - len)
        } else {
            0
        };
        (start, len)
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

    /// 接受当前候选：文件 → `@完整路径␣` 收窗；目录 → `@目录/` **下钻**，
    /// 窗口保持打开并刷新为子层候选（斜杠命令等同文件收尾）。返回接受的候选。
    pub fn accept_completion(&mut self) -> Option<String> {
        let (at, trigger, cand) = {
            let c = self.completion.as_ref()?;
            (c.at, c.trigger, c.candidates.get(c.selected)?.clone())
        };
        let head = self.input[..at].to_string();
        if cand.is_dir {
            self.set_input(format!("{head}{trigger}{}", cand.insert));
            self.refresh_completion();
        } else {
            self.completion = None;
            self.set_input(format!("{head}{trigger}{} ", cand.insert));
        }
        Some(cand.insert)
    }

    pub fn close_completion(&mut self) {
        self.completion = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_editing_insert_move_delete() {
        let mut s = AppState::default();
        for ch in "ab".chars() {
            s.insert_char(ch);
        }
        assert_eq!(s.input, "ab");
        assert_eq!(s.cursor, 2);
        s.cursor_left();
        assert_eq!(s.cursor, 1, "光标落在字符边界");
        s.insert_char('X');
        assert_eq!(s.input, "aXb", "中间插入而非末尾");
        assert_eq!(s.cursor, 2);
        s.backspace();
        assert_eq!(s.input, "ab", "退格删光标前一字符");
        assert_eq!(s.cursor, 1);
        s.delete();
        assert_eq!(s.input, "a", "delete 删光标处字符");
        assert_eq!(s.cursor, 1);
    }

    #[test]
    fn cursor_editing_multibyte_safe() {
        let mut s = AppState::default();
        for ch in "中文".chars() {
            s.insert_char(ch);
        }
        assert_eq!(s.cursor, 6);
        s.cursor_left();
        assert_eq!(s.cursor, 3, "按字符移动，不落在 UTF-8 序列中间");
        s.backspace();
        assert_eq!(s.input, "文");
        assert_eq!(s.cursor, 0);
        s.cursor_left();
        assert_eq!(s.cursor, 0, "行首不再左移");
        s.delete();
        assert_eq!(s.input, "", "行首 delete 删唯一字符");
    }

    #[test]
    fn cursor_home_end_and_history_resets() {
        let mut s = AppState {
            history: vec!["旧输入".into()],
            ..Default::default()
        };
        s.insert_char('h');
        s.cursor_home();
        assert_eq!(s.cursor, 0);
        s.cursor_end();
        assert_eq!(s.cursor, 1);
        s.history_prev();
        assert_eq!(s.input, "旧输入");
        assert_eq!(s.cursor, 9, "换入历史后光标回末尾（3 汉字 × 3 字节）");
        s.cursor_home();
        s.history_next();
        assert_eq!(s.input, "");
        assert_eq!(s.cursor, 0);
    }

    #[test]
    fn accept_completion_puts_cursor_at_end() {
        let mut s = completion_state();
        // 忠实按键序：'@' 打开 → `s` 过滤出 src/ → 接受下钻 → `m` 过滤 main.rs → 接受
        s.input = "@".into();
        s.open_completion();
        s.input = "@s".into();
        s.refresh_completion();
        s.accept_completion();
        assert_eq!(s.input, "@src/", "目录候选下钻");
        s.input = "@src/m".into();
        s.refresh_completion();
        s.accept_completion();
        assert_eq!(s.input, "@src/main.rs ");
        assert_eq!(s.cursor, s.input.len(), "补全后光标落在新输入末尾");
    }

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
        s.set_workspace_root(make_file_tree());
        s
    }

    /// 真实临时目录树（list_dir 是实时 read_dir，不收假清单）：
    /// 根层 docs/ src/ README.md；src/ 下 lib.rs main.rs；docs/ 下 guide.md。
    fn make_file_tree() -> PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp =
            std::env::temp_dir().join(format!("mcode-tui-completion-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        for p in ["src/main.rs", "src/lib.rs", "docs/guide.md", "README.md"] {
            let f = tmp.join(p);
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, "x").unwrap();
        }
        tmp
    }

    fn displays(s: &AppState) -> Vec<String> {
        s.completion
            .as_ref()
            .unwrap()
            .candidates
            .iter()
            .map(|c| c.display.clone())
            .collect()
    }

    #[test]
    fn completion_opens_on_at_and_filters_by_fragment() {
        let mut s = completion_state();
        // 忠实按键序：先敲 '@' 打开，再敲片段刷新
        s.input = "@".into();
        s.open_completion();
        let c = s.completion.as_ref().expect("打开");
        assert_eq!(c.at, 0);
        // 根层：目录在前（带尾 /）、文件在后，各按名字排序
        assert_eq!(displays(&s), vec!["docs/", "src/", "README.md"]);

        // 当前层前缀过滤（层级浏览，不再全路径平铺）
        s.input = "@d".into();
        s.refresh_completion();
        assert_eq!(displays(&s), vec!["docs/"], "只中 docs/");

        // 大小写不敏感
        s.input = "@READ".into();
        s.refresh_completion();
        assert_eq!(displays(&s), vec!["README.md"]);

        // 目录前缀下钻：@src/ 列出子层
        s.input = "@src/".into();
        s.refresh_completion();
        assert_eq!(displays(&s), vec!["lib.rs", "main.rs"]);
        assert_eq!(
            s.completion.as_ref().unwrap().dir_part,
            "src/",
            "dir_part 跟随下钻"
        );
        s.input = "@src/m".into();
        s.refresh_completion();
        assert_eq!(displays(&s), vec!["main.rs"]);

        // 无匹配 → 关闭
        s.input = "@src/zzz".into();
        s.refresh_completion();
        assert!(s.completion.is_none());

        // 空工作区根不开
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
    fn accept_directory_drills_down_file_closes_window() {
        let mut s = completion_state();
        s.input = "@".into();
        s.open_completion();
        // 根层候选：docs/(0) src/(1) README.md(2)
        let n = s.completion.as_ref().unwrap().candidates.len();
        assert_eq!(n, 3);
        s.completion_move(-1);
        assert_eq!(s.completion.as_ref().unwrap().selected, n - 1, "向上环绕");
        s.completion_move(1);
        assert_eq!(s.completion.as_ref().unwrap().selected, 0, "向下回绕");

        // 选中 src/（下标 1）接受 → 下钻，窗口保持打开
        s.completion_move(1);
        let accepted = s.accept_completion().unwrap();
        assert_eq!(accepted, "src/");
        assert!(s.completion.is_some(), "目录接受 = 下钻，弹窗保持");
        assert_eq!(s.input, "@src/");
        assert_eq!(displays(&s), vec!["lib.rs", "main.rs"], "子层候选刷新");

        // 接受文件 → 收窗 + 完整路径 + 空格
        s.completion_move(1); // main.rs
        let accepted = s.accept_completion().unwrap();
        assert_eq!(accepted, "src/main.rs");
        assert!(s.completion.is_none(), "文件接受消费弹窗");
        assert_eq!(s.input, "@src/main.rs ", "替换并补尾随空格");

        // 输入中段的 '@'：目录前缀随下钻累积，替换只动 @frag
        let mut t = completion_state();
        t.input = "看下 @".into();
        t.open_completion();
        t.input = "看下 @d".into();
        t.refresh_completion();
        let picked = t.accept_completion().unwrap();
        assert_eq!(picked, "docs/");
        assert_eq!(t.input, "看下 @docs/");
        t.input = "看下 @docs/g".into();
        t.refresh_completion();
        let picked = t.accept_completion().unwrap();
        assert_eq!(picked, "docs/guide.md");
        assert_eq!(t.input, "看下 @docs/guide.md ");
        assert!(t.completion.is_none());
    }

    #[test]
    fn status_tracks_activity_and_clears_on_finish() {
        let mut s = AppState {
            busy: true,
            turn_started: Some(std::time::Instant::now()),
            ..Default::default()
        };

        s.apply_event(&AgentEvent::Thinking {
            text: "嗯".into(),
            scope: None,
        });
        assert_eq!(s.status, "思考中");

        s.apply_event(&AgentEvent::ToolUseStart {
            id: "t1".into(),
            name: "bash".into(),
            scope: None,
        });
        assert_eq!(s.status, "运行 bash");

        s.apply_event(&AgentEvent::ToolResult {
            name: "bash".into(),
            success: true,
            output: "ok".into(),
            scope: None,
        });
        assert_eq!(s.status, "思考中", "工具结束后回到思考态");

        s.apply_event(&AgentEvent::TextDelta("回".into()));
        assert_eq!(s.status, "回复中");

        s.finish_turn();
        assert_eq!(s.status, "");
        assert!(s.turn_started.is_none(), "收尾清计时起点");
    }

    #[test]
    fn completion_window_scrolls_to_keep_selection_visible() {
        let mut s = AppState::default();
        // 单层 30 个文件（实时列举的候选封顶 20）
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = std::env::temp_dir().join(format!("mcode-tui-completion-many-{n}"));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        for i in 0..30 {
            std::fs::write(tmp.join(format!("file{i:02}.txt")), "x").unwrap();
        }
        s.set_workspace_root(tmp.clone());
        s.input = "@".into();
        s.open_completion();
        assert_eq!(s.completion.as_ref().unwrap().candidates.len(), 20);
        assert_eq!(s.completion_window(), (0, 8), "开窗默认顶部");

        for _ in 0..10 {
            s.completion_move(1);
        }
        assert_eq!(s.completion.as_ref().unwrap().selected, 10);
        assert_eq!(s.completion_window(), (3, 8), "选中下移时窗口跟随");

        for _ in 0..9 {
            s.completion_move(1);
        }
        assert_eq!(s.completion.as_ref().unwrap().selected, 19, "移到末尾");
        assert_eq!(s.completion_window(), (12, 8), "选中到底部时窗口贴底钳制");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn slash_completion_lists_filters_and_accepts_commands() {
        let mut s = AppState::default();
        s.set_slash_commands(vec![
            "help".into(),
            "clear".into(),
            "context".into(),
            "mode".into(),
            "model".into(),
        ]);

        // 敲 '/' 打开 → 全量命令（保持插入序）
        s.input = "/".into();
        s.open_slash_completion();
        let c = s.completion.as_ref().expect("斜杠弹窗打开");
        assert_eq!(c.trigger, '/');
        assert_eq!(c.at, 0);
        assert_eq!(
            displays(&s),
            vec!["help", "clear", "context", "mode", "model"]
        );

        // 继续敲 "mo" → 前缀过滤
        s.input = "/mo".into();
        s.refresh_completion();
        assert_eq!(displays(&s), vec!["mode", "model"]);

        // 接受 → "/mode "，弹窗消费
        s.accept_completion();
        assert_eq!(s.input, "/mode ");
        assert!(s.completion.is_none());

        // 进参数（片段含空格）→ 关闭，不再干扰（真实流程：弹窗已开，逐键输入刷新）
        s.input = "/".into();
        s.open_slash_completion();
        assert!(s.completion.is_some());
        s.input = "/model g".into();
        s.refresh_completion();
        assert!(s.completion.is_none(), "片段含空格应关闭");

        // 无匹配 → 不开窗
        s.input = "/zzz".into();
        s.open_slash_completion();
        assert!(s.completion.is_none(), "无匹配候选不开窗");
    }
}
