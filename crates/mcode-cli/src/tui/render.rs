//! TUI 渲染：transcript + composer + statusline 三段布局。
//!
//! 只读 AppState，不改状态；滚动语义：贴底自动跟随，PageUp/PageDown/
//! 滚轮临时回看，一旦有新内容回贴底。

use super::state::{AppState, LineKind};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line as RLine, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;

/// 处理按键（编辑/滚动/模式/退出）；返回是否被消费。
pub fn on_key(state: &mut AppState, key: KeyEvent) -> bool {
    match key.code {
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.should_quit = true;
            true
        }
        KeyCode::Char('o') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.show_details = !state.show_details;
            true
        }
        KeyCode::PageUp => {
            scroll_up(state, 10);
            true
        }
        KeyCode::PageDown => {
            scroll_down(state, 10);
            true
        }
        KeyCode::Up if key.modifiers.contains(KeyModifiers::SHIFT) => {
            scroll_up(state, 3);
            true
        }
        KeyCode::Down if key.modifiers.contains(KeyModifiers::SHIFT) => {
            scroll_down(state, 3);
            true
        }
        KeyCode::Up => {
            state.history_prev();
            true
        }
        KeyCode::Down => {
            state.history_next();
            true
        }
        KeyCode::Backspace => {
            state.input.pop();
            true
        }
        KeyCode::Enter => true, // 交给 run 循环（要触发 send 逻辑）
        KeyCode::Char(ch) => {
            state.input.push(ch);
            true
        }
        _ => false,
    }
}

/// 滚轮：上滚回看，下滚回底。
pub fn on_mouse(state: &mut AppState, kind: MouseEventKind) {
    match kind {
        MouseEventKind::ScrollUp => scroll_up(state, 3),
        MouseEventKind::ScrollDown => scroll_down(state, 3),
        MouseEventKind::Down(MouseButton::Left) => {}
        _ => {}
    }
}

fn scroll_up(state: &mut AppState, n: u16) {
    state.scroll_from_bottom = Some(state.scroll_from_bottom.unwrap_or(0).saturating_add(n));
}

fn scroll_down(state: &mut AppState, n: u16) {
    let next = state.scroll_from_bottom.unwrap_or(0).saturating_sub(n);
    state.scroll_from_bottom = if next == 0 { None } else { Some(next) };
}

/// 整帧绘制。
pub fn draw(f: &mut Frame, state: &AppState) {
    let [transcript, composer, status] = Layout::vertical([
        Constraint::Min(5),
        Constraint::Length(3),
        Constraint::Length(1),
    ])
    .areas(f.area());

    draw_transcript(f, state, transcript);
    draw_composer(f, state, composer);
    draw_statusline(f, state, status);
    if state.completion.is_some() {
        draw_completion(f, state, transcript);
    }
    draw_overlays(f, state, f.area());
}

/// 补全弹窗（'@' 文件 / '/' 命令）：贴在 composer 上沿，盖住 transcript 底部。
/// 候选超过一屏时按 state.completion_window 滚动，选中项始终可见。
fn draw_completion(f: &mut Frame, state: &AppState, area: Rect) {
    let Some(c) = &state.completion else { return };
    let (start, win) = state.completion_window();
    // 高度预算：候选行 + 提示行 + 上下边框；终端过矮时再裁可视行数
    let vis = win.min(area.height.saturating_sub(3) as usize);
    if vis == 0 {
        return;
    }
    let mut lines: Vec<RLine> = c.candidates[start..start + vis]
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let style = if start + i == c.selected {
                Style::default().bg(Color::Blue).fg(Color::White)
            } else {
                Style::default()
            };
            RLine::from(Span::styled(format!(" {p} "), style))
        })
        .collect();
    lines.push(RLine::from(Span::styled(
        "↑↓ 选择 · Tab/Enter 补全 · Esc 关闭",
        Style::default().fg(Color::DarkGray),
    )));
    let title = if c.trigger == '@' {
        " @ 文件 "
    } else {
        " 命令 "
    };
    let w = 52.min(area.width);
    let h = (vis as u16 + 3).min(area.height);
    let box_area = Rect {
        x: area.x,
        y: area.bottom().saturating_sub(h),
        width: w,
        height: h,
    };
    f.render_widget(Clear, box_area);
    f.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan))
                .title(Span::styled(
                    title,
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )),
        ),
        box_area,
    );
}

