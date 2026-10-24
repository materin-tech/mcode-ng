//! 输入编辑器：多行缓冲 + CJK 安全字节光标 + 历史（含草稿保护）。
//!
//! 键位对齐 CC：Enter 提交 / ctrl+j 或 alt+enter 换行（行尾 `\` + Enter 亦换行）/
//! 上下在多行间移动（单行时翻历史）。crossterm 事件 → [`KeyKind`] 的映射在
//! mod.rs（终端层），本模块只认 KeyKind，可纯逻辑测试。

/// 与终端解耦的键事件（mod.rs 负责从 crossterm 翻译）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyKind {
    Char(char),
    Enter,
    Esc,
    Tab,
    ShiftTab,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    Backspace,
    Delete,
    PageUp,
    PageDown,
    CtrlA,
    CtrlE,
    CtrlC,
    CtrlD,
    CtrlJ,
    CtrlK,
    CtrlO,
    CtrlR,
    CtrlU,
    CtrlW,
    AltEnter,
    Other,
}

#[derive(Debug, Default)]
pub struct InputBuffer {
    /// 可含 '\n' 的多行缓冲
    pub buf: String,
    /// 字节偏移（恒在 char 边界）
    pub cursor: usize,
    pub history: Vec<String>,
    /// Some(i) = 正在浏览 history[i]
    pub hist_pos: Option<usize>,
    /// 进入历史浏览前的草稿
    pub draft: String,
}

/// 历史上限（防长会话内存膨胀）。
const MAX_HISTORY: usize = 500;

