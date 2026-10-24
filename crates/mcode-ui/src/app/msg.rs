//! 跨层共享的 UI 类型：权限弹窗待裁决请求 + 截断工具。
//!
//! 消息流模型已上移 `mcode_core::transcript`（与 CLI 共用的唯一事实源），
//! App 以 `transcript: TranscriptState` 承载对话呈现状态；本模块只剩
//! Desktop 特有的权限弹窗载体。

use mcode_core::permission::Decision;

/// 权限确认弹窗的待裁决请求（respond oneshot 回传 runner）
pub struct PendingPermission {
    pub tool: String,
    pub args_summary: String,
    pub respond: Option<tokio::sync::oneshot::Sender<Decision>>,
}

/// 截断显示文本
pub(crate) fn trunc(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}
