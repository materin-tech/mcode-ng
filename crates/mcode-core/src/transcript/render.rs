//! 单块渲染：Block → 样式行。冻结区与动态区、CLI 与 Desktop 共用。
//!
//! 整帧组装（queue + spinner + 对话框 + 输入框 + footer）是终端特有逻辑，
//! 留在 mcode-cli 的 tui/render.rs；本模块只回答「一块长什么样」。
//! Desktop 端以 `width = usize::MAX` 调用（gpui 容器自行折行）。

use super::blocks::{AgentState, Block, InternalLine, NoticeKind, ToolBlock, ToolStatus};
use super::markdown;
use super::style::{truncate_chars, wrap_line, Color, Span, StyledLine};
use super::tools;

/// 单块渲染（冻结区与动态区共用）。`blink_on` 只影响进行中工具的点。
pub fn render_block(
    b: &Block,
    detail: bool,
    width: usize,
    blink_on: bool,
    frozen: bool,
) -> Vec<StyledLine> {
    let mut out: Vec<StyledLine> = Vec::new();
    match b {
        Block::User { text } => {
            for (i, l) in wrap_line(
                &StyledLine::new(vec![Span::new(text)]),
                width.saturating_sub(2),
            )
            .into_iter()
            .enumerate()
            {
                let mut line = StyledLine::default();
                line.push(if i == 0 {
                    Span::colored(Color::Suggestion, "> ")
                } else {
                    Span::new("  ")
                });
                line.0.extend(l.0);
                out.push(line);
            }
        }
        Block::Assistant { text, streaming } => {
            if *streaming {
                out.extend(render_streaming_assistant(text, width));
                return out;
            }
            let body = markdown::render_markdown(text);
            out.extend(prefix_dot(body, false));
        }
        Block::Thinking { text, streaming } => {
            // 提示只在块还能重绘（动态区/桌面端）时出现：冻结进 scrollback 的块
            // 无法重绘，再印 "(ctrl+o to expand)" 就是空头承诺
            let hint = if frozen {
                ""
            } else if detail {
                " (ctrl+o to collapse)"
            } else if *streaming {
                "… (ctrl+o to expand)"
            } else {
                " (ctrl+o to expand)"
            };
            out.push(StyledLine::new(vec![
                Span::dim_text("⏺  ∴ Thinking").italic(),
                Span::dim_text(hint).italic(),
            ]));
            if !detail {
                return out;
            }
            let body = markdown::render_markdown(text);
            let shown: Vec<StyledLine> = if *streaming {
                body.into_iter()
                    .rev()
                    .take(3)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect()
            } else {
                body
            };
            for l in shown {
                let mut line = StyledLine::default();
                line.push(Span::new("  "));
                for s in l.0 {
                    line.push(Span::dim_text(s.text).italic());
                }
                out.push(line);
            }
        }
        Block::Tool(t) => out.extend(render_tool(t, detail, width, blink_on)),
        Block::Todo { items } => out.extend(render_todo(items)),
        Block::Notice { text, kind } => {
            let color = match kind {
                NoticeKind::Info => Color::Dim,
                NoticeKind::Success => Color::Success,
                NoticeKind::Warning => Color::Warning,
                NoticeKind::Error => Color::Error,
            };
            for (i, seg) in wrap_line(
                &StyledLine::new(vec![Span::colored(color, text)]),
                width.saturating_sub(6),
            )
            .into_iter()
            .enumerate()
            {
                let mut line = StyledLine::default();
                line.push(if i == 0 {
                    Span::colored(color, "  ⎯  ")
                } else {
                    Span::new("     ")
                });
                line.0.extend(seg.0);
                out.push(line);
            }
        }
        Block::Interrupted => {
            out.push(StyledLine::new(vec![
                Span::new("  ⎿  "),
                Span::colored(Color::Error, "Interrupted").bold(),
                Span::dim_text(" by user"),
            ]));
        }
    }
    out
}

