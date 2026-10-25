//! 工具渲染表：`⏺ Tool(参数摘要)` 与 `⎿` 结果行的 CC 等价格式。
//!
//! 每个工具 = display_name + args_summary + result_lines 三件套
//! （对应 CC 工具的 userFacingName / renderToolUseMessage /
//! renderToolResultMessage），未覆盖的工具走通用兜底。
//! 事件流里没有工具耗时，耗时由块的墙钟计时补（见 blocks.rs）。

use super::style::{truncate_chars, Color, LineKind, Span, StyledLine};
use crate::tools::DisplayBlock;

/// 单行结果上限（普通模式），详情模式放开。
const RESULT_PREVIEW_LINES: usize = 5;
const RESULT_DETAIL_LINES: usize = 200;
const DIFF_PREVIEW_LINES: usize = 40;
const WRITE_PREVIEW_LINES: usize = 10;
/// bash 命令摘要截断（CC 160 字符）。
const CMD_SUMMARY_CHARS: usize = 160;

/// todo_write 的清单项。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TodoItem {
    pub content: String,
    pub status: String,
    pub active_form: String,
}

/// mcode 工具名 → CC 风格显示名。
pub fn tool_display_name(name: &str) -> &str {
    match name {
        "read_file" => "Read",
        "write_file" => "Write",
        "edit_file" => "Update",
        "bash" => "Bash",
        "grep" => "Search",
        "glob" => "Search",
        "task" => "Agent",
        "todo_write" => "TodoWrite",
        "web_fetch" => "Fetch",
        other => other,
    }
}

fn args_field(args: &str, key: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(args)
        .ok()?
        .get(key)?
        .as_str()
        .map(|s| s.to_string())
}

/// `⏺ Tool(摘要)` 里的摘要内容。
pub fn tool_args_summary(name: &str, args_json: &str) -> String {
    match name {
        "read_file" => args_field(args_json, "path").unwrap_or_else(|| "?".into()),
        "write_file" => args_field(args_json, "path").unwrap_or_else(|| "?".into()),
        "edit_file" => args_field(args_json, "path").unwrap_or_else(|| "?".into()),
        "bash" => truncate_chars(
            &args_field(args_json, "command").unwrap_or_else(|| "?".into()),
            CMD_SUMMARY_CHARS,
        ),
        "grep" => {
            let pattern = args_field(args_json, "pattern").unwrap_or_else(|| "?".into());
            match args_field(args_json, "output_mode") {
                Some(m) => format!("pattern: \"{pattern}\", output_mode: \"{m}\""),
                None => format!("pattern: \"{pattern}\""),
            }
        }
        "glob" => format!(
            "pattern: \"{}\"",
            args_field(args_json, "pattern").unwrap_or_else(|| "?".into())
        ),
        "task" => args_field(args_json, "description").unwrap_or_else(|| "?".into()),
        "todo_write" => "更新任务清单".to_string(),
        "web_fetch" => args_field(args_json, "url").unwrap_or_else(|| "?".into()),
        _ => truncate_chars(args_json.replace('\n', " ").trim(), 120),
    }
}

/// `⏺ Tool(…)` 行的括号内文本（含摘要）。
pub fn tool_header(name: &str, args_json: &str) -> String {
    format!(
        "{}({})",
        tool_display_name(name),
        tool_args_summary(name, args_json)
    )
}

