//! 样式基元：颜色 token、带样式的 Span/Line、ANSI 序列生成、宽度感知折行。
//!
//! 对齐 Claude Code 的主题 token（text/dim/suggestion/success/error/warning/
//! permission/bashBorder/planMode/autoAccept/claude）；颜色开关由 CLI 侧按
//! `NO_COLOR`/tty 初始化。纯函数可测：渲染断言基于 Span，不碰真实终端。

/// 语义颜色 token（名字对齐 CC 主题系统）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Color {
    /// 正文（无色 = 终端默认前景）
    Text,
    /// 次要信息（ANSI dim）
    Dim,
    /// 焦点/选中/建议（青色）
    Suggestion,
    /// 成功/选中项（绿色）
    Success,
    /// 错误（红色）
    Error,
    /// 警告（黄色）
    Warning,
    /// 权限边框/行内代码（紫色）
    Permission,
    /// bash 模式（橙色）
    BashBorder,
    /// plan 模式指示（青）
    PlanMode,
    /// accept-edits 模式指示（黄）
    AutoAccept,
    /// 品牌色（CC 的 claude 橙，256 色 173）
    Claude,
}

impl Color {
    /// SGR 前景序列（None = 不发颜色码）。
    pub fn sgr(self) -> Option<&'static str> {
        match self {
            Color::Text => None,
            Color::Dim => Some("2"),
            Color::Suggestion => Some("36"),
            Color::Success => Some("32"),
            Color::Error => Some("31"),
            Color::Warning => Some("33"),
            Color::Permission => Some("35"),
            Color::BashBorder => Some("38;5;208"),
            Color::PlanMode => Some("36"),
            Color::AutoAccept => Some("33"),
            Color::Claude => Some("38;5;173"),
        }
    }
}

/// 文本样式开关（NO_COLOR / 非 tty 时关闭；测试注入）。
static COLOR_ENABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

pub fn set_color_enabled(on: bool) {
    COLOR_ENABLED.store(on, std::sync::atomic::Ordering::Relaxed);
}

pub fn color_enabled() -> bool {
    COLOR_ENABLED.load(std::sync::atomic::Ordering::Relaxed)
}

/// 带样式的文本片段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub color: Color,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
}

impl Span {
    pub fn new(text: impl Into<String>) -> Self {
        Span {
            text: text.into(),
            color: Color::Text,
            bold: false,
            italic: false,
            underline: false,
            strike: false,
        }
    }

    pub fn colored(color: Color, text: impl Into<String>) -> Self {
        Span {
            color,
            ..Span::new(text)
        }
    }

    pub fn bold(mut self) -> Self {
        self.bold = true;
        self
    }

    pub fn italic(mut self) -> Self {
        self.italic = true;
        self
    }

    pub fn strike(mut self) -> Self {
        self.strike = true;
        self
    }

    pub fn dim_text(text: impl Into<String>) -> Self {
        Span::colored(Color::Dim, text)
    }

    /// 渲染为 ANSI 字符串（颜色关闭时为纯文本）。
    pub fn paint(&self) -> String {
        if !color_enabled() {
            return self.text.clone();
        }
        let mut codes: Vec<&str> = Vec::new();
        if let Some(c) = self.color.sgr() {
            codes.push(c);
        }
        if self.bold {
            codes.push("1");
        }
        if self.italic {
            codes.push("3");
        }
        if self.underline {
            codes.push("4");
        }
        if self.strike {
            codes.push("9");
        }
        if codes.is_empty() {
            return self.text.clone();
        }
        format!("\x1b[{}m{}\x1b[0m", codes.join(";"), self.text)
    }
}

/// 行级语义（GUI 行容器样式用：标题提字号、代码块上面板；CLI 渲染忽略）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LineKind {
    #[default]
    Normal,
    /// 标题行（级别 1-6）
    Heading(u8),
    /// 代码块行（含块内空行）
    Code,
}

/// 一行 = Span 序列 + 行级语义。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StyledLine(pub Vec<Span>, pub LineKind);

impl From<Span> for StyledLine {
    fn from(s: Span) -> Self {
        StyledLine(vec![s], LineKind::Normal)
    }
}

impl StyledLine {
    pub fn new(spans: Vec<Span>) -> Self {
        StyledLine(spans, LineKind::Normal)
    }

    pub fn plain(text: impl Into<String>) -> Self {
        StyledLine(vec![Span::new(text)], LineKind::Normal)
    }

    pub fn blank() -> Self {
        StyledLine(vec![Span::new("")], LineKind::Normal)
    }

    pub fn push(&mut self, s: Span) {
        self.0.push(s);
    }

