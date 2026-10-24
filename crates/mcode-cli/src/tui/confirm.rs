//! 权限对话框：CC 的紫色 round 上半框 + 三选项（Yes / Yes-and-don't-ask / No）。
//!
//! 状态机独立于终端 IO；`on_key` 返回 `Some(Decision)` 即对话框闭合。

use mcode_core::permission::Decision;

use super::style::{display_width, Color, Span, StyledLine};

/// 选项文案（对齐 CC）。
pub const OPTIONS: [&str; 3] = ["Yes", "Yes, and don't ask again", "No"];

#[derive(Debug)]
pub struct ConfirmDialog {
    pub tool: String,
    pub args_summary: String,
    pub selected: usize,
}

impl ConfirmDialog {
    pub fn new(tool: impl Into<String>, args_summary: impl Into<String>) -> Self {
        ConfirmDialog {
            tool: tool.into(),
            args_summary: args_summary.into(),
            selected: 0,
        }
    }

    /// 对话框标题（CC 每工具定制）。
    pub fn title(&self) -> String {
        match self.tool.as_str() {
            "bash" => "Bash command".into(),
            "edit_file" => "Edit file".into(),
            "write_file" => "Write file".into(),
            "read_file" => "Read file".into(),
            "task" => "Run agent".into(),
            other => format!("{other} permission"),
        }
    }

    /// 按键处理；`Some` = 已选定并闭合。
    pub fn on_key(&mut self, key: super::input::KeyKind) -> Option<Decision> {
        use super::input::KeyKind::*;
        match key {
            Up | ShiftTab => {
                self.selected = (self.selected + OPTIONS.len() - 1) % OPTIONS.len();
                None
            }
            Down | Tab => {
                self.selected = (self.selected + 1) % OPTIONS.len();
                None
            }
            Char('1') => Some(self.pick(0)),
            Char('2') => Some(self.pick(1)),
            Char('3') => Some(self.pick(2)),
            Char('y') => Some(self.pick(0)),
            Char('a') => Some(self.pick(1)),
            Char('n') | Esc => Some(Decision::Deny),
            Enter => Some(self.pick(self.selected)),
            _ => None,
        }
    }

    fn pick(&self, i: usize) -> Decision {
        match i {
            1 => Decision::AllowAlways,
            2 => Decision::Deny,
            _ => Decision::Allow,
        }
    }

    /// 渲染（无底边的 round 框；宽度 = 内容 + padding 1 + 边 1）。
    pub fn render(&self) -> Vec<StyledLine> {
        let title = self.title();
        let mut content: Vec<(String, Color, bool)> = Vec::new();
        for l in self.args_summary.lines() {
            content.push((l.to_string(), Color::Text, false));
        }
        content.push(("Do you want to proceed?".into(), Color::Dim, false));
        let mut menu: Vec<StyledLine> = Vec::new();
        for (i, opt) in OPTIONS.iter().enumerate() {
            let mut line = StyledLine::default();
            if i == self.selected {
                line.push(Span::colored(Color::Suggestion, "❯ "));
                line.push(Span::colored(
                    Color::Suggestion,
                    format!("{}. {}", i + 1, opt),
                ));
            } else {
                line.push(Span::new("  "));
                line.push(Span::dim_text(format!("{}. {}", i + 1, opt)));
            }
            menu.push(line);
        }
        let footer = Span::dim_text("Esc to cancel");
        // 宽度：内容最大宽 + 2（paddingX 1）
        let mut inner = display_width(&title);
        for (t, _, _) in &content {
            inner = inner.max(display_width(t) + 2);
        }
        for l in &menu {
            inner = inner.max(display_width(&l.text()) + 2);
        }
        inner = inner.max(display_width(&footer.text) + 2).max(20);

        let mut out: Vec<StyledLine> = Vec::new();
        out.push(StyledLine::new(vec![Span::colored(
            Color::Permission,
            format!("╭{}╮", "─".repeat(inner)),
        )]));
        out.push(box_line(
            &[Span::colored(Color::Permission, title).bold()],
            inner,
        ));
        out.push(box_line(&[], inner));
        for (t, c, _b) in &content {
            out.push(box_line(&[Span::colored(*c, format!(" {t}"))], inner));
        }
        for l in &menu {
            let mut spans = vec![Span::new(" ")];
            spans.extend(l.0.iter().cloned());
            out.push(box_line(&spans, inner));
        }
        out.push(box_line(&[], inner));
        out.push(box_line(&[footer], inner));
        out.push(StyledLine::new(vec![Span::colored(
            Color::Permission,
            format!("╰{}╯", "─".repeat(inner)),
        )]));
        out
    }
}

