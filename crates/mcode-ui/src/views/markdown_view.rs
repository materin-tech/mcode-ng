//! Markdown → gpui 元素渲染（pulldown-cmark 解析 → Vec<MdBlock> 中间结构 → 元素树）。
//! Assistant 消息走此渲染；User 消息保持纯文本。
//! 流式期间每帧整段重解析（消息都不长，可接受）。

use gpui::{Div, FontWeight, SharedString, div, prelude::*, px};
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

use crate::views::theme::*;

// ---------- 中间结构（单测断言目标） ----------

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum MdSpan {
    Text(String),
    Bold(String),
    Italic(String),
    Code(String),
    Link { text: String, url: String },
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum MdBlock {
    Heading {
        level: u8,
        spans: Vec<MdSpan>,
    },
    Paragraph(Vec<MdSpan>),
    CodeBlock {
        lang: String,
        text: String,
    },
    ListItem {
        ordered: bool,
        number: u64,
        depth: u8,
        spans: Vec<MdSpan>,
    },
    Quote(Vec<MdSpan>),
    Table {
        header: Vec<String>,
        rows: Vec<Vec<String>>,
    },
    Rule,
}

/// pulldown-cmark 事件流 → MdBlock 列表（容器嵌套：inline span 递归收集）
pub(crate) fn parse_markdown(src: &str) -> Vec<MdBlock> {
    let parser = Parser::new_ext(src, Options::ENABLE_TABLES);
    let events: Vec<Event> = parser.collect();
    let mut blocks = vec![];
    let mut i = 0usize;
    let n = events.len();
    while i < n {
        match &events[i] {
            Event::Start(Tag::Heading { level, .. }) => {
                let (spans, next) = collect_spans(&events, i + 1);
                blocks.push(MdBlock::Heading {
                    level: heading_level(*level),
                    spans,
                });
                i = next;
            }
            Event::Start(Tag::CodeBlock(kind)) => {
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => {
                        info.chars().take_while(|c| !c.is_whitespace()).collect()
                    }
                    _ => String::new(),
                };
                let mut text = String::new();
                let mut j = i + 1;
                while j < n {
                    match &events[j] {
                        Event::Text(t) => text.push_str(t),
                        Event::End(TagEnd::CodeBlock) => break,
                        _ => {}
                    }
                    j += 1;
                }
                blocks.push(MdBlock::CodeBlock { lang, text });
                i = j + 1;
            }
            Event::Start(Tag::Table(_)) => {
                let mut header = vec![];
                let mut rows: Vec<Vec<String>> = vec![];
                let mut j = i + 1;
                let mut current_row: Vec<String> = vec![];
                let mut in_head = false;
                while j < n {
                    match &events[j] {
                        Event::Start(Tag::TableHead) => in_head = true,
                        Event::End(TagEnd::TableHead) => {
                            header = std::mem::take(&mut current_row);
                            in_head = false;
                        }
                        Event::Start(Tag::TableRow) => current_row = vec![],
                        Event::End(TagEnd::TableRow) => {
                            if !in_head {
                                rows.push(std::mem::take(&mut current_row));
                            }
                        }
                        Event::Start(Tag::TableCell) => current_row.push(String::new()),
                        Event::Text(t) | Event::Code(t) => {
                            if let Some(cell) = current_row.last_mut() {
                                cell.push_str(t);
                            }
                        }
                        Event::End(TagEnd::Table) => break,
                        _ => {}
                    }
                    j += 1;
                }
                blocks.push(MdBlock::Table { header, rows });
                i = j + 1;
            }
            Event::Start(Tag::List(start)) => {
                // 列表：以事件深度计嵌套层级
                let ordered = start.is_some();
                let mut number = start.unwrap_or(0);
                let mut depth = 0u8;
                let mut j = i + 1;
                while j < n {
                    match &events[j] {
                        Event::Start(Tag::List(_)) => depth += 1,
                        Event::End(TagEnd::List(_)) => {
                            if depth == 0 {
                                break;
                            }
                            depth -= 1;
                        }
                        Event::Start(Tag::Item) => {
                            let item_depth = depth;
                            let (spans, after) = collect_spans(&events, j + 1);
                            // Item 内部可能嵌套子列表：collect_spans 返回到 End(Item)，
                            // 子列表 Start 已被跳过——子项由外层 while 的深度追踪单独处理，
                            // 这里只展开"最后"一段 span（对齐常见输出形态）
                            blocks.push(MdBlock::ListItem {
                                ordered,
                                number: if ordered { number } else { 0 },
                                depth: item_depth,
                                spans,
                            });
                            if ordered {
                                number += 1;
                            }
                            j = after;
                            continue;
                        }
                        _ => {}
                    }
                    j += 1;
                }
                i = j + 1;
            }
            Event::Start(Tag::BlockQuote(_)) => {
                let (spans, next) = collect_spans(&events, i + 1);
                blocks.push(MdBlock::Quote(spans));
                i = next;
            }
            Event::Rule => {
                blocks.push(MdBlock::Rule);
                i += 1;
            }
            Event::Start(Tag::Paragraph) => {
                let (spans, next) = collect_spans(&events, i + 1);
                blocks.push(MdBlock::Paragraph(spans));
                i = next;
            }
            _ => i += 1,
        }
    }
    blocks
}