/// 冻结块 → 行集（含块间空行），由调用方一次性上屏。
pub fn render_frozen(b: &Block, detail: bool, width: usize) -> Vec<StyledLine> {
    let mut out = render_block(b, detail, width, false, true);
    out.push(StyledLine::blank());
    out
}

/// 流式正文：稳定前缀走 markdown，尾部（未定稿块）按纯文本 —— 已打印
/// 内容视觉上不再回退，逼近 CC 的 stable-prefix 单调推进。
fn render_streaming_assistant(text: &str, _width: usize) -> Vec<StyledLine> {
    let (stable, tail) = markdown::split_stable_stream(text);
    let mut body = markdown::render_markdown_trimmed(&stable);
    if tail.len() > 2 {
        let tail = tail.trim_start_matches('\n');
        for l in tail.lines() {
            body.push(StyledLine::plain(l));
        }
    }
    prefix_dot(body, false)
}

/// `⏺  ` 头 + 后续行 2 空格对齐。
fn prefix_dot(body: Vec<StyledLine>, dim_dot: bool) -> Vec<StyledLine> {
    let mut out = Vec::with_capacity(body.len());
    for (i, l) in body.into_iter().enumerate() {
        let mut line = StyledLine::default();
        if i == 0 {
            line.push(if dim_dot {
                Span::dim_text("⏺  ")
            } else {
                Span::new("⏺  ")
            });
        } else {
            line.push(Span::new("  "));
        }
        line.0.extend(l.0);
        out.push(line);
    }
    if out.is_empty() {
        out.push(StyledLine::new(vec![Span::new("⏺  ")]));
    }
    out
}

/// 工具块：`⏺ Tool(args)` + `⎿` 结果 / agent 树枝。
fn render_tool(t: &ToolBlock, detail: bool, width: usize, blink_on: bool) -> Vec<StyledLine> {
    let mut out = Vec::new();
    let pending = matches!(t.status, ToolStatus::Pending);
    // pending 闪烁（亮/dim 交替）；定稿后恒 dim
    let dot = if pending && blink_on {
        Span::new("⏺")
    } else {
        Span::dim_text("⏺")
    };
    let header = tools::tool_header(&t.tool, &t.args_json);
    let header = truncate_chars(&header, width.saturating_sub(4).max(20));
    out.push(StyledLine::new(vec![
        dot,
        Span::new("  "),
        Span::new(header),
    ]));

    match (&t.status, &t.agent) {
        (ToolStatus::Pending, None) => {
            out.push(StyledLine::new(vec![
                Span::new("  ⎿  "),
                Span::dim_text("Running…"),
            ]));
        }
        (ToolStatus::Pending, Some(agent)) => {
            out.extend(render_agent_progress(agent, detail));
        }
        (
            ToolStatus::Resolved {
                success,
                output,
                display,
            },
            agent,
        ) => {
            if let Some(a) = agent {
                out.extend(render_agent_progress(a, detail));
                // CC 的 Done 汇总行
                let summary = t.agent_done_summary();
                out.push(StyledLine::new(vec![
                    Span::new("  ⎿  "),
                    Span::colored(Color::Success, "Done"),
                    Span::dim_text(summary.trim_start_matches("Done")),
                ]));
            }
            let result = tools::tool_result_lines(
                &t.tool,
                &t.args_json,
                *success,
                output,
                display.as_ref(),
                detail,
            );
            let mut first = true;
            for l in result {
                let mut line = StyledLine::default();
                line.push(if first {
                    Span::dim_text("  ⎿  ")
                } else {
                    Span::new("     ")
                });
                first = false;
                line.0.extend(l.0);
                out.push(line);
            }
            if first && agent.is_none() {
                out.push(StyledLine::new(vec![
                    Span::new("  ⎿  "),
                    Span::dim_text("Done"),
                ]));
            }
        }
    }
    out
}