/// `│ {spans}` 补齐到 inner 宽并封右缘——CC 的权限对话框是闭合矩形。
fn box_line(spans: &[Span], inner: usize) -> StyledLine {
    let mut line = StyledLine::new(vec![Span::colored(Color::Permission, "│")]);
    let used: usize = spans.iter().map(|s| display_width(&s.text)).sum();
    line.0.extend_from_slice(spans);
    if inner > used {
        line.push(Span::new(" ".repeat(inner - used)));
    }
    line.push(Span::colored(Color::Permission, "│"));
    line
}

#[cfg(test)]
mod tests {
    use super::super::input::KeyKind;
    use super::*;

    #[test]
    fn number_keys_pick_decisions_directly() {
        let mut d = ConfirmDialog::new("bash", "cargo test");
        assert_eq!(d.on_key(KeyKind::Char('3')), Some(Decision::Deny));
        let mut d = ConfirmDialog::new("bash", "cargo test");
        assert_eq!(d.on_key(KeyKind::Char('2')), Some(Decision::AllowAlways));
        let mut d = ConfirmDialog::new("bash", "cargo test");
        assert_eq!(d.on_key(KeyKind::Char('1')), Some(Decision::Allow));
    }

    #[test]
    fn y_a_n_and_esc_shortcuts() {
        let mut d = ConfirmDialog::new("edit_file", "x");
        assert_eq!(d.on_key(KeyKind::Char('y')), Some(Decision::Allow));
        let mut d = ConfirmDialog::new("edit_file", "x");
        assert_eq!(d.on_key(KeyKind::Char('a')), Some(Decision::AllowAlways));
        let mut d = ConfirmDialog::new("edit_file", "x");
        assert_eq!(d.on_key(KeyKind::Char('n')), Some(Decision::Deny));
        let mut d = ConfirmDialog::new("edit_file", "x");
        assert_eq!(d.on_key(KeyKind::Esc), Some(Decision::Deny), "Esc = 拒绝");
    }

    #[test]
    fn navigation_cycles_and_enter_confirms() {
        let mut d = ConfirmDialog::new("bash", "x");
        assert_eq!(d.on_key(KeyKind::Down), None);
        assert_eq!(d.selected, 1);
        assert_eq!(d.on_key(KeyKind::Up), None);
        assert_eq!(d.selected, 0);
        assert_eq!(d.on_key(KeyKind::Up), None);
        assert_eq!(d.selected, 2, "循环到末尾");
        assert_eq!(d.on_key(KeyKind::Enter), Some(Decision::Deny));
    }

    #[test]
    fn titles_map_per_tool() {
        assert_eq!(ConfirmDialog::new("bash", "").title(), "Bash command");
        assert_eq!(ConfirmDialog::new("edit_file", "").title(), "Edit file");
        assert_eq!(ConfirmDialog::new("write_file", "").title(), "Write file");
        assert_eq!(ConfirmDialog::new("mcp_x", "").title(), "mcp_x permission");
    }

    #[test]
    fn render_has_top_border_options_and_cancel_hint() {
        let mut d = ConfirmDialog::new("bash", "npm run build");
        d.selected = 1;
        let lines = d.render();
        let texts: Vec<String> = lines.iter().map(|l| l.text()).collect();
        assert!(texts[0].starts_with("╭"), "{:?}", texts[0]);
        assert!(texts[0].ends_with('╮'));
        let last = texts.last().unwrap();
        assert!(last.starts_with('╰') && last.ends_with('╯'), "{last:?}");
        // 内容行左右缘封闭
        let title_line = texts.iter().find(|t| t.contains("Bash command")).unwrap();
        assert!(title_line.starts_with('│') && title_line.ends_with('│'));
        assert!(texts.iter().any(|t| t.contains("Bash command")));
        assert!(texts.iter().any(|t| t.contains("npm run build")));
        assert!(texts.iter().any(|t| t.contains("Do you want to proceed?")));
        assert!(texts
            .iter()
            .any(|t| t.contains("❯ 2. Yes, and don't ask again")));
        assert!(
            texts
                .iter()
                .any(|t| t.contains("1. Yes") && !t.contains("❯")),
            "未选中项不带 ❯ 标记"
        );
        assert!(texts.iter().any(|t| t.contains("Esc to cancel")));
        // 焦点行青色
        let focus = lines.iter().find(|l| l.text().contains("❯ 2.")).unwrap();
        assert!(
            focus
                .0
                .iter()
                .any(|sp| sp.text == "❯ " && sp.color == Color::Suggestion),
            "焦点行 ❯ 与选项青色"
        );
        // 边框紫
        assert_eq!(lines[0].0[0].color, Color::Permission);
    }
}