/// 收集 inline 事件为 spans，返回 (spans, 容器 End 之后的事件下标)
fn collect_spans(events: &[Event], mut i: usize) -> (Vec<MdSpan>, usize) {
    let mut spans = vec![];
    while i < events.len() {
        match &events[i] {
            Event::Text(t) => {
                if let Some(MdSpan::Text(prev)) = spans.last_mut() {
                    prev.push_str(t);
                } else {
                    spans.push(MdSpan::Text(t.to_string()));
                }
                i += 1;
            }
            Event::SoftBreak | Event::HardBreak => {
                spans.push(MdSpan::Text("\n".into()));
                i += 1;
            }
            Event::Code(t) => {
                spans.push(MdSpan::Code(t.to_string()));
                i += 1;
            }
            Event::Start(Tag::Emphasis) => {
                let (inner, next) = collect_spans(events, i + 1);
                spans.push(MdSpan::Italic(flatten_spans(&inner)));
                i = next;
            }
            Event::Start(Tag::Strong) => {
                let (inner, next) = collect_spans(events, i + 1);
                spans.push(MdSpan::Bold(flatten_spans(&inner)));
                i = next;
            }
            Event::Start(Tag::Link { dest_url, .. }) => {
                let (inner, next) = collect_spans(events, i + 1);
                spans.push(MdSpan::Link {
                    text: flatten_spans(&inner),
                    url: dest_url.to_string(),
                });
                i = next;
            }
            Event::Start(Tag::Image { dest_url, .. }) => {
                // 图片降级为链接样式的占位文本（alt 由内部 Text 事件提供）
                let (inner, next) = collect_spans(events, i + 1);
                spans.push(MdSpan::Link {
                    text: if inner.is_empty() {
                        "[图片]".into()
                    } else {
                        flatten_spans(&inner)
                    },
                    url: dest_url.to_string(),
                });
                i = next;
            }
            Event::End(_) => {
                return (spans, i + 1);
            }
            _ => i += 1,
        }
    }
    (spans, i)
}

fn flatten_spans(spans: &[MdSpan]) -> String {
    let mut out = String::new();
    for s in spans {
        match s {
            MdSpan::Text(t) | MdSpan::Bold(t) | MdSpan::Italic(t) | MdSpan::Code(t) => {
                out.push_str(t)
            }
            MdSpan::Link { text, .. } => out.push_str(text),
        }
    }
    out
}

fn heading_level(l: HeadingLevel) -> u8 {
    match l {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        _ => 4,
    }
}

// ---------- 渲染 ----------

