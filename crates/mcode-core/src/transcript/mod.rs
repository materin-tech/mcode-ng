//! 终端呈现层（CLI 与 Desktop 共用的唯一事实源）。
//!
//! 「块模型 + 事件状态机 + 渲染」三者在此一层闭环：
//! - [`style`]：颜色 token 与带样式 Span/StyledLine、宽度感知折行
//! - [`markdown`]：pulldown-cmark → CC 风格样式行（h1 bold+italic+underline、
//!   codespan 紫、blockquote ▎、列表 2×depth、管道表格）
//! - [`blocks`]：Block 模型 + AgentEvent 驱动的状态机（[`blocks::TranscriptState`]）
//! - [`tools`]：`⏺ Tool(参数摘要)` + `⎿` 结果行的每工具格式表
//! - [`spinner`]：`✻ Verb… (esc to interrupt · ↓ N tokens · 32s)` 状态行
//! - [`render`]：单块渲染（`render_block`），冻结/动态两端共用
//!
//! 终端特有的部分（ANSI 落盘、PTY、键位、整帧组装）留在 mcode-cli；
//! Desktop 端消费同一套块渲染，只做 Span → gpui 高亮的映射。

pub mod blocks;
pub mod markdown;
pub mod render;
pub mod spinner;
pub mod style;
pub mod tools;

pub use blocks::{
    AgentState, Block, InternalLine, NoticeKind, StreamMode, ToolBlock, ToolStatus,
    TranscriptState, SHELL_SCOPE,
};
pub use markdown::{render_markdown, render_markdown_trimmed};
pub use render::render_block;
pub use style::{
    compact_tokens, display_width, format_duration, truncate_chars, Color, LineKind, Span,
    StyledLine,
};
pub use tools::{tool_header, tool_result_lines, TodoItem};