    pub fn is_blank(&self) -> bool {
        self.0.iter().all(|s| s.text.is_empty())
    }

    pub fn text(&self) -> String {
        self.0.iter().map(|s| s.text.as_str()).collect()
    }

    pub fn paint(&self) -> String {
        self.0.iter().map(|s| s.paint()).collect()
    }
}

/// 显示宽度（unicode-width；控制字符计 0）。
pub fn display_width(s: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(s)
}

/// 按显示宽度把一行硬折成多行（样式跨行保留）。
pub fn wrap_line(line: &StyledLine, width: usize) -> Vec<StyledLine> {
    if width == 0 {
        return vec![line.clone()];
    }
    // 相邻字符样式一致时并入同一 Span，避免逐字符切片
    fn same_style(a: &Span, b: &Span) -> bool {
        a.color == b.color
            && a.bold == b.bold
            && a.italic == b.italic
            && a.underline == b.underline
            && a.strike == b.strike
    }
    let mut out: Vec<StyledLine> = Vec::new();
    let mut cur: Vec<Span> = Vec::new();
    let mut cur_w = 0usize;
    for span in &line.0 {
        for ch in span.text.chars() {
            let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            if cur_w + w > width && cur_w > 0 {
                out.push(StyledLine(std::mem::take(&mut cur), line.1));
                cur_w = 0;
            }
            match cur.last_mut() {
                Some(last) if same_style(last, span) => last.text.push(ch),
                _ => {
                    let mut s = span.clone();
                    s.text.clear();
                    s.text.push(ch);
                    cur.push(s);
                }
            }
            cur_w += w;
        }
    }
    if !cur.is_empty() || out.is_empty() {
        out.push(StyledLine(cur, line.1));
    }
    out
}

/// token 数紧凑显示（1.2k / 86k / 1.5m）。
pub fn compact_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}m", n as f64 / 1_000_000.0)
    } else if n >= 1000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

/// 时长显示（CC formatDuration 语义：59s / 1m 32s / 12m 4s）。
pub fn format_duration(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m {}s", secs / 60, secs % 60)
    }
}

/// 按字符数截断（不撕裂 UTF-8），超出补省略号。
pub fn truncate_chars(s: &str, max: usize) -> String {
    let mut out: String = s.chars().take(max).collect();
    if s.chars().count() > max {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn span_paints_ansi_and_respects_color_switch() {
        set_color_enabled(true);
        let s = Span::colored(Color::Success, "ok").bold();
        assert_eq!(s.paint(), "\x1b[32;1mok\x1b[0m");
        set_color_enabled(false);
        assert_eq!(s.paint(), "ok", "关色退化为纯文本");
        set_color_enabled(true);
        let plain = Span::new("x");
        assert_eq!(plain.paint(), "x", "无样式不发序列");
    }

    #[test]
    fn wrap_preserves_styles_across_lines() {
        let line = StyledLine(
            vec![Span::new("aaaa"), Span::colored(Color::Error, "bbbbb")],
            LineKind::Normal,
        );
        let wrapped = wrap_line(&line, 6);
        assert_eq!(wrapped.len(), 2);
        assert_eq!(wrapped[0].text(), "aaaabb", "4a+2b 填满 6 列");
        assert_eq!(wrapped[1].text(), "bbb");
        assert_eq!(wrapped[0].0[1].color, Color::Error, "跨行保留颜色");
        assert_eq!(wrapped[1].0[0].color, Color::Error);
    }

    #[test]
    fn wrap_preserves_line_kind() {
        let line = StyledLine(vec![Span::new("fn main() {}")], LineKind::Code);
        let wrapped = wrap_line(&line, 8);
        assert!(
            wrapped.iter().all(|l| l.1 == LineKind::Code),
            "折行后仍标 Code"
        );
    }

    #[test]
    fn wrap_counts_cjk_as_two_columns() {
        let line = StyledLine::plain("中文中文中文");
        let wrapped = wrap_line(&line, 4);
        assert_eq!(wrapped.len(), 3);
        assert_eq!(wrapped[0].text(), "中文");
    }

    #[test]
    fn formats_tokens_and_durations() {
        assert_eq!(compact_tokens(999), "999");
        assert_eq!(compact_tokens(15300), "15.3k");
        assert_eq!(compact_tokens(2_300_000), "2.3m");
        assert_eq!(format_duration(32), "32s");
        assert_eq!(format_duration(92), "1m 32s");
        assert_eq!(format_duration(724), "12m 4s");
    }

    #[test]
    fn truncate_keeps_char_boundaries() {
        assert_eq!(truncate_chars("中文", 10), "中文");
        assert_eq!(truncate_chars("中文", 1), "中…");
    }
}