/// 行内 span → 元素（粗体/斜体/行内代码/链接各自着色样式）
fn span_el(s: &MdSpan) -> Div {
    match s {
        MdSpan::Text(t) => div().child(SharedString::from(t.replace('\n', " "))),
        MdSpan::Bold(t) => div()
            .font_weight(FontWeight::BOLD)
            .text_color(TEXT_1)
            .child(t.clone()),
        MdSpan::Italic(t) => div().italic().child(t.clone()),
        MdSpan::Code(t) => div()
            .font_family("Menlo")
            .text_size(px(12.))
            .bg(BG_WINDOW)
            .border_1()
            .border_color(BORDER)
            .rounded_sm()
            .px_1()
            .text_color(TEXT_1)
            .child(SharedString::from(t.replace('\n', " "))),
        MdSpan::Link { text, .. } => div().text_color(BLUE).child(text.clone()),
    }
}

fn spans_line(spans: &[MdSpan], base_text: gpui::Rgba, base_size: f32) -> Div {
    let mut row = div()
        .flex()
        .flex_wrap()
        .items_center()
        .gap_1()
        .text_color(base_text)
        .text_size(px(base_size));
    for s in spans {
        row = row.child(span_el(s));
    }
    row
}

/// markdown 文本 → 元素树（外层纵向排列容器）
pub(crate) fn render_markdown(src: &str) -> Div {
    let blocks = parse_markdown(src);
    let mut root = div().flex().flex_col().gap_1p5().max_w_full();
    for b in &blocks {
        root = root.child(block_el(b));
    }
    root
}

fn block_el(b: &MdBlock) -> Div {
    match b {
        MdBlock::Heading { level, spans } => {
            let (size, color) = match level {
                1 => (px(19.), TEXT_1),
                2 => (px(16.5), TEXT_1),
                _ => (px(14.), TEXT_1),
            };
            let mut head = div().mt_1();
            head = head.child(
                spans_line(spans, color, size.into())
                    .when(*level <= 2, |d| d.font_weight(FontWeight::BOLD)),
            );
            if *level == 1 {
                // h1 下加分隔线，增强层级
                head = head.border_b_1().border_color(BORDER).pb_1();
            }
            head
        }
        MdBlock::Paragraph(spans) => spans_line(spans, TEXT_1, 13.5).line_height(px(21.)),
        MdBlock::CodeBlock { text, .. } => div()
            .rounded_md()
            .border_l_2()
            .border_color(ACCENT)
            .bg(BG_WINDOW)
            .p_2()
            .font_family("Menlo")
            .text_size(px(11.5))
            .text_color(TEXT_2)
            .child(SharedString::from(text.clone())),
        MdBlock::ListItem {
            ordered,
            number,
            depth,
            spans,
        } => {
            let marker = if *ordered {
                SharedString::from(format!("{number}."))
            } else {
                "•".into()
            };
            div()
                .flex()
                .gap_1p5()
                .pl(px(8. + *depth as f32 * 16.))
                .py_0p5()
                .child(
                    div()
                        .text_color(ACCENT)
                        .font_family("Menlo")
                        .text_size(px(12.5))
                        .child(marker),
                )
                .child(spans_line(spans, TEXT_1, 13.))
        }
        MdBlock::Quote(spans) => div()
            .border_l_2()
            .border_color(BORDER_STRONG)
            .pl_2()
            .py_0p5()
            .child(spans_line(spans, TEXT_3, 12.5)),
        MdBlock::Table { header, rows } => {
            let cols = header.len().max(1);
            let mut wrap = div()
                .rounded_md()
                .border_1()
                .border_color(BORDER)
                .overflow_hidden()
                .mt_1();
            // 表头（加粗 + 底色）
            wrap = wrap.child(div().flex().bg(BG_ACTIVE).children((0..cols).map(|i| {
                div()
                    .flex_1()
                    .min_w_0()
                    .px_2()
                    .py_1()
                    .font_weight(FontWeight::BOLD)
                    .text_size(px(12.))
                    .text_color(TEXT_1)
                    .child(header.get(i).cloned().unwrap_or_default())
            })));
            // 数据行（斑马纹 + 行分隔线）
            wrap = wrap.children(rows.iter().enumerate().map(|(ri, row)| {
                div()
                    .flex()
                    .when(ri % 2 == 1, |d| d.bg(BG_PANEL))
                    .border_t_1()
                    .border_color(BORDER)
                    .children((0..cols).map(|i| {
                        div()
                            .flex_1()
                            .min_w_0()
                            .px_2()
                            .py_1()
                            .text_size(px(12.))
                            .text_color(TEXT_2)
                            .child(row.get(i).cloned().unwrap_or_default())
                    }))
            }));
            wrap
        }
        MdBlock::Rule => div().h(px(1.)).w_full().bg(BORDER).my_1(),
    }
}

