//! inline 终端控制：raw mode（不开 alternate screen）、冻结区追加打印、
//! 动态区回移重绘、光标落点。
//!
//! CC 的 static/dynamic 两区模型在这里落地：冻结行直接 `print + \r\n`
//! 进 scrollback（打完即忘）；动态区每帧把光标移回顶部、清屏重写。

use std::io::{stdout, Stdout, Write};

use crossterm::{
    cursor::{MoveToColumn, MoveUp},
    execute, queue,
    style::Print,
    terminal::{self, Clear, ClearType},
};

use super::style::StyledLine;

pub struct Term {
    out: Stdout,
    /// 当前动态区占用的终端行数
    dyn_height: usize,
    /// 光标停在动态区第几行（0 = 帧顶）。回移必须精确到帧顶：
    /// 光标通常停在输入行（帧中间），按 dyn_height 整体回移会越回冻结区
    cursor_at: usize,
}

impl Term {
    pub fn enable() -> std::io::Result<Self> {
        terminal::enable_raw_mode()?;
        execute!(stdout(), crossterm::cursor::Show)?;
        Ok(Term {
            out: stdout(),
            dyn_height: 0,
            cursor_at: 0,
        })
    }

    /// 测试注入用构造（不碰真实终端状态）。
    #[cfg(test)]
    fn with(out: Stdout) -> Self {
        Term {
            out,
            dyn_height: 0,
            cursor_at: 0,
        }
    }

    pub fn restore(&mut self) {
        // 动态区最后保留：清掉重绘区，光标落到行首（退出后 shell 提示符干净）
        let _ = self.out.write_all(b"\r\n");
        let _ = self.out.flush();
        let _ = terminal::disable_raw_mode();
    }

    /// 每帧刷新尺寸（终端 resize 感知）。
    pub fn refresh_size(&mut self) -> (usize, usize) {
        let (w, h) = terminal::size().unwrap_or((80, 24));
        (w as usize, h as usize)
    }

    /// 冻结区追加：写完即忘（scrollback 原生滚动/选择/复制）。
    ///
    /// 时序：先精确回移到动态区帧顶 → 擦掉整幅动态区 → 再打印冻结行。
    /// 顺序反了冻结内容会打在上一帧输入框行的位置（光标停在帧中间）。
    pub fn freeze(&mut self, lines: &[StyledLine]) -> std::io::Result<()> {
        if lines.is_empty() {
            return Ok(());
        }
        if self.dyn_height > 0 {
            if self.cursor_at > 0 {
                queue!(self.out, MoveUp(self.cursor_at as u16))?;
            }
            queue!(self.out, Clear(ClearType::FromCursorDown))?;
        }
        // MoveUp 只动行不动列；光标若停在输入列，内容会整体右移并逐帧漂移
        queue!(self.out, MoveToColumn(0))?;
        for l in lines {
            queue!(self.out, Print(l.paint()), Print("\r\n"))?;
        }
        self.dyn_height = 0;
        self.cursor_at = 0;
        self.out.flush()
    }

    /// 动态区整帧重绘；`cursor` = (行号, 显示列)，行号从帧顶数。
    ///
    /// 帧末行**不带** `\r\n`：贴屏底时末行换行会把整屏顶滚一行，累积即失步。
    /// 帧高钳到屏高-1，composer 永远完整可见。
    pub fn draw_dynamic(
        &mut self,
        lines: &[StyledLine],
        cursor: Option<(usize, usize)>,
    ) -> std::io::Result<()> {
        let (_w, h) = self.refresh_size();
        let skip = frame_skip(lines.len(), h);
        let lines = &lines[skip..];
        let n = lines.len();
        if self.dyn_height > 0 {
            if self.cursor_at > 0 {
                queue!(self.out, MoveUp(self.cursor_at as u16))?;
            }
            queue!(self.out, Clear(ClearType::FromCursorDown))?;
        }
        // 同 freeze：打印起点必须回到列 0，否则整帧右移漂移
        queue!(self.out, MoveToColumn(0))?;
        for (i, l) in lines.iter().enumerate() {
            if i + 1 == n {
                queue!(self.out, Print(l.paint()))?;
            } else {
                queue!(self.out, Print(l.paint()), Print("\r\n"))?;
            }
        }
        self.dyn_height = n;
        match cursor {
            Some((row, col)) => {
                // 打完后光标在末行行尾：回移到目标行（钳在帧内）
                let row = row.min(n.saturating_sub(1));
                let from_end = n.saturating_sub(1) - row;
                if from_end > 0 {
                    queue!(self.out, MoveUp(from_end as u16))?;
                }
                queue!(self.out, MoveToColumn(col as u16))?;
                self.cursor_at = row;
            }
            None => {
                queue!(self.out, MoveToColumn(0))?;
                self.cursor_at = n.saturating_sub(1);
            }
        }
        self.out.flush()
    }

    /// 外部程序（编辑器挂起）接管过终端后调用：动态区簿记作废，
    /// 下一帧不再回移，直接整幅重印。
    pub fn reset_dynamic(&mut self) {
        self.dyn_height = 0;
        self.cursor_at = 0;
    }
}

/// 帧高钳制：屏高 h 时最多显示 h-1 行（末行留白不滚屏），裁头保尾。
/// 屏高不可信（0/1，如 PTY 未设 winsize）时不裁——误裁会把整个 composer 裁没。
fn frame_skip(len: usize, screen_h: usize) -> usize {
    if screen_h < 2 {
        return 0;
    }
    len.saturating_sub(screen_h - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    // 终端 IO 本体（raw mode/清屏/回移）无法在 CI 断言，走人工冒烟清单；
    // 这里只锁簿记状态。

    #[test]
    fn initial_state_has_zero_dynamic_height() {
        let t = Term::with(std::io::stdout());
        assert_eq!(t.dyn_height, 0);
        assert_eq!(t.cursor_at, 0);
    }

    #[test]
    fn frame_skip_keeps_tail_within_screen() {
        assert_eq!(frame_skip(5, 30), 0, "矮帧不裁");
        assert_eq!(frame_skip(10, 5), 6, "10 行帧钳到屏高-1=4 行：裁掉前 6");
        assert_eq!(frame_skip(4, 5), 0);
        assert_eq!(frame_skip(10, 1), 0, "屏高不可信（0/1）不裁");
        assert_eq!(frame_skip(0, 30), 0);
    }
}
