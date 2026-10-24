//! 动态区整帧组装（queue、流式块、spinner、对话框、输入框、补全菜单
//! 和 footer）。单块渲染已上移 `mcode_core::transcript::render`，CLI 与
//! Desktop 共用；本模块只剩终端特有的整帧拼装与光标定位。

use super::blocks::TuiState;
use super::complete::CompletionState;
use super::confirm::ConfirmDialog;
use super::input::InputBuffer;
use super::spinner;
use super::style::{display_width, truncate_chars, wrap_line, Color, Span, StyledLine};

pub use mcode_core::transcript::render::render_frozen;
use mcode_core::transcript::render_block;

/// 动态区一帧（lines + 光标落点）。
pub struct DynamicFrame {
    pub lines: Vec<StyledLine>,
    /// (行号, 显示列)，行号从 lines 顶部数
    pub cursor: Option<(usize, usize)>,
}

/// 整帧组装所需的 UI 旁路状态（blocks 之外的一切）。
pub struct FrameCtx<'a> {
    pub state: &'a TuiState,
    pub input: &'a InputBuffer,
    pub completion: Option<&'a CompletionState>,
    pub confirm: Option<&'a ConfirmDialog>,
    pub bash_mode: bool,
    /// manual / auto / plan / passon
    pub mode: &'a str,
    /// 距 auto-compact 的百分比（None = 不显示）
    pub context_pct: Option<u64>,
    /// 双 esc 回退列表（Some = 打开：(条目文案, 选中下标，最新在前)）
    pub rewind: Option<(&'a [String], usize)>,
    pub width: usize,
    pub frame_tick: u64,
}