/// 覆盖层：权限确认弹窗 / rewind 列表（后画者在上）。
fn draw_overlays(f: &mut Frame, state: &AppState, area: Rect) {
    if state.rewind.is_some() {
        draw_rewind(f, state, area);
    }
    if state.confirm.is_some() {
        draw_confirm(f, state, area);
    }
}

/// 居中矩形（钳制在 area 内）。
fn centered(width: u16, height: u16, area: Rect) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    }
}

fn draw_confirm(f: &mut Frame, state: &AppState, area: Rect) {
    let Some(c) = &state.confirm else { return };
    let body = vec![
        RLine::from(Span::styled(
            format!("工具 {} 请求执行：", c.tool),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        RLine::from(""),
        RLine::from(Span::styled(
            c.args_summary.clone(),
            Style::default().fg(Color::DarkGray),
        )),
        RLine::from(""),
        RLine::from(Span::styled(
            "[y/Enter] 允许一次   [a] 本会话总是允许   [n/Esc] 拒绝",
            Style::default().fg(Color::Yellow),
        )),
    ];
    let box_area = centered(62, (body.len() as u16) + 2, area);
    f.render_widget(Clear, box_area);
    f.render_widget(
        Paragraph::new(body).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow))
                .title(Span::styled(
                    " 权限确认 ",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )),
        ),
        box_area,
    );
}

fn draw_rewind(f: &mut Frame, state: &AppState, area: Rect) {
    let Some(sel) = state.rewind else { return };
    // 最新在前展示；选中下标（最新在前）映射回时间序比对
    let mut items: Vec<RLine> = Vec::new();
    for (i, cp) in state.checkpoints.iter().enumerate().rev() {
        let style = if state.checkpoints.len() - 1 - sel == i {
            Style::default().bg(Color::Blue).fg(Color::White)
        } else {
            Style::default()
        };
        let prompt: String = cp.prompt.chars().take(48).collect();
        items.push(RLine::from(Span::styled(format!(" {prompt} "), style)));
    }
    items.push(RLine::from(Span::styled(
        "↑↓ 选择 · Enter 回退到此处（丢弃其后改动）· Esc 取消",
        Style::default().fg(Color::DarkGray),
    )));
    let h = (items.len() as u16 + 2).min(area.height * 2 / 3);
    let box_area = centered(64, h, area);
    f.render_widget(Clear, box_area);
    f.render_widget(
        Paragraph::new(items).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Blue))
                .title(Span::styled(
                    " 回退会话 ",
                    Style::default()
                        .fg(Color::Blue)
                        .add_modifier(Modifier::BOLD),
                )),
        ),
        box_area,
    );
}

fn draw_transcript(f: &mut Frame, state: &AppState, area: Rect) {
    let mut lines: Vec<RLine> = Vec::with_capacity(state.lines.len() + 2);
    for l in &state.lines {
        // 思考流只在详情视图出现
        if l.kind == LineKind::Thinking && !state.show_details {
            continue;
        }
        let style = match l.kind {
            LineKind::User => Style::default().fg(Color::Cyan),
            LineKind::Assistant => Style::default(),
            LineKind::Tool => Style::default().fg(Color::DarkGray),
            LineKind::Thinking => Style::default()
                .fg(Color::Blue)
                .add_modifier(Modifier::ITALIC),
            LineKind::System => Style::default().fg(Color::Yellow),
        };
        let prefix = if l.kind == LineKind::User { "> " } else { "" };
        // 详情视图：有 detail 的行展开为多行（工具完整输出/思考全文）
        let body_text = if state.show_details {
            l.detail.as_deref().unwrap_or(l.text.as_str())
        } else {
            l.text.as_str()
        };
        for (i, seg) in body_text.split('\n').enumerate() {
            if i == 0 {
                lines.push(RLine::from(vec![
                    Span::styled(prefix, style.add_modifier(Modifier::BOLD)),
                    Span::styled(seg.to_string(), style),
                ]));
            } else {
                lines.push(RLine::from(Span::styled(seg.to_string(), style)));
            }
        }
    }
    // 空会话引导
    if lines.is_empty() {
        lines.push(RLine::from(Span::styled(
            "MCode 已就绪。输入需求开始，/help 看命令，shift+tab 切权限模式。",
            Style::default().fg(Color::DarkGray),
        )));
    }

    // 手工折行：1 逻辑行 → N 可视行（Paragraph 不再 Wrap），
    // 折后行数与终端行一一对应，贴底/上滚窗口按可视行精确计算
    let width = area.width.max(1) as usize;
    let mut rows: Vec<RLine> = Vec::with_capacity(lines.len());
    for line in &lines {
        for row in wrap_line(&line.spans, width) {
            rows.push(RLine::from(row));
        }
    }
    let (start, end) = transcript_window(
        rows.len(),
        area.height as usize,
        state.scroll_from_bottom.unwrap_or(0) as usize,
    );
    let view: Vec<RLine> = rows.drain(start..end).collect();
    let para = Paragraph::new(view).block(Block::default().borders(Borders::NONE));
    f.render_widget(para, area);
}