/// 结果行（`⎿` 之后的内容）。普通模式给摘要 + 预览，`detail` 模式放开全文。
pub fn tool_result_lines(
    name: &str,
    args_json: &str,
    success: bool,
    output: &str,
    display: Option<&DisplayBlock>,
    detail: bool,
) -> Vec<StyledLine> {
    if !success {
        return error_lines(output);
    }
    let mut lines = match display {
        Some(DisplayBlock::Diff { lines: diff, .. }) => {
            let (add, del) = diff_stats(diff);
            let mut out = vec![diff_stat_line(add, del)];
            out.extend(diff_lines(
                diff,
                if detail {
                    usize::MAX
                } else {
                    DIFF_PREVIEW_LINES
                },
            ));
            out
        }
        Some(DisplayBlock::Output {
            stdout,
            stderr,
            code,
        }) => output_display_lines(stdout, stderr, *code, detail),
        Some(DisplayBlock::FileRef { lines: range, .. }) => {
            vec![count_line("Read", file_range_count(range), "line")]
        }
        None => fallback_result_lines(name, args_json, output, detail),
    };
    // 统一截断兜底（普通模式）
    if !detail {
        let limit = if matches!(display, Some(DisplayBlock::Diff { .. })) {
            DIFF_PREVIEW_LINES
        } else if name == "write_file" {
            // Wrote 行 + 前 10 行预览（预览自带展开提示）
            WRITE_PREVIEW_LINES + 1
        } else {
            RESULT_PREVIEW_LINES
        };
        if lines.len() > limit {
            let extra = lines.len() - limit;
            lines.truncate(limit);
            lines.push(Span::dim_text(format!("… +{extra} lines (ctrl+o to expand)")).into());
        }
    } else if lines.len() > RESULT_DETAIL_LINES {
        let extra = lines.len() - RESULT_DETAIL_LINES;
        lines.truncate(RESULT_DETAIL_LINES);
        lines.push(Span::dim_text(format!("… +{extra} more lines")).into());
    }
    lines
}

/// 未成功：红错误行（CC 统一红文案）。
fn error_lines(output: &str) -> Vec<StyledLine> {
    let mut out = Vec::new();
    for l in output.lines().take(6) {
        out.push(Span::colored(Color::Error, l).into());
    }
    if out.is_empty() {
        out.push(Span::colored(Color::Error, "Error").into());
    }
    out
}

/// 无 display 时的文本兜底：空输出 = Done；图片读取提示 dim。
fn fallback_result_lines(
    name: &str,
    args_json: &str,
    output: &str,
    detail: bool,
) -> Vec<StyledLine> {
    let trimmed = output.trim();
    if trimmed.is_empty() {
        return vec![Span::colored(Color::Success, "Done").into()];
    }
    if trimmed.starts_with("[图片已读取") {
        return vec![Span::dim_text(trimmed).into()];
    }
    match name {
        "read_file" => {
            // FileRef 缺失时从输出猜行数没有意义，直接展示首行
            vec![Span::dim_text(truncate_chars(trimmed.lines().next().unwrap_or(""), 160)).into()]
        }
        "write_file" => {
            // CC：`Wrote N lines to path` + 前 10 行预览
            let path = args_field(args_json, "path").unwrap_or_else(|| "?".into());
            let n = args_field(args_json, "content")
                .map(|c| c.lines().count())
                .unwrap_or(0);
            let mut out = vec![StyledLine(
                vec![
                    Span::dim_text("Wrote "),
                    Span::new(n.to_string()).bold(),
                    Span::dim_text(format!(" line{} to {path}", if n == 1 { "" } else { "s" })),
                ],
                LineKind::Normal,
            )];
            out.extend(write_preview_lines(args_json));
            out
        }
        "glob" => {
            let n = trimmed.lines().count();
            vec![count_line("Found", n, "files")]
        }
        "grep" => grep_result_lines(trimmed, args_json),
        "task" => {
            // 最终报告 + `[类型 | N 轮 | model: x]` 后缀
            vec![Span::dim_text(truncate_chars(trimmed.lines().next().unwrap_or(""), 200)).into()]
        }
        _ => {
            let _ = detail;
            // 全量输出，外层统一截断到预览行数 + 展开提示
            trimmed.lines().map(|l| Span::new(l).into()).collect()
        }
    }
}

