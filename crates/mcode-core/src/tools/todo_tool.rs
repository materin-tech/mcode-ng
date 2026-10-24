//! TodoWrite —— 任务清单工具（CC 的 TodoWrite 对齐）。
//!
//! 语义：整表替换；同表内至多一个 in_progress（CC 提示语对齐）；
//! 状态机 pending → in_progress → completed。

use super::{Tool, ToolContext, ToolDefinition, ToolResult};
use crate::tools::state::{TodoItem, TodoItemStatus};
use anyhow::Result;
use serde_json::{json, Value};

pub struct TodoWriteTool;

fn parse_status(s: &str) -> Option<TodoItemStatus> {
    match s {
        "pending" => Some(TodoItemStatus::Pending),
        "in_progress" => Some(TodoItemStatus::InProgress),
        "completed" => Some(TodoItemStatus::Completed),
        _ => None,
    }
}

fn status_name(s: &TodoItemStatus) -> &'static str {
    match s {
        TodoItemStatus::Pending => "pending",
        TodoItemStatus::InProgress => "in_progress",
        TodoItemStatus::Completed => "completed",
    }
}

#[async_trait::async_trait]
impl Tool for TodoWriteTool {
    fn def(&self) -> ToolDefinition {
        ToolDefinition {
            name: "todo_write".into(),
            description: "维护任务清单（整表替换）。复杂多步任务开工前先列计划；\
                开工一条置 in_progress，完成即置 completed 再开下一条——\
                同一时刻至多一条 in_progress。简单任务不要用。"
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "todos": {
                        "type": "array",
                        "description": "完整任务列表",
                        "items": {
                            "type": "object",
                            "properties": {
                                "content": { "type": "string", "description": "任务描述（祈使句，如 \"跑测试\"）" },
                                "status": { "type": "string", "description": "pending | in_progress | completed" },
                                "active_form": { "type": "string", "description": "进行时文案（如 \"正在跑测试\"）" }
                            },
                            "required": ["content", "status", "active_form"]
                        }
                    }
                },
                "required": ["todos"]
            }),
            is_read_only: false,
        }
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> Result<ToolResult> {
        let Some(arr) = input["todos"].as_array() else {
            return Ok(ToolResult::err("todo_write: todos 必须是数组"));
        };
        let mut items: Vec<TodoItem> = Vec::new();
        for (i, v) in arr.iter().enumerate() {
            let content = v["content"].as_str().unwrap_or_default().trim().to_string();
            let status_raw = v["status"].as_str().unwrap_or_default();
            let active_form = v["active_form"]
                .as_str()
                .unwrap_or_default()
                .trim()
                .to_string();
            let Some(status) = parse_status(status_raw) else {
                return Ok(ToolResult::err(format!(
                    "todo_write: 第 {} 项 status 非法（{status_raw:?}），须为 pending/in_progress/completed",
                    i + 1
                )));
            };
            if content.is_empty() {
                return Ok(ToolResult::err(format!(
                    "todo_write: 第 {} 项 content 不能为空",
                    i + 1
                )));
            }
            let active_form = if active_form.is_empty() {
                content.clone()
            } else {
                active_form
            };
            items.push(TodoItem {
                content,
                status,
                active_form,
            });
        }
        {
            let mut shared = ctx.shared.lock().expect("工具状态锁");
            if let Err(e) = shared.todos.replace_all(items) {
                return Ok(ToolResult::err(format!("todo_write: {e}")));
            }
        }
        // 状态摘要
        let shared = ctx.shared.lock().expect("工具状态锁");
        let (done, total) = shared.todos.progress();
        let active = shared.todos.active();
        let active_status = shared
            .todos
            .active()
            .map(|a| status_name(&a.status))
            .unwrap_or("none");
        let mut summary = format!("清单已更新（{done}/{total} 完成，active={active_status}）");
        if let Some(a) = active {
            summary.push_str(&format!("；进行中: {}", a.active_form));
        }
        let multi = shared
            .todos
            .items()
            .iter()
            .filter(|i| i.status == TodoItemStatus::InProgress)
            .count();
        if multi > 1 {
            summary.push_str("\n[提醒] 多个任务同时 in_progress——一次专注一条");
        }
        Ok(ToolResult::ok(summary))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn ctx() -> ToolContext {
        let (ctx, _tx) = ToolContext::for_tests(std::env::temp_dir());
        ctx
    }

    #[tokio::test]
    async fn replaces_whole_table_and_reports_progress() {
        let ctx = ctx().await;
        let out = TodoWriteTool
            .execute(
                json!({"todos": [
                    {"content": "写实现", "status": "completed", "active_form": "写实现中"},
                    {"content": "跑测试", "status": "in_progress", "active_form": "跑测试中"},
                    {"content": "发版", "status": "pending", "active_form": "发版中"}
                ]}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.success, "{}", out.output);
        assert!(out.output.contains("1/3"));
        assert!(out.output.contains("跑测试中"));
        let shared = ctx.shared.lock().unwrap();
        assert_eq!(shared.todos.items().len(), 3);
    }

    #[tokio::test]
    async fn rejects_invalid_status_and_empty_content() {
        let ctx = ctx().await;
        let out = TodoWriteTool
            .execute(
                json!({"todos": [{"content": "x", "status": "doing", "active_form": "x"}]}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!out.success);
        assert!(out.output.contains("status 非法"));
        let out = TodoWriteTool
            .execute(
                json!({"todos": [{"content": "  ", "status": "pending", "active_form": "x"}]}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!out.success);
        assert!(out.output.contains("不能为空"));
    }

    #[tokio::test]
    async fn warns_on_multiple_in_progress() {
        let ctx = ctx().await;
        let out = TodoWriteTool
            .execute(
                json!({"todos": [
                    {"content": "a", "status": "in_progress", "active_form": "a"},
                    {"content": "b", "status": "in_progress", "active_form": "b"}
                ]}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.success);
        assert!(out.output.contains("多个任务同时 in_progress"));
    }

    #[test]
    fn status_roundtrip() {
        assert_eq!(parse_status("pending"), Some(TodoItemStatus::Pending));
        assert_eq!(parse_status("bogus"), None);
        assert_eq!(status_name(&TodoItemStatus::InProgress), "in_progress");
    }
}