/// 动态区整帧。
pub fn render_frame(ctx: &FrameCtx) -> DynamicFrame {
    let width = ctx.width;
    let mut lines: Vec<StyledLine> = Vec::new();

    // 1. 队列通知（输入框上方）
    for (display, _) in ctx.state.queue.iter() {
        let mut line = StyledLine::default();
        line.push(Span::dim_text("  ⏸  "));
        line.push(Span::dim_text(truncate_chars(
            display,
            width.saturating_sub(10),
        )));
        lines.push(line);
    }

    // 2. 进度块（动态区里的未定稿块）
    let blink_on = (ctx.frame_tick / 10).is_multiple_of(2);
    for b in ctx.state.dynamic_blocks() {
        for l in render_block(b, ctx.state.detail_view, width, blink_on, false) {
            lines.push(l);
        }
        lines.push(StyledLine::blank());
    }
    while lines.last().is_some_and(StyledLine::is_blank) {
        lines.pop();
    }

    // 3. spinner
    if ctx.state.busy {
        let elapsed = ctx
            .state
            .turn_started
            .map(|t| t.elapsed().as_secs())
            .unwrap_or(0);
        lines.push(spinner::status_line(
            ctx.state.spinner_verb,
            spinner::frame_at(ctx.frame_tick),
            ctx.frame_tick,
            elapsed,
            ctx.state.turn_tokens,
            width,
            None,
        ));
    }

    // 4. 权限对话框
    if let Some(d) = ctx.confirm {
        lines.push(StyledLine::blank());
        for l in d.render() {
            lines.push(l);
        }
    }

    // 4.5 回退列表（双 esc 唤起）
    if let Some((items, sel)) = ctx.rewind {
        lines.push(StyledLine::blank());
        lines.push(StyledLine::new(vec![Span::colored(
            Color::Suggestion,
            "Rewind to:",
        )
        .bold()]));
        for (i, t) in items.iter().enumerate() {
            let mut line = StyledLine::default();
            if i == sel {
                line.push(Span::colored(Color::Suggestion, "❯ "));
                line.push(Span::colored(Color::Suggestion, t.clone()));
            } else {
                line.push(Span::new("  "));
                line.push(Span::new(t.clone()));
            }
            lines.push(line);
        }
        lines.push(StyledLine::new(vec![Span::dim_text(
            "↑↓ select · enter rewind · esc cancel",
        )]));
    }

    // 5. 补全菜单（输入框上方：composer 恒钉底，菜单向上生长——CC 同款）
    if let Some(c) = ctx.completion {
        let first = c.visible_first_offset();
        for (abs, cand) in c
            .candidates
            .iter()
            .enumerate()
            .skip(first)
            .take(c.visible().len())
        {
            let selected = abs == c.selected;
            let mut line = StyledLine::default();
            line.push(Span::new("  "));
            if selected {
                line.push(Span::colored(Color::Suggestion, "❯ "));
                line.push(Span::colored(Color::Suggestion, cand.display.clone()).bold());
            } else {
                line.push(Span::new("  "));
                line.push(Span::new(cand.display.clone()));
            }
            if !cand.description.is_empty() {
                line.push(Span::new("  "));
                line.push(Span::dim_text(truncate_chars(
                    &cand.description,
                    width
                        .saturating_sub(display_width(&cand.display) + 8)
                        .max(10),
                )));
            }
            lines.push(line);
        }
    }

    // 6. 输入框（上下边框 + prompt 行，闭合矩形——CC 的 composer 是全框）
    let border_color = if ctx.bash_mode {
        Color::BashBorder
    } else {
        Color::Dim
    };
    lines.push(StyledLine::new(vec![Span::colored(
        border_color,
        format!("╭{}╮", "─".repeat(width.saturating_sub(2))),
    )]));
    let input_start = lines.len();
    let prompt = if ctx.bash_mode {
        Span::colored(Color::BashBorder, "! ")
    } else {
        Span::colored(Color::Suggestion, "❯ ")
    };
    let body_width = width.saturating_sub(2);
    let placeholder = if ctx.input.is_empty() {
        Some(if ctx.state.submitted_once {
            "Press up to edit queued messages"
        } else {
            "Try: 帮我看看这个仓库的结构"
        })
    } else {
        None
    };
    let body_lines: Vec<StyledLine> = if let Some(ph) = placeholder {
        vec![StyledLine::new(vec![Span::dim_text(ph)])]
    } else {
        ctx.input
            .render_lines(body_width)
            .into_iter()
            .map(StyledLine::plain)
            .collect()
    };
    for (i, l) in body_lines.into_iter().enumerate() {
        let mut line = StyledLine::default();
        line.push(if i == 0 {
            prompt.clone()
        } else {
            Span::new("  ")
        });
        line.0.extend(l.0);
        lines.push(line);
    }
    lines.push(StyledLine::new(vec![Span::colored(
        border_color,
        format!("╰{}╯", "─".repeat(width.saturating_sub(2))),
    )]));
    let (input_row, cc) = ctx.input.cursor_row_col(body_width);
    let cursor_col = 2 + cc;

    // 7. footer
    let mut footer: Vec<StyledLine> = Vec::new();
    if ctx.state.detail_view {
        footer.push(StyledLine::new(vec![Span::dim_text(
            "Showing detailed transcript · ctrl+o to toggle",
        )]));
    }
    match ctx.mode {
        "plan" => footer.push(StyledLine::new(vec![Span::colored(
            Color::PlanMode,
            "⏸ plan mode on (shift+tab)",
        )])),
        "auto" => footer.push(StyledLine::new(vec![Span::colored(
            Color::AutoAccept,
            "⏵⵵ accept edits on (shift+tab)",
        )])),
        "passon" => footer.push(StyledLine::new(vec![Span::colored(
            Color::Error,
            "⏵⵵ bypass permissions on (shift+tab)",
        )])),
        _ => {}
    }
    if let Some(pct) = ctx.context_pct {
        footer.push(StyledLine::new(vec![Span::dim_text(format!(
            "{pct}% until auto-compact"
        ))]));
    }
    if footer.is_empty() && !ctx.state.busy && ctx.confirm.is_none() {
        footer.push(StyledLine::new(vec![Span::dim_text("? for shortcuts")]));
    }
    lines.extend(footer);

    // 全帧兜底折行。输入框行本身已按 body_width 折过（加 2 列前缀 ≤ width），
    // 只有输入框之前的行可能再折——光标行号要加上它们多折出来的行数。
    let mut wrapped: Vec<StyledLine> = Vec::new();
    let mut cursor_extra = 0usize;
    for (i, l) in lines.iter().enumerate() {
        let parts = wrap_line(l, width);
        if i < input_start {
            cursor_extra += parts.len().saturating_sub(1);
        }
        wrapped.extend(parts);
    }
    let cursor_row = input_start + input_row + cursor_extra;
    DynamicFrame {
        lines: wrapped,
        cursor: Some((cursor_row, cursor_col.min(width.saturating_sub(1)))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::complete::CompletionCandidate;
    use crate::tui::input::InputBuffer;

    fn frame_of(state: &TuiState, input: &InputBuffer, width: usize) -> DynamicFrame {
        let ctx = FrameCtx {
            state,
            input,
            completion: None,
            confirm: None,
            bash_mode: false,
            mode: "manual",
            context_pct: None,
            rewind: None,
            width,
            frame_tick: 0,
        };
        render_frame(&ctx)
    }

    #[test]
    fn frame_contains_input_box_and_footer() {
        let mut s = TuiState::new("m");
        s.push_user("hi");
        let mut input = InputBuffer::new();
        input.set_text("测试输入");
        let f = frame_of(&s, &input, 60);
        let texts: Vec<String> = f.lines.iter().map(|l| l.text()).collect();
        // 已 push 的 User 块是冻结块 → 打进 scrollback，不进动态区
        assert!(
            !texts.iter().any(|t| t.starts_with("> hi")),
            "冻结块不出现在动态区"
        );
        assert!(texts.iter().any(|t| t.starts_with("╭") && t.ends_with("╮")));
        assert!(texts.iter().any(|t| t.starts_with("╰") && t.ends_with("╯")));
        // 下边框紧跟最后一行输入体（闭合矩形）
        let input_pos = texts.iter().position(|t| t.contains("测试输入")).unwrap();
        let bottom_pos = texts.iter().position(|t| t.starts_with("╰")).unwrap();
        assert_eq!(bottom_pos, input_pos + 1, "╰╯ 紧跟输入行");
        assert!(texts
            .iter()
            .any(|t| t.starts_with("❯ ") && t.contains("测试输入")));
        assert!(texts
            .last()
            .map(|t| t == "? for shortcuts")
            .unwrap_or(false));
        // 光标落在输入行
        let (row, col) = f.cursor.unwrap();
        assert_eq!(
            f.lines[row].text().chars().take(2).collect::<String>(),
            "❯ "
        );
        assert_eq!(
            col,
            2 + "测试输入"
                .chars()
                .map(|c| unicode_width::UnicodeWidthChar::width(c).unwrap_or(1))
                .sum::<usize>()
        );
    }

    #[test]
    fn frame_busy_shows_spinner_and_interrupt_hint() {
        let mut s = TuiState::new("m");
        s.begin_turn();
        s.turn_started = Some(std::time::Instant::now());
        let input = InputBuffer::new();
        let f = frame_of(&s, &input, 90);
        let spinner_line = f
            .lines
            .iter()
            .find(|l| l.text().contains("esc to interrupt"))
            .expect("spinner 行存在");
        assert!(spinner_line
            .text()
            .contains(&format!("{}…", s.spinner_verb)));
    }

    #[test]
    fn mode_indicator_in_footer() {
        let s = TuiState::new("m");
        let input = InputBuffer::new();
        for (mode, needle, color) in [
            ("plan", "⏸ plan mode on", Color::PlanMode),
            ("auto", "⏵⵵ accept edits on", Color::AutoAccept),
            ("passon", "⏵⵵ bypass permissions on", Color::Error),
        ] {
            let ctx = FrameCtx {
                state: &s,
                input: &input,
                completion: None,
                confirm: None,
                bash_mode: false,
                mode,
                context_pct: Some(42),
                rewind: None,
                width: 90,
                frame_tick: 0,
            };
            let f = render_frame(&ctx);
            let hit = f.lines.iter().find(|l| l.text().contains(needle)).unwrap();
            assert_eq!(hit.0[0].color, color, "{mode}");
            assert!(f.lines.iter().any(|l| l.text() == "42% until auto-compact"));
        }
    }

    #[test]
    fn completion_menu_renders_above_input() {
        let mut s = TuiState::new("m");
        let input = InputBuffer::new();
        let comp = CompletionState {
            at: 0,
            trigger: '/',
            dir_part: String::new(),
            candidates: vec![
                CompletionCandidate {
                    insert: "help ".into(),
                    display: "/help".into(),
                    is_dir: false,
                    description: "帮助".into(),
                },
                CompletionCandidate {
                    insert: "quit ".into(),
                    display: "/quit".into(),
                    is_dir: false,
                    description: String::new(),
                },
            ],
            selected: 0,
        };
        let ctx = FrameCtx {
            state: &s,
            input: &input,
            completion: Some(&comp),
            confirm: None,
            bash_mode: false,
            mode: "manual",
            context_pct: None,
            rewind: None,
            width: 90,
            frame_tick: 0,
        };
        let f = render_frame(&ctx);
        let texts: Vec<String> = f.lines.iter().map(|l| l.text()).collect();
        let help_pos = texts.iter().position(|t| t.contains("/help")).unwrap();
        let box_pos = texts.iter().position(|t| t.starts_with("╭")).unwrap();
        assert!(help_pos < box_pos, "菜单在输入框上方（composer 恒钉底）");
        assert!(texts
            .iter()
            .any(|t| t.contains("❯ /help") && t.contains("帮助")));
        assert!(texts
            .iter()
            .any(|t| t.contains("/quit") && !t.contains("❯")));
        let _ = &mut s;
    }

    #[test]
    fn bash_mode_prompt_is_orange_bang() {
        let s = TuiState::new("m");
        let mut input = InputBuffer::new();
        input.set_text("cargo test");
        let ctx = FrameCtx {
            state: &s,
            input: &input,
            completion: None,
            confirm: None,
            bash_mode: true,
            mode: "manual",
            context_pct: None,
            width: 90,
            frame_tick: 0,
            rewind: None,
        };
        let f = render_frame(&ctx);
        let input_line = f.lines.iter().find(|l| l.text().starts_with("! ")).unwrap();
        assert_eq!(input_line.0[0].color, Color::BashBorder);
        let border = f.lines.iter().find(|l| l.text().starts_with("╭")).unwrap();
        assert_eq!(border.0[0].color, Color::BashBorder, "bash 模式边框橙");
        let bottom = f.lines.iter().find(|l| l.text().starts_with("╰")).unwrap();
        assert_eq!(bottom.0[0].color, Color::BashBorder, "bash 模式下边框橙");
    }
}
