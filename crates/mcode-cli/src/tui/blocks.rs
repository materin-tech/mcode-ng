//! 块模型 + 事件状态机（CLI 侧薄壳）。
//!
//! 实现已上移 `mcode_core::transcript::blocks`（CLI 与 Desktop 共用的唯一
//! 事实源）；`TranscriptState` 在此别名为历史名字 `TuiState`，tui 内部
//! 引用路径不变。

pub use mcode_core::transcript::blocks::{Block, NoticeKind, TranscriptState as TuiState};