/// grep 文本输出 → CC 计数文案。
fn grep_result_lines(output: &str, args_json: &str) -> Vec<StyledLine> {
    let mode = args_field(args_json, "output_mode").unwrap_or_else(|| "content".into());
    match mode.as_str() {
        "files_with_matches" => vec![count_line("Found", output.lines().count(), "file")],
        "count" => {
            // "{f}: {c}" 每行
            let total: u64 = output
                .lines()
                .filter_map(|l| l.rsplit_once(": "))
                .map(|(_, c)| c.trim().parse::<u64>().unwrap_or(0))
                .sum();
            vec![count_line("Found", total as usize, "match")]
        }
        _ => {
            let n = output.lines().count();
            vec![count_line("Found", n, "line")]
        }
    }
}

/// DisplayBlock::Output → stdout/stderr/exit 码行集。
fn output_display_lines(stdout: &str, stderr: &str, code: i32, detail: bool) -> Vec<StyledLine> {
    let mut out = Vec::new();
    let limit = if detail {
        RESULT_DETAIL_LINES
    } else {
        RESULT_PREVIEW_LINES
    };
    let stdout_trim = stdout.trim_end_matches('\n');
    let stderr_trim = stderr.trim_end_matches('\n');
    if code != 0 {
        out.push(Span::colored(Color::Error, format!("Error: exit code {code}")).into());
    }
    if !stderr_trim.is_empty() {
        let lines: Vec<&str> = stderr_trim.lines().collect();
        let show = if detail {
            lines.as_slice()
        } else {
            &lines[lines.len().saturating_sub(limit)..]
        };
        for l in show {
            out.push(Span::colored(Color::Error, *l).into());
        }
    }
    if !stdout_trim.is_empty() {
        let lines: Vec<&str> = stdout_trim.lines().collect();
        let show = &lines[lines.len().saturating_sub(limit)..];
        for l in show {
            out.push(Span::new(*l).into());
        }
    }
    if out.is_empty() {
        out.push(if code == 0 {
            Span::colored(Color::Success, "Done").into()
        } else {
            Span::colored(Color::Error, format!("Error: exit code {code}")).into()
        });
    }
    out
}

/// DisplayBlock::Diff → `│± ` 彩色行（hunk 间 dim 省略）。
fn diff_lines(diff: &[crate::tools::DiffLine], max: usize) -> Vec<StyledLine> {
    use crate::tools::DiffKind;
    let mut out = Vec::new();
    let total = diff.len();
    for line in diff.iter().take(max) {
        match line.kind {
            DiffKind::Hunk => out.push(Span::dim_text("…").into()),
            DiffKind::Delete => {
                out.push(Span::colored(Color::Error, format!("│-{}", line.text)).into())
            }
            DiffKind::Add => {
                out.push(Span::colored(Color::Success, format!("│+{}", line.text)).into())
            }
            DiffKind::Context => out.push(Span::dim_text(format!("│ {}", line.text)).into()),
        }
    }
    if total > max {
        out.push(Span::dim_text(format!("… +{} lines (ctrl+o to expand)", total - max)).into());
    }
    out
}

fn diff_stats(diff: &[crate::tools::DiffLine]) -> (usize, usize) {
    use crate::tools::DiffKind;
    let add = diff.iter().filter(|l| l.kind == DiffKind::Add).count();
    let del = diff.iter().filter(|l| l.kind == DiffKind::Delete).count();
    (add, del)
}

/// `Added 3 lines, removed 1 line`（数字 bold，单复数区分）。
fn diff_stat_line(add: usize, del: usize) -> StyledLine {
    let plural = |n: usize| if n == 1 { "" } else { "s" };
    let mut line = StyledLine::default();
    line.push(Span::dim_text("Added "));
    line.push(Span::new(add.to_string()).bold());
    line.push(Span::dim_text(format!(" line{}", plural(add))));
    line.push(Span::dim_text(", "));
    line.push(Span::dim_text("removed "));
    line.push(Span::new(del.to_string()).bold());
    line.push(Span::dim_text(format!(" line{}", plural(del))));
    line
}