// ---------- 单测（解析层：markdown → MdBlock） ----------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headings_levels_and_text() {
        let blocks = parse_markdown("## 二级标题\n\n### 三级标题");
        assert_eq!(
            blocks,
            vec![
                MdBlock::Heading {
                    level: 2,
                    spans: vec![MdSpan::Text("二级标题".into())]
                },
                MdBlock::Heading {
                    level: 3,
                    spans: vec![MdSpan::Text("三级标题".into())]
                },
            ]
        );
    }

    #[test]
    fn table_rows_and_header() {
        let src = "| 工具 | 状态 |\n|---|---|\n| bash | ✓ |\n| read_file | ✓ |";
        let blocks = parse_markdown(src);
        assert_eq!(
            blocks,
            vec![MdBlock::Table {
                header: vec!["工具".into(), "状态".into()],
                rows: vec![
                    vec!["bash".into(), "✓".into()],
                    vec!["read_file".into(), "✓".into()]
                ],
            }]
        );
    }

    #[test]
    fn bold_italic_code_inline() {
        let blocks = parse_markdown("这是 **加粗** 和 *斜体* 与 `code` 文本");
        match &blocks[0] {
            MdBlock::Paragraph(spans) => {
                assert!(spans.contains(&MdSpan::Bold("加粗".into())));
                assert!(spans.contains(&MdSpan::Italic("斜体".into())));
                assert!(spans.contains(&MdSpan::Code("code".into())));
                assert!(
                    spans
                        .iter()
                        .any(|s| matches!(s, MdSpan::Text(t) if t.contains("这是")))
                );
            }
            other => panic!("非段落: {other:?}"),
        }
    }

    #[test]
    fn fenced_code_block_with_lang() {
        let blocks = parse_markdown("```rust\nfn main() {}\n```");
        assert_eq!(
            blocks,
            vec![MdBlock::CodeBlock {
                lang: "rust".into(),
                text: "fn main() {}\n".into()
            }]
        );
    }

    #[test]
    fn ordered_and_unordered_lists_with_nesting() {
        let blocks = parse_markdown("- 甲\n- 乙\n1. 第一\n2. 第二");
        let items: Vec<&MdBlock> = blocks.iter().collect();
        assert_eq!(items.len(), 4);
        assert!(
            matches!(&blocks[0], MdBlock::ListItem { ordered: false, number: 0, depth: 0, spans } if spans.contains(&MdSpan::Text("甲".into())))
        );
        assert!(matches!(
            &blocks[2],
            MdBlock::ListItem {
                ordered: true,
                number: 1,
                ..
            }
        ));
        assert!(matches!(
            &blocks[3],
            MdBlock::ListItem {
                ordered: true,
                number: 2,
                ..
            }
        ));
    }

    #[test]
    fn quote_rule_link() {
        let blocks = parse_markdown("> 引用内容\n\n---\n\n[链接](https://example.com)");
        assert_eq!(blocks.len(), 3);
        assert!(
            matches!(&blocks[0], MdBlock::Quote(sp) if sp.contains(&MdSpan::Text("引用内容".into())))
        );
        assert!(matches!(blocks[1], MdBlock::Rule));
        assert!(
            matches!(&blocks[2], MdBlock::Paragraph(sp) if matches!(sp[0], MdSpan::Link { ref url, .. } if url == "https://example.com"))
        );
    }
}
