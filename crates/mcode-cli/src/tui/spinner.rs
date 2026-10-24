//! spinner（CLI 侧薄壳）。
//!
//! 实现已上移 `mcode_core::transcript::spinner`（CLI 与 Desktop 共用），
//! 这里只再导出 CLI 用到的子集。

pub use mcode_core::transcript::spinner::{frame_at, status_line, time_nonce};