/// agent 进行中状态：Initializing… / 最近 ≤3 条内部工具 / +N more。
fn render_agent_progress(agent: &AgentState, detail: bool) -> Vec<StyledLine> {
    let mut out = Vec::new();
    let tool_lines: Vec<_> = agent
        .internal
        .iter()
        .filter(|l| matches!(l, InternalLine::Tool { .. }))
        .collect();
    let thinking_lines: Vec<_> = agent
        .internal
        .iter()
        .filter(|l| matches!(l, InternalLine::Thinking(_)))
        .collect();
    if detail {
        for l in &agent.internal {
            match l {
                InternalLine::Tool {
                    summary, success, ..
                } => {
                    let mut line = StyledLine::default();
                    line.push(Span::new("  │  ⎿  "));
                    line.push(if *success {
                        Span::dim_text(summary)
                    } else {
                        Span::colored(Color::Error, summary)
                    });
                    out.push(line);
                }
                InternalLine::Thinking(t) => {
                    out.push(StyledLine::new(vec![
                        Span::new("  │  "),
                        Span::dim_text("∴ ").italic(),
                        Span::dim_text(truncate_chars(t, 120)).italic(),
                    ]));
                }
            }
        }
    } else {
        let show = 3;
        for l in tool_lines.iter().rev().take(show).rev() {
            if let InternalLine::Tool {
                summary, success, ..
            } = l
            {
                let mut line = StyledLine::default();
                line.push(Span::new("  │  ⎿  "));
                line.push(if *success {
                    Span::dim_text(summary)
                } else {
                    Span::colored(Color::Error, summary)
                });
                out.push(line);
            }
        }
        if tool_lines.len() > show {
            out.push(StyledLine::new(vec![Span::dim_text(format!(
                "  ⎿  +{} more tool use(s) (ctrl+o to expand)",
                tool_lines.len() - show
            ))]));
        }
        if !thinking_lines.is_empty() {
            let _ = thinking_lines.len();
        }
    }
    if out.is_empty() {
        out.push(StyledLine::new(vec![
            Span::new("  ⎿  "),
            Span::dim_text("Initializing…"),
        ]));
    }
    out
}

