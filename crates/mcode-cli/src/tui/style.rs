//! 样式基元（CLI 侧薄壳）。
//!
//! 实现已上移 `mcode_core::transcript::style`（CLI 与 Desktop 共用的唯一
//! 事实源）；本模块只保留终端特有的环境探测（tty / NO_COLOR），并再导出
//! CLI 用到的子集，tui 内部引用路径不变。

pub use mcode_core::transcript::style::{
    compact_tokens, display_width, truncate_chars, wrap_line, Color, Span, StyledLine,
};

/// 从环境初始化（非 tty 或 NO_COLOR 存在 → 关色）。
pub fn init_from_env() {
    let no_color = std::env::var_os("NO_COLOR").is_some();
    let tty = !no_color && crossterm::tty::IsTty::is_tty(&std::io::stdout());
    mcode_core::transcript::style::set_color_enabled(tty && !no_color);
}