/// `Read 120 lines` / `Found 3 files`（数字 bold）。
fn count_line(prefix: &str, n: usize, noun: &str) -> StyledLine {
    let plural = if n == 1 { "" } else { "s" };
    StyledLine(
        vec![
            Span::dim_text(format!("{prefix} ")),
            Span::new(n.to_string()).bold(),
            Span::dim_text(format!(" {noun}{plural}")),
        ],
        LineKind::Normal,
    )
}

/// FileRef `lines: "start-end"` → 行数。
fn file_range_count(lines: &str) -> usize {
    match lines.split_once('-') {
        Some((a, b)) => match (a.parse::<usize>(), b.parse::<usize>()) {
            (Ok(a), Ok(b)) => b.saturating_sub(a) + 1,
            _ => 1,
        },
        None => 1,
    }
}

/// write_file 落地内容预览（args.content 前 10 行）。
pub fn write_preview_lines(args_json: &str) -> Vec<StyledLine> {
    let Some(content) = args_field(args_json, "content") else {
        return Vec::new();
    };
    let lines: Vec<&str> = content.lines().collect();
    let mut out: Vec<StyledLine> = lines
        .iter()
        .take(WRITE_PREVIEW_LINES)
        .map(|l| StyledLine::from(Span::new(*l)))
        .collect();
    if lines.len() > WRITE_PREVIEW_LINES {
        out.push(
            Span::dim_text(format!(
                "… +{} lines (ctrl+o to expand)",
                lines.len() - WRITE_PREVIEW_LINES
            ))
            .into(),
        );
    }
    out
}

