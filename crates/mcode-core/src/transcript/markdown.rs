//! pulldown-cmark → CC 风格样式行。
//!
//! 规则对齐 Claude Code utils/markdown.ts：
//! - h1 bold+italic+underline；h2+ bold；块间 1 空行
//! - codespan 紫（permission）；链接保文字加下划线
//! - blockquote 每行 `▎ ` dim 前缀 + italic 正文
//! - 列表缩进 2×depth；有序序号按深度 1. → a. → i.
//! - 表格管道风（列宽 max(3, 内容宽)，`-` 分隔行）；hr `---`
//! - 删除线禁用（不开 STRIKETHROUGH，`~` 保持字面）
//! - 行级语义（LineKind）：标题/代码块行打标，GUI 行容器按级提字号/上面板

use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag};

use super::style::{display_width, Color, LineKind, Span, StyledLine};

/// 渲染整段 markdown 为样式行（行尾无多余空行）。
pub fn render_markdown(text: &str) -> Vec<StyledLine> {
    let mut r = MdRenderer::new();
    r.feed(text);
    r.finish()
}

/// 渲染并去掉首尾空行（动态区流式用）。
pub fn render_markdown_trimmed(text: &str) -> Vec<StyledLine> {
    let lines = render_markdown(text);
    let first = lines.iter().position(|l| !l.is_blank());
    let last = lines.iter().rposition(|l| !l.is_blank());
    match (first, last) {
        (Some(a), Some(b)) => lines[a..=b].to_vec(),
        _ => Vec::new(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct InlineStyle {
    bold: bool,
    italic: bool,
    underline: bool,
}

impl InlineStyle {
    fn plain() -> Self {
        InlineStyle {
            bold: false,
            italic: false,
            underline: false,
        }
    }

    fn apply(self, span: &mut Span) {
        if self.bold {
            span.bold = true;
        }
        if self.italic {
            span.italic = true;
        }
        if self.underline {
            span.underline = true;
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct ListCtx {
    ordered: bool,
    counter: u64,
}

struct MdRenderer {
    lines: Vec<StyledLine>,
    cur: StyledLine,
    /// 当前行已写入可见内容
    cur_active: bool,
    style: InlineStyle,
    quote_depth: usize,
    /// 列表嵌套栈
    list_stack: Vec<ListCtx>,
    /// 代码块内（逐行原样输出）
    code_open: bool,
    /// item 标记行刚起、正文未接（松散列表的 Paragraph 不换行）
    item_fresh: bool,
}

impl MdRenderer {
    fn new() -> Self {
        MdRenderer {
            lines: Vec::new(),
            cur: StyledLine::default(),
            cur_active: false,
            style: InlineStyle::plain(),
            quote_depth: 0,
            list_stack: Vec::new(),
            code_open: false,
            item_fresh: false,
        }
    }

    /// 新起一行：写入引用前缀 + 列表缩进。
    fn start_line(&mut self) {
        self.cur = StyledLine::default();
        if self.quote_depth > 0 {
            self.cur.push(Span::dim_text("▎ ".repeat(self.quote_depth)));
        }
        let indent = 2 * self.list_stack.len().saturating_sub(1);
        if indent > 0 {
            self.cur.push(Span::new(" ".repeat(indent)));
        }
        self.cur_active = false;
    }

    fn push_span(&mut self, text: &str, color: Option<Color>) {
        if text.is_empty() {
            return;
        }
        let mut s = Span::new(text);
        s.color = color.unwrap_or(Color::Text);
        self.style.apply(&mut s);
        self.cur.push(s);
        self.cur_active = true;
    }

    /// 当前行若非空则封口。
    fn seal_line(&mut self) {
        if self.cur_active {
            self.lines.push(std::mem::take(&mut self.cur));
            self.cur_active = false;
        }
        self.start_line();
    }

    /// 块间分隔：上一行非空时补一空行。
    fn maybe_sep(&mut self) {
        if self.lines.last().is_some_and(|l| !l.is_blank()) {
            self.lines.push(StyledLine::blank());
        }
    }

    fn finish(mut self) -> Vec<StyledLine> {
        self.seal_line();
        while self.lines.last().is_some_and(StyledLine::is_blank) {
            self.lines.pop();
        }
        self.lines
    }

    fn feed(&mut self, text: &str) {
        let opts = Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS;
        let parser = Parser::new_ext(text, opts);
        let mut table: Option<TableBuf> = None;
        for ev in parser {
            match ev {
                Event::Start(tag) => match tag {
                    Tag::Heading { level, .. } => {
                        self.seal_line();
                        self.maybe_sep();
                        self.style = InlineStyle {
                            bold: true,
                            italic: level == HeadingLevel::H1,
                            underline: level == HeadingLevel::H1,
                        };
                        self.cur.1 = LineKind::Heading(level as u8);
                    }
                    Tag::BlockQuote(_) => {
                        self.seal_line();
                        self.maybe_sep();
                        self.quote_depth += 1;
                        self.style.italic = true;
                        self.start_line();
                    }
                    Tag::Paragraph => {
                        if !self.list_stack.is_empty() && self.item_fresh {
                            // 松散列表：标记行刚起，段落正文接在同一行
                            self.item_fresh = false;
                        } else {
                            self.seal_line();
                            if self.list_stack.is_empty() {
                                self.maybe_sep();
                            }
                        }
                    }
                    Tag::CodeBlock(_) => {
                        self.seal_line();
                        if self.list_stack.is_empty() {
                            self.maybe_sep();
                        }
                        self.code_open = true;
                    }
                    Tag::List(start) => {
                        self.seal_line();
                        if self.list_stack.is_empty() {
                            self.maybe_sep();
                        }
                        self.list_stack.push(ListCtx {
                            ordered: start.is_some(),
                            counter: start.unwrap_or(0),
                        });
                        self.start_line();
                    }
                    Tag::Item => {
                        self.seal_line();
                        let depth = self.list_stack.len();
                        if let Some(ctx) = self.list_stack.last_mut() {
                            // pulldown 的 start 从 1 起：先取当前号再自增
                            let marker = if ctx.ordered {
                                let m = ordered_marker(ctx.counter.max(1), depth);
                                ctx.counter += 1;
                                m
                            } else {
                                "-".to_string()
                            };
                            // 先注入缩进（start_line 由 seal 调过），再写序号
                            self.start_line();
                            self.push_span(&marker, None);
                            self.push_span(" ", None);
                            self.item_fresh = true;
                        }
                    }
                    Tag::Table(_) => {
                        self.seal_line();
                        self.maybe_sep();
                        table = Some(TableBuf::default());
                    }
                    Tag::TableHead => {
                        if let Some(t) = table.as_mut() {
                            t.in_head = true;
                        }
                    }
                    Tag::TableCell => {
                        if let Some(t) = table.as_mut() {
                            t.cur_cell = Some(StyledLine::default());
                        }
                    }
                    Tag::Emphasis => self.style.italic = true,
                    Tag::Strong => self.style.bold = true,
                    // 链接文字加下划线（URL 不展示，CC 同口径）
                    Tag::Link { .. } => self.style.underline = true,
                    _ => {}
                },
                Event::End(tag) => match tag {
                    pulldown_cmark::TagEnd::Heading(_) => {
                        self.style = InlineStyle::plain();
                        self.seal_line();
                    }
                    pulldown_cmark::TagEnd::BlockQuote(_) => {
                        self.seal_line();
                        self.quote_depth = self.quote_depth.saturating_sub(1);
                        self.style.italic = false;
                    }
                    pulldown_cmark::TagEnd::Paragraph => {
                        self.item_fresh = false;
                        self.seal_line();
                    }
                    pulldown_cmark::TagEnd::CodeBlock => {
                        self.seal_line();
                        self.code_open = false;
                    }
                    pulldown_cmark::TagEnd::List(_) => {
                        self.seal_line();
                        self.list_stack.pop();
                    }
                    pulldown_cmark::TagEnd::Item => {
                        self.seal_line();
                    }
                    pulldown_cmark::TagEnd::Table => {
                        if let Some(t) = table.take() {
                            self.seal_line();
                            self.flush_table(t);
                        }
                    }
                    pulldown_cmark::TagEnd::TableHead => {
                        if let Some(t) = table.as_mut() {
                            t.in_head = false;
                            // 表头行没有 TableRow 事件，这里自己落一行
                            let row = std::mem::take(&mut t.cur_row);
                            t.rows.push(row);
                        }
                    }
                    pulldown_cmark::TagEnd::TableRow => {
                        if let Some(t) = table.as_mut() {
                            let row = std::mem::take(&mut t.cur_row);
                            t.rows.push(row);
                        }
                    }
                    pulldown_cmark::TagEnd::TableCell => {
                        if let Some(t) = table.as_mut() {
                            if let Some(cell) = t.cur_cell.take() {
                                t.cur_row.push(cell);
                            }
                        }
                    }
                    pulldown_cmark::TagEnd::Emphasis => self.style.italic = false,
                    pulldown_cmark::TagEnd::Strong => self.style.bold = false,
                    pulldown_cmark::TagEnd::Link => self.style.underline = false,
                    _ => {}
                },
                Event::Text(t) => {
                    if self.code_open {
                        for line in t.lines() {
                            self.start_line();
                            self.cur.1 = LineKind::Code;
                            if line.is_empty() {
                                // 块内空行保留（GUI 代码面板连续；CLI paint 为空串，视觉不变）
                                self.lines.push(std::mem::take(&mut self.cur));
                            } else {
                                self.push_span(line, None);
                                self.seal_line();
                            }
                        }
                    } else if let Some(tb) = table.as_mut() {
                        if let Some(cell) = tb.cur_cell.as_mut() {
                            let bold = tb.in_head;
                            let mut s = Span::new(t.to_string());
                            if bold {
                                s.bold = true;
                            }
                            cell.push(s);
                        }
                    } else {
                        self.push_span(&t, None);
                    }
                }
                Event::Code(c) => {
                    // 表格单元格内的 code span 必须进 cell 缓冲：走 push_span
                    // 会落到当前行缓冲，跨行/跨格累积成 index.htmlgame.js…
                    // 黏连行（模型表格首列全是 code span 时必现），且原格失字
                    if let Some(cell) = table.as_mut().and_then(|tb| tb.cur_cell.as_mut()) {
                        cell.push(Span::colored(Color::Permission, c.to_string()));
                    } else {
                        self.push_span(&c, Some(Color::Permission));
                    }
                }
                Event::SoftBreak | Event::HardBreak => {
                    self.seal_line();
                }
                Event::Rule => {
                    self.seal_line();
                    self.maybe_sep();
                    self.push_span("---", None);
                    self.seal_line();
                }
                Event::TaskListMarker(done) => {
                    self.push_span(if done { "[x] " } else { "[ ] " }, None);
                }
                Event::InlineHtml(h) => {
                    if !self.code_open {
                        self.push_span(h.trim(), None);
                    }
                }
                Event::Html(_) => {}
                _ => {}
            }
        }
    }

    fn flush_table(&mut self, t: TableBuf) {
        if t.rows.is_empty() {
            return;
        }
        let ncols = t.rows.iter().map(|r| r.len()).max().unwrap_or(0);
        if ncols == 0 {
            return;
        }
        let mut widths = vec![3usize; ncols];
        for row in &t.rows {
            for (i, cell) in row.iter().enumerate() {
                widths[i] = widths[i].max(display_width(&cell.text()).max(3));
            }
        }
        let render_row = |row: &[StyledLine]| -> StyledLine {
            let mut line = StyledLine::default();
            line.push(Span::dim_text("|"));
            for (i, w) in widths.iter().enumerate() {
                let cell = row.get(i).map(|c| c.text()).unwrap_or_default();
                let pad = w.saturating_sub(display_width(&cell));
                line.push(Span::dim_text(" "));
                if row
                    .get(i)
                    .is_some_and(|c| c.0.first().is_some_and(|s| s.bold))
                {
                    line.push(Span::new(cell).bold());
                } else {
                    line.push(Span::new(cell));
                }
                line.push(Span::dim_text(" ".repeat(pad + 1) + "|"));
            }
            line
        };
        self.maybe_sep();
        self.lines.push(render_row(t.rows.first().unwrap()));
        let sep: Vec<Span> = std::iter::once(Span::dim_text("|"))
            .chain(widths.iter().flat_map(|w| {
                [
                    Span::dim_text(" "),
                    Span::dim_text("-".repeat(*w)),
                    Span::dim_text(" |"),
                ]
            }))
            .collect();
        self.lines.push(StyledLine(sep, LineKind::Normal));
        for row in &t.rows[1..] {
            self.lines.push(render_row(row));
        }
        self.start_line();
    }
}

/// 有序列表序号：深度 1 → "1."，深度 2 → "a."，深度 3+ → 罗马数字。
fn ordered_marker(n: u64, depth: usize) -> String {
    match depth {
        1 => format!("{n}."),
        2 => {
            let mut s = String::new();
            let mut n = n;
            while n > 26 {
                s.push('z');
                n -= 26;
            }
            let c = char::from_u32(b'a' as u32 + (n as u32).saturating_sub(1)).unwrap_or('a');
            s.push(c);
            format!("{s}.")
        }
        _ => format!("{}.", roman(n)),
    }
}

fn roman(mut n: u64) -> String {
    const PAIRS: [(u64, &str); 13] = [
        (1000, "m"),
        (900, "cm"),
        (500, "d"),
        (400, "cd"),
        (100, "c"),
        (90, "xc"),
        (50, "l"),
        (40, "xl"),
        (10, "x"),
        (9, "ix"),
        (5, "v"),
        (4, "iv"),
        (1, "i"),
    ];
    let mut out = String::new();
    for (v, sym) in PAIRS {
        while n >= v {
            out.push_str(sym);
            n -= v;
        }
    }
    out
}

#[derive(Debug, Default)]
struct TableBuf {
    rows: Vec<Vec<StyledLine>>,
    cur_row: Vec<StyledLine>,
    cur_cell: Option<StyledLine>,
    in_head: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(lines: &[StyledLine]) -> Vec<String> {
        lines.iter().map(|l| l.text()).collect()
    }

    #[test]
    fn heading_styles_and_separators() {
        let lines = render_markdown("# 标题\n\n正文一段");
        assert_eq!(texts(&lines), vec!["标题", "", "正文一段"]);
        let h1 = &lines[0].0[0];
        assert!(
            h1.bold && h1.italic && h1.underline,
            "h1 = bold+italic+underline"
        );
        let h2 = render_markdown("## 二级\n\nbody");
        assert!(h2[0].0[0].bold && !h2[0].0[0].italic, "h2 只有 bold");
    }

    #[test]
    fn codespan_is_purple() {
        let lines = render_markdown("用 `cargo test` 跑测试");
        let code = lines[0]
            .0
            .iter()
            .find(|s| s.text == "cargo test")
            .expect("codespan span 存在");
        assert_eq!(code.color, Color::Permission);
    }

    #[test]
    fn codespan_multiline_normalizes_newlines_to_spaces() {
        // CommonMark：code span 内的换行归一化为空格（pulldown 已做），
        // 渲染层不得再见到内嵌 \n
        let lines = render_markdown("主要子项目：`index.html\ngame.js`");
        assert_eq!(lines.len(), 1, "归一化后单行");
        assert_eq!(lines[0].text(), "主要子项目：index.html game.js");
        assert!(lines[0].text().matches('\n').count() == 0);
    }

    #[test]
    fn table_cells_with_codespans_keep_text_in_place() {
        // 回归（线上截图）：首列全是 code span 的表格，Code 事件走
        // push_span 落进当前行缓冲 → 黏连成 index.htmlgame.js… 且原格失字
        let lines = render_markdown(
            "**主要子项目：**\n| 目录/文件 | 内容 |\n|---|---|\n| `index.html` + `game.js` + `three.min.js` | 一个 Three.js 3D 游戏 |\n| `tetris3d/` | 3D 俄罗斯方块 |\n",
        );
        let joined: Vec<String> = texts(&lines);
        assert!(
            !joined.iter().any(|t| t.contains("index.htmlgame.js")),
            "code span 不得黏连成一行: {joined:?}"
        );
        let row1 = joined
            .iter()
            .find(|t| t.contains("Three.js"))
            .expect("表格行存在");
        assert!(
            row1.contains("index.html") && row1.contains("game.js"),
            "首列 code span 落在表格行内: {row1}"
        );
        assert!(row1.starts_with("| "), "仍是管道表格行");
    }

    #[test]
    fn blockquote_prefixes_and_italic() {
        let lines = render_markdown("> 引用行\n> 第二行");
        assert_eq!(texts(&lines), vec!["▎ 引用行", "▎ 第二行"]);
        let body = lines[0].0.iter().find(|s| s.text == "引用行").unwrap();
        assert!(body.italic, "引用正文 italic");
        assert_eq!(lines[0].0[0].color, Color::Dim, "▎ 前缀 dim");
    }

    #[test]
    fn list_markers_and_indent_by_depth() {
        let lines = render_markdown("- 一\n  - 二\n    - 三\n- 二个");
        assert_eq!(texts(&lines), vec!["- 一", "  - 二", "    - 三", "- 二个"]);
        let ordered = render_markdown("1. 甲\n2. 乙\n   1. 子\n   2. 丑\n       1. 寅");
        assert_eq!(
            texts(&ordered),
            vec!["1. 甲", "2. 乙", "  a. 子", "  b. 丑", "    i. 寅"],
            "深度序号 1. → a. → i."
        );
    }

    #[test]
    fn loose_list_no_blank_between_items() {
        let lines = render_markdown("- 甲\n\n- 乙");
        assert_eq!(
            texts(&lines),
            vec!["- 甲", "- 乙"],
            "松散列表条目间不插空行"
        );
    }

    #[test]
    fn table_pipe_render_with_min_width() {
        let lines = render_markdown("| a | bbb |\n|---|---|\n| 1 | 22 |");
        assert_eq!(
            texts(&lines),
            vec!["| a   | bbb |", "| --- | --- |", "| 1   | 22  |"]
        );
    }

    #[test]
    fn code_block_lines_verbatim() {
        let lines = render_markdown("```\nfn a() {}\nlet b = 1;\n```");
        assert_eq!(texts(&lines), vec!["fn a() {}", "let b = 1;"]);
        assert!(
            lines.iter().all(|l| l.1 == LineKind::Code),
            "代码块行全部打 Code 标"
        );
    }

    #[test]
    fn code_block_keeps_inner_blank_lines_with_kind() {
        let lines = render_markdown("```\na\n\nb\n```");
        assert_eq!(texts(&lines), vec!["a", "", "b"], "块内空行不吞");
        assert!(lines.iter().all(|l| l.1 == LineKind::Code));
        assert_eq!(lines[1].0.len(), 0, "空行无 span");
    }

    #[test]
    fn heading_lines_carry_level_kind() {
        let lines = render_markdown("# 一\n\n## 二\n\n正文");
        assert_eq!(lines[0].1, LineKind::Heading(1));
        assert_eq!(lines[2].1, LineKind::Heading(2));
        assert_eq!(lines[4].1, LineKind::Normal);
    }

    #[test]
    fn link_text_gets_underline() {
        let lines = render_markdown("见 [文档](https://example.com) 说明");
        let body = lines[0]
            .0
            .iter()
            .find(|s| s.text == "文档")
            .expect("链接文字 span 存在");
        assert!(body.underline, "链接文字带下划线（URL 不展示）");
    }

    #[test]
    fn strikethrough_stays_literal() {
        let lines = render_markdown("大约 ~100 个");
        assert_eq!(texts(&lines), vec!["大约 ~100 个"]);
    }

    #[test]
    fn rule_and_tasklist() {
        assert_eq!(
            texts(&render_markdown("a\n\n---\n\nb")),
            vec!["a", "", "---", "", "b"]
        );
        let tl = render_markdown("- [x] 完成\n- [ ] 待办");
        assert_eq!(texts(&tl), vec!["- [x] 完成", "- [ ] 待办"]);
    }
}