/// transcript 视口窗口 (start, end)：贴底（offset=0）显示最后 height 行；
/// 上滚 offset 行时视口结束于内容底部上方 offset 行。
fn transcript_window(total: usize, height: usize, offset: usize) -> (usize, usize) {
    let end = total.saturating_sub(offset);
    (end.saturating_sub(height), end)
}

/// 按显示宽度把带样式行硬折成若干可视行（每行 ≤ width 列）。
/// CJK 等宽字符按 2 列计（unicode-width）；放不进任何行的超宽单字丢弃；
/// 行首零宽字符丢弃。折行后逻辑行与终端行一一对应。
fn wrap_line<'a>(spans: &[Span<'a>], width: usize) -> Vec<Vec<Span<'a>>> {
    use unicode_width::UnicodeWidthChar;
    let width = width.max(1);
    let mut rows: Vec<Vec<Span<'a>>> = Vec::new();
    let mut cur: Vec<Span<'a>> = Vec::new();
    let mut used = 0usize;
    // 当前行内未封口的同风格 chunk（style, 文本, 宽度）——避免逐字建 Span
    let mut open: Option<(Style, String, usize)> = None;

    for span in spans {
        let style = span.style;
        for ch in span.content.chars() {
            let cw = ch.width().unwrap_or(0);
            if cw > width {
                continue;
            }
            let open_w = open.as_ref().map_or(0, |(_, _, w)| *w);
            if used + open_w + cw > width {
                // 折行：封 chunk → 收行
                if let Some((s, buf, _)) = open.take() {
                    if !buf.is_empty() {
                        cur.push(Span::styled(buf, s));
                    }
                }
                rows.push(std::mem::take(&mut cur));
                used = 0;
                if cw == 0 {
                    continue;
                }
            }
            match &mut open {
                Some((s, buf, w)) if *s == style => {
                    buf.push(ch);
                    *w += cw;
                }
                _ => {
                    if let Some((s, buf, w)) = open.take() {
                        if !buf.is_empty() {
                            cur.push(Span::styled(buf, s));
                        }
                        used += w;
                    }
                    open = Some((style, String::from(ch), cw));
                }
            }
        }
    }
    if let Some((s, buf, _)) = open.take() {
        if !buf.is_empty() {
            cur.push(Span::styled(buf, s));
        }
    }
    rows.push(cur);
    rows
}

fn draw_composer(f: &mut Frame, state: &AppState, area: Rect) {
    let title = if state.busy {
        format!("输入（排队 {} 条，esc 打断）", state.queue.len())
    } else {
        "输入".to_string()
    };
    let para = Paragraph::new(state.input.as_str())
        .wrap(Wrap { trim: false })
        .block(Block::default().borders(Borders::ALL).title(Span::styled(
            title,
            Style::default().fg(if state.busy {
                Color::Yellow
            } else {
                Color::Blue
            }),
        )));
    f.render_widget(para, area);
}