/// Todo 清单（CC TaskListV2 样式）。
fn render_todo(items: &[tools::TodoItem]) -> Vec<StyledLine> {
    let done = items.iter().filter(|i| i.status == "completed").count();
    let in_prog = items.iter().filter(|i| i.status == "in_progress").count();
    let open = items.len() - done - in_prog;
    let mut out = vec![StyledLine::new(vec![
        Span::dim_text(format!("{} ", items.len())),
        Span::dim_text("task"),
        Span::dim_text(if items.len() == 1 { "" } else { "s" }),
        Span::dim_text(" ("),
        Span::new(done.to_string()).bold(),
        Span::dim_text(" done, "),
        Span::new(in_prog.to_string()).bold(),
        Span::dim_text(" in progress, "),
        Span::new(open.to_string()).bold(),
        Span::dim_text(" open)"),
    ])];
    for item in items {
        let label = if item.status == "in_progress" && !item.active_form.is_empty() {
            item.active_form.clone()
        } else {
            item.content.clone()
        };
        let line = match item.status.as_str() {
            "completed" => StyledLine::new(vec![
                Span::colored(Color::Success, "✓ "),
                Span::dim_text(label).strike(),
            ]),
            "in_progress" => StyledLine::new(vec![
                Span::colored(Color::Claude, "▪ ").bold(),
                Span::colored(Color::Claude, label).bold(),
            ]),
            _ => StyledLine::new(vec![Span::dim_text("▪ "), Span::new(label)]),
        };
        out.push(line);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentEvent;
    use crate::tools::DisplayBlock;
    use crate::transcript::blocks::TranscriptState;

    #[test]
    fn user_block_renders_gt_prefix() {
        let lines = render_block(
            &Block::User {
                text: "修 bug".into(),
            },
            false,
            80,
            false,
            false,
        );
        assert_eq!(lines[0].text(), "> 修 bug");
        assert_eq!(lines[0].0[0].color, Color::Suggestion);
    }

    #[test]
    fn tool_pending_blinks_dot_and_running_line() {
        let mut s = TranscriptState::new("m");
        s.begin_turn();
        s.apply(AgentEvent::ToolUseStart {
            id: "1".into(),
            name: "bash".into(),
            scope: None,
        });
        let on = render_block(&s.blocks[0], false, 80, true, false);
        let off = render_block(&s.blocks[0], false, 80, false, false);
        assert_eq!(on[0].text(), "⏺  Bash(?)", "args 未到时兜底 ?");
        assert_eq!(on[0].0[0].color, Color::Text, "blink on = 亮点");
        assert_eq!(off[0].0[0].color, Color::Dim, "blink off = dim 点");
        assert!(on[1].text().contains("Running…"));
    }

    #[test]
    fn resolved_tool_shows_result_with_gutter() {
        let mut s = TranscriptState::new("m");
        s.begin_turn();
        s.apply(AgentEvent::ToolUseStart {
            id: "1".into(),
            name: "read_file".into(),
            scope: None,
        });
        s.apply(AgentEvent::ToolInputDelta {
            id: "1".into(),
            partial_json: r#"{"path":"src/main.rs"}"#.into(),
            scope: None,
        });
        s.apply(AgentEvent::ToolResult {
            name: "read_file".into(),
            success: true,
            output: String::new(),
            display: Some(DisplayBlock::FileRef {
                path: "src/main.rs".into(),
                lines: "1-120".into(),
            }),
            scope: None,
        });
        let lines = render_block(&s.blocks[0], false, 80, false, false);
        assert_eq!(lines[0].text(), "⏺  Read(src/main.rs)");
        assert_eq!(lines[0].0[0].color, Color::Dim, "定稿点 dim");
        assert_eq!(lines[1].text(), "  ⎿  Read 120 lines");
    }

    #[test]
    fn todo_block_renders_counts_and_styles() {
        let items = vec![
            tools::TodoItem {
                content: "完成".into(),
                status: "completed".into(),
                active_form: String::new(),
            },
            tools::TodoItem {
                content: "进行".into(),
                status: "in_progress".into(),
                active_form: "进行中".into(),
            },
            tools::TodoItem {
                content: "待办".into(),
                status: "pending".into(),
                active_form: String::new(),
            },
        ];
        let lines = render_todo(&items);
        assert_eq!(lines[0].text(), "3 tasks (1 done, 1 in progress, 1 open)");
        assert_eq!(lines[1].text(), "✓ 完成");
        assert!(lines[1].0[0].color == Color::Success);
        assert!(lines[1].0[1].strike, "完成项删除线");
        assert_eq!(lines[2].text(), "▪ 进行中", "in_progress 用 active_form");
        assert_eq!(lines[2].0[1].color, Color::Claude);
        assert_eq!(lines[3].text(), "▪ 待办");
    }

    #[test]
    fn interrupted_block_renders() {
        let lines = render_block(&Block::Interrupted, false, 80, false, false);
        assert_eq!(lines[0].text(), "  ⎿  Interrupted by user");
        assert_eq!(lines[0].0[1].color, Color::Error);
    }

    #[test]
    fn thinking_expand_hint_only_while_redrawable() {
        let b = Block::Thinking {
            text: "想想".into(),
            streaming: false,
        };
        let live = render_block(&b, false, 80, false, false);
        assert!(
            live[0].text().contains("(ctrl+o to expand)"),
            "动态区可重绘才印 expand 提示: {:?}",
            live[0].text()
        );
        let frozen = render_frozen(&b, false, 80);
        assert!(
            !frozen[0].text().contains("ctrl+o"),
            "冻结块印 expand 提示是空头承诺: {:?}",
            frozen[0].text()
        );
        let expanded = render_block(&b, true, 80, false, false);
        assert!(
            expanded[0].text().contains("(ctrl+o to collapse)"),
            "已展开提示 collapse: {:?}",
            expanded[0].text()
        );
        // 冻结 + detail：正文展开但头部仍无提示
        let frozen_detail = render_frozen(&b, true, 80);
        assert!(!frozen_detail[0].text().contains("ctrl+o"));
        assert!(frozen_detail.iter().any(|l| l.text().contains("想想")));
    }
}