/// 解析 todo_write args → 清单项。
pub fn parse_todos(args_json: &str) -> Vec<TodoItem> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(args_json) else {
        return Vec::new();
    };
    let Some(items) = v.get("todos").and_then(|t| t.as_array()) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|it| {
            Some(TodoItem {
                content: it.get("content")?.as_str()?.to_string(),
                status: it
                    .get("status")
                    .and_then(|s| s.as_str())
                    .unwrap_or("pending")
                    .to_string(),
                active_form: it
                    .get("active_form")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_names_map_to_cc_style() {
        assert_eq!(tool_display_name("read_file"), "Read");
        assert_eq!(tool_display_name("write_file"), "Write");
        assert_eq!(tool_display_name("edit_file"), "Update");
        assert_eq!(tool_display_name("bash"), "Bash");
        assert_eq!(tool_display_name("grep"), "Search");
        assert_eq!(tool_display_name("task"), "Agent");
        assert_eq!(tool_display_name("custom_x"), "custom_x", "未知工具兜底");
    }

    #[test]
    fn bash_summary_truncated_at_160() {
        let long = "echo ".repeat(60); // 300 chars
        let s = tool_args_summary("bash", &format!(r#"{{"command":"{long}"}}"#));
        assert!(
            s.chars().count() <= CMD_SUMMARY_CHARS + 1,
            "{}",
            s.chars().count()
        );
    }

    #[test]
    fn grep_summary_includes_pattern_and_mode() {
        let s = tool_args_summary("grep", r#"{"pattern":"foo","output_mode":"content"}"#);
        assert_eq!(s, r#"pattern: "foo", output_mode: "content""#);
        let s = tool_args_summary("grep", r#"{"pattern":"foo"}"#);
        assert_eq!(s, r#"pattern: "foo""#);
    }

    #[test]
    fn diff_result_stat_and_colors() {
        use crate::tools::{DiffKind, DiffLine};
        let lines = tool_result_lines(
            "edit_file",
            r#"{"path":"a.rs"}"#,
            true,
            "ok",
            Some(&DisplayBlock::Diff {
                path: "a.rs".into(),
                lines: vec![
                    DiffLine {
                        kind: DiffKind::Hunk,
                        text: "".into(),
                    },
                    DiffLine {
                        kind: DiffKind::Context,
                        text: "ctx".into(),
                    },
                    DiffLine {
                        kind: DiffKind::Delete,
                        text: "old".into(),
                    },
                    DiffLine {
                        kind: DiffKind::Add,
                        text: "new".into(),
                    },
                    DiffLine {
                        kind: DiffKind::Add,
                        text: "new2".into(),
                    },
                ],
            }),
            false,
        );
        assert_eq!(lines[0].text(), "Added 2 lines, removed 1 line");
        assert_eq!(lines[1].text(), "…", "hunk → dim 省略");
        assert_eq!(lines[2].text(), "│ ctx");
        assert_eq!(lines[3].text(), "│-old");
        assert_eq!(lines[4].text(), "│+new");
        assert_eq!(lines[3].0[0].color, Color::Error);
        assert_eq!(lines[4].0[0].color, Color::Success);
    }

    #[test]
    fn output_result_stderr_red_exit_code_line() {
        let out = DisplayBlock::Output {
            stdout: "line1\nline2".into(),
            stderr: "boom".into(),
            code: 2,
        };
        let lines = tool_result_lines("bash", r#"{"command":"x"}"#, true, "", Some(&out), false);
        assert_eq!(lines[0].text(), "Error: exit code 2");
        assert_eq!(lines[1].text(), "boom");
        assert_eq!(lines[1].0[0].color, Color::Error);
        assert_eq!(lines[2].text(), "line1");
        assert_eq!(lines[3].text(), "line2");
    }

    #[test]
    fn empty_success_output_is_done() {
        let lines = tool_result_lines("bash", r#"{"command":"true"}"#, true, "", None, false);
        assert_eq!(lines[0].text(), "Done");
        assert_eq!(lines[0].0[0].color, Color::Success);
    }

    #[test]
    fn fileref_counts_range() {
        let r = DisplayBlock::FileRef {
            path: "src/main.rs".into(),
            lines: "10-129".into(),
        };
        let lines = tool_result_lines("read_file", "{}", true, "", Some(&r), false);
        assert_eq!(lines[0].text(), "Read 120 lines");
        let n = &lines[0].0[1];
        assert!(n.bold && n.text == "120", "数字 bold");
    }

    #[test]
    fn long_plain_output_truncated_with_expand_hint() {
        let body = (0..30)
            .map(|i| format!("L{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let lines = tool_result_lines("bash", "{}", true, &body, None, false);
        assert_eq!(lines.len(), 6, "预览 5 行 + 展开提示");
        assert!(lines[5].text().contains("+25 lines (ctrl+o to expand)"));
        // 详情模式放开
        let detail = tool_result_lines("bash", "{}", true, &body, None, true);
        assert_eq!(detail.len(), 30);
    }

    #[test]
    fn failure_lines_are_red() {
        let lines = tool_result_lines("read_file", "{}", false, "File not found: x", None, false);
        assert_eq!(lines[0].text(), "File not found: x");
        assert_eq!(lines[0].0[0].color, Color::Error);
    }

    #[test]
    fn write_preview_shows_first_ten_lines() {
        let content = (0..15)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let args = serde_json::json!({ "path": "a.rs", "content": content }).to_string();
        let lines = write_preview_lines(&args);
        assert_eq!(lines.len(), 11, "前 10 行 + 提示");
        assert!(lines[10].text().contains("+5 lines (ctrl+o to expand)"));
    }

    #[test]
    fn todo_args_parsed() {
        let args = serde_json::json!({ "todos": [
            { "content": "写测试", "status": "completed", "active_form": "写测试" },
            { "content": "跑 CI", "status": "in_progress", "active_form": "跑 CI 中" },
            { "content": "发版", "status": "pending", "active_form": "发版" }
        ]})
        .to_string();
        let todos = parse_todos(&args);
        assert_eq!(todos.len(), 3);
        assert_eq!(todos[0].status, "completed");
        assert_eq!(todos[1].active_form, "跑 CI 中");
    }

    #[test]
    fn count_line_pluralization() {
        assert_eq!(count_line("Found", 1, "file").text(), "Found 1 file");
        assert_eq!(count_line("Found", 3, "file").text(), "Found 3 files");
    }
}