impl InputBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn text(&self) -> &str {
        &self.buf
    }

    #[cfg(test)]
    pub fn set_text(&mut self, s: &str) {
        self.buf = s.to_string();
        self.cursor = self.buf.len();
        self.hist_pos = None;
    }

    pub fn clear(&mut self) {
        self.buf.clear();
        self.cursor = 0;
        self.hist_pos = None;
    }

    fn clamp_cursor(&mut self) {
        while self.cursor > 0 && !self.buf.is_char_boundary(self.cursor) {
            self.cursor -= 1;
        }
    }

    pub fn insert_char(&mut self, c: char) {
        self.insert_str(&c.to_string());
    }

    pub fn insert_str(&mut self, s: &str) {
        self.clamp_cursor();
        self.buf.insert_str(self.cursor, s);
        self.cursor += s.len();
        self.hist_pos = None;
    }

    pub fn backspace(&mut self) {
        self.clamp_cursor();
        if self.cursor == 0 {
            return;
        }
        let prev = self.buf[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0);
        self.buf.replace_range(prev..self.cursor, "");
        self.cursor = prev;
        self.hist_pos = None;
    }

    pub fn delete(&mut self) {
        self.clamp_cursor();
        if self.cursor >= self.buf.len() {
            return;
        }
        let next = self.buf[self.cursor..]
            .chars()
            .next()
            .map(char::len_utf8)
            .unwrap_or(0);
        let end = (self.cursor + next).min(self.buf.len());
        self.buf.replace_range(self.cursor..end, "");
        self.hist_pos = None;
    }

    /// 光标所在行（0 基）与行文本。
    pub fn cursor_line(&self) -> (usize, &str) {
        let before = &self.buf[..self.cursor];
        let row = before.matches('\n').count();
        let line_start = before.rfind('\n').map(|i| i + 1).unwrap_or(0);
        let line_end = self.buf[self.cursor..]
            .find('\n')
            .map(|i| self.cursor + i)
            .unwrap_or(self.buf.len());
        (row, &self.buf[line_start..line_end])
    }

    fn line_bounds(&self, row: usize) -> (usize, usize) {
        let mut start = 0;
        for _ in 0..row {
            match self.buf[start..].find('\n') {
                Some(i) => start += i + 1,
                None => return (start, self.buf.len()),
            }
        }
        let end = self.buf[start..]
            .find('\n')
            .map(|i| start + i)
            .unwrap_or(self.buf.len());
        (start, end)
    }

    /// 光标在行内的显示列（宽字符按 2 计）。
    pub fn cursor_col(&self) -> usize {
        let (row, _) = self.cursor_line();
        let (start, _) = self.line_bounds(row);
        super::style::display_width(&self.buf[start..self.cursor])
    }

    pub fn left(&mut self) {
        self.clamp_cursor();
        if let Some((i, _)) = self.buf[..self.cursor].char_indices().next_back() {
            self.cursor = i;
        }
    }

    pub fn right(&mut self) {
        self.clamp_cursor();
        if let Some(c) = self.buf[self.cursor..].chars().next() {
            self.cursor += c.len_utf8();
        }
    }

    pub fn home(&mut self) {
        let (row, _) = self.cursor_line();
        let (start, _) = self.line_bounds(row);
        self.cursor = start;
    }

    pub fn end(&mut self) {
        let (row, _) = self.cursor_line();
        let (_, end) = self.line_bounds(row);
        self.cursor = end;
    }

    /// 多行上行；返回 false = 已在首行（调用方转历史）。
    pub fn up(&mut self) -> bool {
        let (row, _) = self.cursor_line();
        if row == 0 {
            return false;
        }
        let col = self.cursor_col();
        let (start, _) = self.line_bounds(row - 1);
        self.cursor = col_at_display_col(&self.buf[start..], col) + start;
        true
    }

    /// 多行下行；返回 false = 已在末行（调用方转历史）。
    pub fn down(&mut self) -> bool {
        let (row, _) = self.cursor_line();
        let total = self.buf.matches('\n').count() + 1;
        if row + 1 >= total {
            return false;
        }
        let col = self.cursor_col();
        let (start, end) = self.line_bounds(row + 1);
        let line = &self.buf[start..end];
        self.cursor = (col_at_display_col(line, col) + start).min(end);
        true
    }

    /// 插入换行（ctrl+j / alt+enter / 行尾 `\` + Enter）。
    pub fn newline(&mut self) {
        self.insert_char('\n');
    }

    /// 提交：入历史并清空。
    pub fn submit(&mut self) {
        let t = std::mem::take(&mut self.buf);
        self.cursor = 0;
        self.hist_pos = None;
        self.draft.clear();
        if t.trim().is_empty() {
            return;
        }
        // 去重：已有同条则挪到末尾（shell 惯例）
        self.history.retain(|h| h != &t);
        self.history.push(t);
        if self.history.len() > MAX_HISTORY {
            self.history.remove(0);
        }
    }

    /// 历史上翻；返回 None = 已到最旧（保持不动）。
    pub fn history_prev(&mut self) -> bool {
        if self.history.is_empty() {
            return false;
        }
        match self.hist_pos {
            None => {
                self.draft = self.buf.clone();
                self.hist_pos = Some(self.history.len() - 1);
            }
            Some(0) => return false,
            Some(i) => self.hist_pos = Some(i - 1),
        }
        let h = self.history[self.hist_pos.unwrap()].clone();
        self.buf = h;
        self.cursor = self.buf.len();
        true
    }

    /// 历史下翻；翻过最新一条时恢复草稿。返回 None = 已到草稿。
    pub fn history_next(&mut self) -> bool {
        match self.hist_pos {
            None => false,
            Some(i) => {
                if i + 1 >= self.history.len() {
                    self.hist_pos = None;
                    self.buf = std::mem::take(&mut self.draft);
                    self.cursor = self.buf.len();
                } else {
                    self.hist_pos = Some(i + 1);
                    self.buf = self.history[i + 1].clone();
                    self.cursor = self.buf.len();
                }
                true
            }
        }
    }

    /// 输入区渲染行（按显示宽折行，保留 '\n' 结构）。
    pub fn render_lines(&self, width: usize) -> Vec<String> {
        let mut out = Vec::new();
        if self.buf.is_empty() {
            return vec![String::new()];
        }
        for line in self.buf.split('\n') {
            if width == 0 || line.is_empty() {
                out.push(line.to_string());
                continue;
            }
            let mut cur = String::new();
            let mut cur_w = 0;
            for ch in line.chars() {
                let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
                if cur_w + w > width && !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    cur_w = 0;
                }
                cur.push(ch);
                cur_w += w;
            }
            out.push(cur);
        }
        out
    }

    /// 光标的（折行后行, 列）——终端实心光标落点。
    pub fn cursor_row_col(&self, width: usize) -> (usize, usize) {
        let (logical_row, _) = self.cursor_line();
        // 折行偏移 = 前面逻辑行的折行数
        let mut rows_before = 0usize;
        for line in self.buf.split('\n').take(logical_row) {
            rows_before += wrapped_count(line, width);
        }
        let (start, end) = {
            let mut start = 0;
            for _ in 0..logical_row {
                start = self.buf[start..]
                    .find('\n')
                    .map(|i| start + i + 1)
                    .unwrap_or(start);
            }
            let end = self.buf[start..]
                .find('\n')
                .map(|i| start + i)
                .unwrap_or(self.buf.len());
            (start, end)
        };
        let line = &self.buf[start..end];
        let mut cur_w = 0usize;
        let mut row_off = 0usize;
        let mut col_off = 0usize;
        let target = self.cursor.min(end);
        for (bi, ch) in line.char_indices() {
            let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            if cur_w + w > width && cur_w > 0 {
                row_off += 1;
                cur_w = 0;
                col_off = 0;
            }
            if start + bi >= target {
                break;
            }
            cur_w += w;
            col_off += w;
        }
        (rows_before + row_off, col_off)
    }
}

