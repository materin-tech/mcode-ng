//! 工具体系 —— rich tool contract + 注册表 + 轻量校验。
//!
//! 相对旧版的关键升级：
//! 1. **输出钳制升级为落盘**：超过 MAX_TOOL_OUTPUT_CHARS 的结果写
//!    `~/.mcode/tool-outputs/` 并提示模型用 read_file 取回（旧版直接截断丢数据，
//!    长测试日志/大 grep 结果信息量全灭）；
//! 2. **read-before-write**：edit_file/write_file 前校验文件已读且未被动过
//!    （见 state::FileStateTracker）；
//! 3. **定义直达 API**：[`ToolDefinition`] 可零成本转 [`crate::api::ToolSpec`]；
//! 4. 工具层不依赖 agent 模块（旧版 ToolContext 反向依赖 runner，
//!    task 工具改走 `extra: Arc<dyn Any>` 注入点，消除环）。

pub mod bash_tool;
pub mod fs_tools;
pub mod state;
pub mod todo_tool;
pub mod web;

pub use state::{BashSession, FileStateTracker, TodoState};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use tokio::sync::watch;

// ---------------------------------------------------------------------------
// 核心类型
// ---------------------------------------------------------------------------

/// 工具定义（模型可见 schema）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    /// 内置工具为静态名，MCP 工具为运行时名（mcp__<server>__<tool>）
    pub name: String,
    pub description: String,
    /// JSON Schema（type/properties/required）
    pub parameters: serde_json::Value,
    pub is_read_only: bool,
}