fn draw_statusline(f: &mut Frame, state: &AppState, area: Rect) {
    let mode_style = match state.mode {
        mcode_core::permission::Mode::Passon => Color::DarkGray,
        mcode_core::permission::Mode::Auto => Color::Green,
        mcode_core::permission::Mode::Manual => Color::Yellow,
        mcode_core::permission::Mode::Plan => Color::Blue,
    };
    let mut spans = vec![
        Span::styled(
            format!(" {} ", state.mode.label()),
            Style::default().bg(mode_style).fg(Color::Black),
        ),
        Span::raw(" "),
    ];
    if !state.model.is_empty() {
        spans.push(Span::styled(
            state.model.clone(),
            Style::default().fg(Color::Magenta),
        ));
        spans.push(Span::raw("  "));
    }
    spans.push(Span::styled(
        state.status.clone(),
        Style::default().fg(Color::DarkGray),
    ));
    spans.push(Span::raw("  "));
    spans.push(Span::styled(
        state.tokens.render(),
        Style::default().fg(Color::DarkGray),
    ));
    spans.push(Span::raw("  "));
    spans.push(Span::styled(
        if state.show_details {
            "详情"
        } else {
            "简洁"
        },
        Style::default().fg(Color::DarkGray),
    ));
    f.render_widget(Paragraph::new(RLine::from(spans)), area);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spans_text(spans: &[Span<'_>]) -> String {
        spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn transcript_window_pins_to_bottom() {
        // 100 行、可视 10：贴底 = 最后 10 行
        assert_eq!(transcript_window(100, 10, 0), (90, 100));
        // 上滚 5：视口结束于内容底部上方 5 行
        assert_eq!(transcript_window(100, 10, 5), (85, 95));
        // 内容不足一屏：从头显示
        assert_eq!(transcript_window(5, 10, 0), (0, 5));
        // 偏移超过内容：视口清空
        assert_eq!(transcript_window(5, 10, 50), (0, 0));
    }

    #[test]
    fn wrap_line_hard_breaks_by_display_width() {
        let width = 10;
        // ASCII 30 列 → 3 行等宽
        let rows = wrap_line(&[Span::raw("a".repeat(30))], width);
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().all(|r| spans_text(r) == "aaaaaaaaaa"));

        // CJK 每字 2 列：宽 3 下每行只装 1 字
        let rows = wrap_line(&[Span::raw("一二三四五")], 3);
        assert_eq!(rows.len(), 5);
        assert_eq!(spans_text(&rows[0]), "一");

        // 未超宽 → 原样 1 行；空内容 → 1 空行
        let rows = wrap_line(&[Span::raw("hello")], 10);
        assert_eq!(rows.len(), 1);
        assert_eq!(spans_text(&rows[0]), "hello");
        assert_eq!(wrap_line(&[Span::raw("")], 10).len(), 1);
    }

    #[test]
    fn wrap_line_preserves_styles_across_breaks() {
        let red = Style::default().fg(Color::Red);
        let blue = Style::default().fg(Color::Blue);
        let spans = vec![Span::styled("aaaa", red), Span::styled("bbbb", blue)];

        // 宽 4：恰好按 span 边界折成两行，样式各自保留
        let rows = wrap_line(&spans, 4);
        assert_eq!(rows.len(), 2);
        assert_eq!(spans_text(&rows[0]), "aaaa");
        assert_eq!(spans_text(&rows[1]), "bbbb");
        assert_eq!(rows[0][0].style.fg, Some(Color::Red));
        assert_eq!(rows[1][0].style.fg, Some(Color::Blue));

        // 宽 5：折断点落在第二个 span 内部，row0 = 红 aaaa + 蓝 b
        let rows = wrap_line(&spans, 5);
        assert_eq!(rows.len(), 2);
        assert_eq!(spans_text(&rows[0]), "aaaab");
        assert_eq!(spans_text(&rows[1]), "bbb");
        assert_eq!(rows[0].len(), 2, "折断行内应有两个样式段");
        assert_eq!(rows[0][0].style.fg, Some(Color::Red));
        assert_eq!(rows[0][1].style.fg, Some(Color::Blue));
    }
}
