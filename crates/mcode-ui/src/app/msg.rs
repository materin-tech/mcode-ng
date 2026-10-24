//! 消息流视图模型与折叠状态机（App 与渲染层共享的类型定义）。

use gpui::SharedString;
use mcode_core::permission::Decision;

/// 消息折叠三态（点击头部行循环：Expanded → Preview → Collapsed → Expanded）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum FoldState {
    /// 完整渲染
    #[default]
    Expanded,
    /// 预览：前 10 行（长回复不刷屏）
    Preview,
    /// 单行摘要
    Collapsed,
}

impl FoldState {
    pub(crate) fn next(self) -> Self {
        match self {
            FoldState::Expanded => FoldState::Preview,
            FoldState::Preview => FoldState::Collapsed,
            FoldState::Collapsed => FoldState::Expanded,
        }
    }
}

/// 默认折叠态：≤30 行完整展开，超长回复默认预览（长回复不刷屏）
pub(crate) fn default_fold_for(text: &str) -> FoldState {
    if text.lines().count() > 30 {
        FoldState::Preview
    } else {
        FoldState::Expanded
    }
}

/// 消息流视图模型（对齐 prototype chat.html 的消息形态）
#[derive(Clone, Debug)]
pub enum ChatMsg {
    User {
        text: SharedString,
        refs: Vec<SharedString>,
    },
    Assistant {
        text: SharedString,
        fold: FoldState,
    },
    /// 思考过程（连续 Reasoning 增量合并；与 Assistant 同一套三态折叠）
    Thinking {
        text: SharedString,
        fold: FoldState,
    },
    /// 工具调用卡片（MCP 工具名为运行时字符串，name 不用 &'static str）
    ToolCard {
        name: SharedString,
        args: SharedString,
        summary: SharedString,
        running: bool,
        success: bool,
    },
    Diff {
        path: SharedString,
        lines: Vec<DiffLine>,
    },
    Notice {
        text: SharedString,
        error: bool,
    },
    /// 回合运行占位（finish_run 时移除）
    Running,
}

#[derive(Clone, Debug)]
pub struct DiffLine {
    pub add: bool,
    pub del: bool,
    pub hunk: bool,
    pub text: SharedString,
}

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