impl ToolDefinition {
    /// → API 层工具规格（anthropic 原生工具块 / responses function / chat
    /// function 三种后端共用同一 wire 形态）。
    pub fn to_spec(&self) -> crate::api::ToolSpec {
        crate::api::ToolSpec {
            name: self.name.clone(),
            description: self.description.clone(),
            parameters: self.parameters.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub success: bool,
    /// 回传给模型的文本（已钳制/落盘）
    pub output: String,
    /// 给 UI 的渲染块（diff / 输出卡片）
    pub display: Option<DisplayBlock>,
    /// 附件图片（read_file 读图时携带；多模态模型才真正可见）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<ToolImage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolImage {
    pub media_type: String,
    /// base64 裸数据
    pub data: String,
}

impl ToolResult {
    pub fn ok(output: impl Into<String>) -> Self {
        ToolResult {
            success: true,
            output: output.into(),
            display: None,
            images: vec![],
        }
    }

    pub fn err(output: impl Into<String>) -> Self {
        ToolResult {
            success: false,
            output: output.into(),
            display: None,
            images: vec![],
        }
    }

    pub fn with_display(mut self, d: DisplayBlock) -> Self {
        self.display = Some(d);
        self
    }

    pub fn with_image(mut self, media_type: &str, data: String) -> Self {
        self.images.push(ToolImage {
            media_type: media_type.to_string(),
            data,
        });
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DisplayBlock {
    Diff {
        path: String,
        lines: Vec<DiffLine>,
    },
    Output {
        stdout: String,
        stderr: String,
        code: i32,
    },
    FileRef {
        path: String,
        lines: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffLine {
    pub kind: DiffKind,
    pub text: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DiffKind {
    Context,
    Add,
    Delete,
    Hunk,
}

/// 统一 diff 行级数据（edit_file / write_file 产出，UI 直接渲染）。
pub fn unified_diff(old: &str, new: &str, path: &str) -> Vec<DiffLine> {
    use similar::ChangeTag;
    let diff = similar::TextDiff::from_lines(old, new);
    let mut out = vec![DiffLine {
        kind: DiffKind::Hunk,
        text: format!("@@ {path} @@"),
    }];
    for change in diff.iter_all_changes() {
        let line = match change.tag() {
            ChangeTag::Equal => DiffLine {
                kind: DiffKind::Context,
                text: change.to_string(),
            },
            ChangeTag::Insert => DiffLine {
                kind: DiffKind::Add,
                text: change.to_string(),
            },
            ChangeTag::Delete => DiffLine {
                kind: DiffKind::Delete,
                text: change.to_string(),
            },
        };
        out.push(line);
    }
    out
}

pub fn diff_stats(lines: &[DiffLine]) -> (usize, usize) {
    let add = lines.iter().filter(|l| l.kind == DiffKind::Add).count();
    let del = lines.iter().filter(|l| l.kind == DiffKind::Delete).count();
    (add, del)
}

// ---------------------------------------------------------------------------
// 执行上下文
// ---------------------------------------------------------------------------

/// 跨工具共享的会话级状态。
pub struct ToolSharedState {
    /// read-before-write 追踪
    pub files: FileStateTracker,
    /// bash 会话（cwd 持久化 + 后台任务表）
    pub bash: BashSession,
    /// TodoWrite 状态
    pub todos: TodoState,
    /// 大输出落盘目录（默认 ~/.mcode/tool-outputs；测试可重定向）
    pub output_dir: PathBuf,
}

impl Default for ToolSharedState {
    fn default() -> Self {
        let output_dir = crate::util::mcode_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("tool-outputs");
        ToolSharedState {
            files: FileStateTracker::default(),
            bash: BashSession::default(),
            todos: TodoState::default(),
            output_dir,
        }
    }
}

/// 工具执行上下文。
#[derive(Clone)]
pub struct ToolContext {
    pub cwd: PathBuf,
    pub cancel: watch::Receiver<bool>,
    pub shared: Arc<std::sync::Mutex<ToolSharedState>>,
    /// harness 注入点（task 工具派发 subagent 用；普通工具忽略）。
    /// 工具层不反向依赖 agent 模块，用类型擦除解环。
    pub extra: Arc<dyn std::any::Any + Send + Sync>,
}

impl ToolContext {
    /// 测试/纯工具场景的最小上下文。
    pub fn for_tests(cwd: PathBuf) -> (Self, watch::Sender<bool>) {
        let (tx, rx) = watch::channel(false);
        (
            ToolContext {
                cwd,
                cancel: rx,
                shared: Arc::default(),
                extra: Arc::new(()),
            },
            tx,
        )
    }
}

#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    fn def(&self) -> ToolDefinition;
    async fn execute(&self, input: serde_json::Value, ctx: &ToolContext) -> Result<ToolResult>;
}

// ---------------------------------------------------------------------------
// 注册表
// ---------------------------------------------------------------------------

/// 工具注册表 — 收纳 + 按名检索（支持运行时注册 MCP 工具）。
pub struct ToolRegistry {
    tools: RwLock<Vec<Arc<dyn Tool>>>,
}

impl ToolRegistry {
    pub fn with_builtins() -> Self {
        let r = Self {
            tools: RwLock::new(vec![]),
        };
        for t in fs_tools::all_fs_tools() {
            r.register(t);
        }
        r.register(Arc::new(bash_tool::BashTool));
        r.register(Arc::new(todo_tool::TodoWriteTool));
        for t in web::all_web_tools() {
            r.register(t);
        }
        r
    }

    pub fn new_empty() -> Self {
        Self {
            tools: RwLock::new(vec![]),
        }
    }

    pub fn register(&self, t: Arc<dyn Tool>) {
        if let Ok(mut tools) = self.tools.write() {
            tools.push(t);
        }
    }

    /// 移除名字以 `prefix` 开头的工具（MCP 刷新时清掉旧 `mcp__` 条目）。
    pub fn unregister_by_prefix(&self, prefix: &str) {
        if let Ok(mut tools) = self.tools.write() {
            tools.retain(|t| !t.def().name.starts_with(prefix));
        }
    }

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools
            .read()
            .map(|tools| tools.iter().map(|t| t.def()).collect())
            .unwrap_or_default()
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools
            .read()
            .ok()?
            .iter()
            .find(|t| t.def().name == name)
            .cloned()
    }

    pub fn names(&self) -> Vec<String> {
        self.tools
            .read()
            .map(|tools| tools.iter().map(|t| t.def().name.clone()).collect())
            .unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------
// 输入校验 + 输出钳制
// ---------------------------------------------------------------------------

/// 轻量 JSON-schema 校验：required + 类型 + 未知参数（对齐 CC validate）。
pub fn validate_input(
    tool: &ToolDefinition,
    input: &serde_json::Value,
) -> std::result::Result<(), String> {
    let Some(obj) = input.as_object() else {
        return Err(format!("{}: 输入不是对象", tool.name));
    };
    if let Some(required) = tool.parameters["required"].as_array() {
        for key in required {
            let key = key.as_str().unwrap_or_default();
            if !obj.contains_key(key) {
                return Err(format!("{}: 缺少必填参数 '{key}'", tool.name));
            }
        }
    }
    let props = &tool.parameters["properties"];
    for (key, value) in obj {
        let Some(prop) = props.get(key) else {
            return Err(format!("{}: 未知参数 '{key}'", tool.name));
        };
        let expected = prop["type"].as_str().unwrap_or_default();
        let actual = if value.is_array() {
            "array"
        } else if value.is_object() {
            "object"
        } else if value.is_string() {
            "string"
        } else if value.is_boolean() {
            "boolean"
        } else if value.is_number() {
            "number"
        } else {
            "null"
        };
        if !expected.is_empty() && expected != actual {
            return Err(format!(
                "{}: 参数 '{key}' 应为 {expected}，实为 {actual}",
                tool.name
            ));
        }
    }
    Ok(())
}

/// 工具产出钳制阈值（对齐 CC）。
pub const MAX_TOOL_OUTPUT_CHARS: usize = 30_000;

/// 硬上限（超过直接拒绝入库——比如 read 到疯狂输出）。
pub const MAX_TOOL_OUTPUT_HARD: usize = 3_000_000;

/// 钳制：≤阈值原样返回；超限落盘 `~/.mcode/tool-outputs/<id>.txt` 并在尾部
/// 提示完整输出路径（模型可 read_file 取回）——信息不灭，对齐 CC 行为。
pub fn clamp_output(shared: &ToolSharedState, s: String) -> String {
    let len = s.chars().count();
    if len <= MAX_TOOL_OUTPUT_CHARS {
        return s;
    }
    let persist = std::fs::create_dir_all(&shared.output_dir).is_ok();
    let path = shared
        .output_dir
        .join(format!("out-{}.txt", uuid::Uuid::new_v4().simple()));
    let saved = persist && std::fs::write(&path, &s).is_ok();
    let head: String = s.chars().take(MAX_TOOL_OUTPUT_CHARS).collect();
    match saved {
        true => format!(
            "{head}\n\n[输出共 {len} 字符，已截断。完整输出已保存到 {}，可用 read_file 查看]",
            path.display()
        ),
        false => format!("{head}\n\n[输出共 {len} 字符，已截断（落盘失败）]",),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_marks_add_delete() {
        let lines = unified_diff("a\nb\n", "a\nc\n", "f.rs");
        let (a, d) = diff_stats(&lines);
        assert_eq!((a, d), (1, 1));
    }

    #[test]
    fn registry_has_twelve_builtins() {
        let r = ToolRegistry::with_builtins();
        let names = r.names();
        for expected in [
            "read_file",
            "write_file",
            "edit_file",
            "glob",
            "grep",
            "bash",
            "todo_write",
            "web_fetch",
            "web_search",
        ] {
            assert!(
                names.iter().any(|n| n == expected),
                "缺少工具 {expected}，实际: {names:?}"
            );
        }
        assert!(names.len() >= 9);
    }

    #[test]
    fn validate_required_and_unknown() {
        let def = ToolDefinition {
            name: "t".to_string(),
            description: String::new(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"]
            }),
            is_read_only: true,
        };
        assert!(validate_input(&def, &serde_json::json!({"path": "a"})).is_ok());
        assert_eq!(
            validate_input(&def, &serde_json::json!({})).unwrap_err(),
            "t: 缺少必填参数 'path'"
        );
        assert!(validate_input(&def, &serde_json::json!({"path": 1})).is_err());
        assert!(validate_input(&def, &serde_json::json!({"path": "a", "x": 1})).is_err());
        assert!(validate_input(&def, &serde_json::json!("not-object")).is_err());
    }

    #[test]
    fn clamp_truncates_and_persists_to_disk() {
        let dir = std::env::temp_dir().join(format!("mcode-tools-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let shared = ToolSharedState {
            output_dir: dir.clone(),
            ..Default::default()
        };
        // 短输出直通
        assert_eq!(clamp_output(&shared, "short".into()), "short");
        // 长输出截断 + 落盘 + 提示路径
        let long = "x".repeat(MAX_TOOL_OUTPUT_CHARS + 10);
        let out = clamp_output(&shared, long.clone());
        assert!(out.contains("已截断"));
        assert!(out.contains("read_file"));
        // 落盘内容完整
        let saved = std::fs::read_dir(&dir).unwrap().next().unwrap().unwrap();
        let full = std::fs::read_to_string(saved.path()).unwrap();
        assert_eq!(full.chars().count(), long.chars().count());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn definition_converts_to_api_spec() {
        let d = ToolDefinition {
            name: "grep".into(),
            description: "search".into(),
            parameters: serde_json::json!({"type": "object"}),
            is_read_only: true,
        };
        let spec = d.to_spec();
        assert_eq!(spec.name, "grep");
        assert_eq!(spec.parameters["type"], "object");
    }
}