fn wrapped_count(line: &str, width: usize) -> usize {
    if width == 0 {
        return 1;
    }
    let mut rows = 1;
    let mut cur_w = 0;
    for ch in line.chars() {
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if cur_w + w > width {
            rows += 1;
            cur_w = 0;
        }
        cur_w += w;
    }
    rows
}

/// 在单行文本里找显示列 ≤ col 的最大 char 边界偏移。
fn col_at_display_col(line: &str, col: usize) -> usize {
    let mut off = 0;
    let mut w = 0;
    for ch in line.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if w + cw > col {
            break;
        }
        w += cw;
        off += ch.len_utf8();
    }
    off
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cjk_cursor_moves_by_char_not_byte() {
        let mut b = InputBuffer::new();
        b.set_text("中文ab");
        assert_eq!(b.cursor, 8, "中文各 3 字节 + ab");
        b.left();
        assert_eq!(b.cursor, 7, "a");
        b.left();
        assert_eq!(b.cursor, 6, "文（3 字节）");
        b.left();
        assert_eq!(b.cursor, 3, "中（3 字节）");
        b.left();
        assert_eq!(b.cursor, 0, "到头不动");
        b.right();
        assert_eq!(b.cursor, 3);
    }

    #[test]
    fn backspace_removes_whole_char() {
        let mut b = InputBuffer::new();
        b.set_text("中文");
        b.backspace();
        assert_eq!(b.text(), "中");
        b.insert_char('x');
        assert_eq!(b.text(), "中x");
        b.backspace();
        assert_eq!(b.text(), "中");
    }

    #[test]
    fn multiline_navigation_preserves_goal_column() {
        let mut b = InputBuffer::new();
        b.set_text("abcd\n中文行\nzz");
        // 光标移到第 2 行第 2 列（显示列 2 = '文' 前）
        b.cursor = "abcd\n".len();
        b.right();
        assert_eq!(b.cursor, "abcd\n中".len());
        b.up();
        assert_eq!(b.cursor, 2, "目标显示列 2 → 第一行 'cd' 之间");
        b.down();
        assert_eq!(b.cursor, "abcd\n中".len(), "回到显示列 2");
        assert!(b.down(), "还有末行");
        assert_eq!(
            b.cursor,
            "abcd\n中文行\nzz".len(),
            "末行按显示列落位（列 2 → zz 末尾）"
        );
        assert!(!b.down(), "末行再下返回 false");
        b.up();
        b.up();
        assert!(!b.up(), "首行再上返回 false");
    }

    #[test]
    fn home_end_work_within_line() {
        let mut b = InputBuffer::new();
        b.set_text("ab\ncd");
        b.cursor = b.buf.len();
        b.home();
        assert_eq!(b.cursor, "ab\n".len(), "行首而非全文首");
        b.end();
        assert_eq!(b.cursor, b.buf.len());
    }

    #[test]
    fn history_with_draft_protection() {
        let mut b = InputBuffer::new();
        b.set_text("草稿未发送");
        assert!(!b.history_prev(), "空历史不动");
        b.set_text("第一条");
        b.submit();
        b.set_text("第二条");
        b.submit();
        assert!(b.is_empty());
        // 正在打草稿时上翻
        b.insert_str("当前输入");
        assert!(b.history_prev());
        assert_eq!(b.text(), "第二条");
        assert!(b.history_prev());
        assert_eq!(b.text(), "第一条");
        assert!(!b.history_prev(), "到顶");
        assert!(b.history_next());
        assert_eq!(b.text(), "第二条");
        assert!(b.history_next());
        assert_eq!(b.text(), "当前输入", "翻过最新恢复草稿");
        assert!(!b.history_next());
        // 提交去重
        b.set_text("第一条");
        b.submit();
        assert_eq!(b.history.len(), 2, "相邻重复不入历史");
    }

    #[test]
    fn submit_rejects_blank() {
        let mut b = InputBuffer::new();
        b.set_text("   ");
        b.submit();
        assert!(b.history.is_empty(), "空白不进历史");
        assert!(b.is_empty());
    }

    #[test]
    fn render_lines_wrap_by_display_width() {
        let mut b = InputBuffer::new();
        b.set_text("中文abc\nx");
        let rows = b.render_lines(4);
        assert_eq!(rows, vec!["中文", "abc", "x"]);
        let (r, c) = {
            b.cursor = b.buf.len();
            b.cursor_row_col(4)
        };
        assert_eq!((r, c), (2, 1), "光标落在折行后的第 3 行第 1 列");
    }

    #[test]
    fn delete_removes_char_after_cursor() {
        let mut b = InputBuffer::new();
        b.set_text("中文x");
        b.cursor = 0;
        b.delete();
        assert_eq!(b.text(), "文x");
    }
}
