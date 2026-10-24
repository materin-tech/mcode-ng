// MCode_Desktop — AI coding agent desktop app
// Copyright (c) 2026 Materin-tech. All rights reserved.
// SPDX-License-Identifier: MIT

//! transcript 呈现桥 — mcode_core::transcript 的 StyledLine（CLI 共享渲染核）
//! → gpui StyledText。颜色映射 = CLI 主题 token → Desktop 色板；
//! 粗体/斜体/下划线/删除线逐 span 落到 HighlightStyle，gpui 容器自行折行。

use gpui::{FontStyle, FontWeight, HighlightStyle, StyledText};
use mcode_core::transcript::{Color, StyledLine};

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
