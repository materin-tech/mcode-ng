// MCode_Desktop — AI coding agent desktop app
// Copyright (c) 2026 Materin-tech. All rights reserved.
// SPDX-License-Identifier: MIT

//! transcript 呈现桥 — mcode_core::transcript 的 StyledLine（CLI 共享渲染核）
//! → gpui StyledText。颜色映射 = CLI 主题 token → Desktop 色板；
//! 粗体/斜体/下划线/删除线逐 span 落到 HighlightStyle，gpui 容器自行折行。
//! 行级语义（LineKind）→ 行容器样式：标题提字号、代码块上面板、空行撑行高。

use gpui::{
    FontStyle, FontWeight, HighlightStyle, ParentElement, Pixels, Styled, StyledText, div, px,
};
use mcode_core::transcript::{Color, LineKind, StyledLine};

use crate::views::theme::*;

/// CLI 语义色 token → Desktop 色板（名字对齐 CC 主题系统）
pub(crate) fn color_of(c: Color) -> gpui::Rgba {
    match c {
        Color::Text => TEXT_1,
        Color::Dim => TEXT_3,
        Color::Suggestion => BLUE,
        Color::Success => GREEN,
        Color::Error => RED,
        Color::Warning => YELLOW,
        Color::Permission => PURPLE,
        Color::BashBorder => ACCENT,
        Color::PlanMode => BLUE,
        Color::AutoAccept => YELLOW,
        Color::Claude => ACCENT,
    }
}

/// 单行 → gpui StyledText（逐 span 生成字节区间高亮）
pub(crate) fn styled_line(line: &StyledLine) -> StyledText {
    let mut text = String::new();
    let mut highlights = Vec::new();
    for span in &line.0 {
        let mut hl = HighlightStyle {
            color: Some(color_of(span.color).into()),
            ..Default::default()
        };
        if span.bold {
            hl.font_weight = Some(FontWeight::BOLD);
        }
        if span.italic {
            hl.font_style = Some(FontStyle::Italic);
        }
        if span.underline {
            hl.underline = Some(gpui::UnderlineStyle {
                thickness: gpui::px(1.),
                ..Default::default()
            });
        }
        if span.strike {
            hl.strikethrough = Some(gpui::StrikethroughStyle {
                thickness: gpui::px(1.),
                ..Default::default()
            });
        }
        let start = text.len();
        text.push_str(&span.text);
        highlights.push((start..text.len(), hl));
    }
    StyledText::new(text).with_highlights(highlights)
}

/// 标题字号（正文 13px 基准分级；h4+ 不再放大，靠粗体区分）
fn heading_size(level: u8) -> Pixels {
    match level {
        1 => px(17.),
        2 => px(15.5),
        3 => px(14.5),
        _ => px(13.5),
    }
}

/// 行 → 行容器 div（kind 驱动）：
/// - Heading(n)：提字号（粗斜下划线已在 span 上）
/// - Code：BG_CODE 面板 + 左右内边距（空行也保面板连续）
/// - Normal 空行：空串 div 高度为 0，换成一个空格撑出自然行高（段落间隔）
pub(crate) fn line_div(line: &StyledLine) -> gpui::Div {
    match line.1 {
        LineKind::Heading(level) => div()
            .text_size(heading_size(level))
            .child(styled_line(line)),
        LineKind::Code => div().bg(BG_CODE).px_2().child(styled_line(line)),
        LineKind::Normal => {
            if line.is_blank() {
                div().child(" ")
            } else {
                div().child(styled_line(line))
            }
        }
    }
}
