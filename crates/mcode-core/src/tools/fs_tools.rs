//! 文件类内置工具：read_file / write_file / edit_file / glob / grep。
//!
//! 关键语义：
//! - read_file：`cat -n` 行号格式、offset/limit 分段、图片转 base64 附件、
//!   二进制拒绝、读取登记（read-before-write 的"读"半边）；
//! - write_file：覆盖已有文件前校验已读+未漂移；产出 diff；
//! - edit_file：old_string 必须唯一（replace_all 例外）、替换计数反馈；
//! - glob：结果按 mtime 降序（最近改动的排前面）；
//! - grep：正则 + glob 过滤 + 三种输出模式（content / files_with_matches /
//!   count）+ 上下文行。

use super::{DisplayBlock, Tool, ToolContext, ToolDefinition, ToolResult};
use crate::util::paths::contain_to_workspace;
use anyhow::{Context as _, Result};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// read_file
// ---------------------------------------------------------------------------

const READ_DEFAULT_LINES: usize = 2000;
const MAX_LINE_CHARS: usize = 20_000;

pub struct ReadFileTool;

/// cat -n 风格行号：右对齐 6 位 + tab。
pub fn format_numbered_line(idx: usize, text: &str) -> String {
    format!("{:>6}\t{}", idx + 1, text)
}

#[async_trait::async_trait]
impl Tool for ReadFileTool {
    fn def(&self) -> ToolDefinition {
        ToolDefinition {
            name: "read_file".into(),
            description:
                "读取工作区内文件。默认读前 2000 行，行号格式为 cat -n（右对齐行号 + tab）。\
                支持图片（转 base64 附件，多模态模型可见）。写入前必须先读过目标文件。"
                    .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "文件路径（相对工作区或绝对路径）" },
                    "offset": { "type": "number", "description": "起始行号（从 1 计）" },
                    "limit": { "type": "number", "description": "最多读取行数，默认 2000" }
                },
                "required": ["path"]
            }),
            is_read_only: true,
        }
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> Result<ToolResult> {
        let path_str = input["path"].as_str().unwrap_or_default();
        if path_str.is_empty() {
            return Ok(ToolResult::err("read_file: path 不能为空"));
        }
        let path = contain_to_workspace(&ctx.cwd, path_str)?;
        if path.is_dir() {
            return Ok(ToolResult::err(format!(
                "read_file: {} 是目录，请用 glob 列出文件",
                path_str
            )));
        }
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => return Ok(ToolResult::err(format!("read_file {path_str}: {e}"))),
        };
        if crate::util::paths::is_probably_binary(&bytes) {
            return Ok(ToolResult::err(format!(
                "read_file {path_str}: 二进制文件无法以文本读取"
            )));
        }
        // 图片 → base64 附件
        if let Some((mime, data)) = image_attachment(&path, &bytes) {
            let msg = format!(
                "[图片已读取: {} ({mime})，已作为附件提供给多模态模型]",
                path.display()
            );
            return Ok(ToolResult::ok(msg).with_image(mime, data));
        }
        let text = String::from_utf8_lossy(&bytes);
        let all_lines: Vec<&str> = text.lines().collect();
        let total = all_lines.len();
        let offset = input["offset"].as_u64().unwrap_or(1).max(1) as usize;
        let limit = input["limit"].as_u64().unwrap_or(READ_DEFAULT_LINES as u64) as usize;
        let start = (offset - 1).min(total);
        let end = (start + limit).min(total);
        let mut body = String::new();
        for (i, line) in all_lines[start..end].iter().enumerate() {
            let line = truncate_line(line);
            body.push_str(&format_numbered_line(start + i, &line));
            body.push('\n');
        }
        let mut out = body;
        if end < total {
            out.push_str(&format!(
                "\n[文件共 {total} 行，已显示 {}-{}。用 offset 参数继续读取]",
                start + 1,
                end
            ));
        }
        if out.is_empty() {
            out = "[空文件]".into();
        }
        // 登记完整内容（read-before-write 蓝本是全文件，不是窗口）
        {
            let mut shared = ctx.shared.lock().expect("工具状态锁");
            shared.files.record_read(&path, &text);
        }
        Ok(ToolResult::ok(out).with_display(DisplayBlock::FileRef {
            path: path.display().to_string(),
            lines: format!("{}-{}", start + 1, end),
        }))
    }
}

fn truncate_line(line: &str) -> String {
    if line.chars().count() <= MAX_LINE_CHARS {
        return line.to_string();
    }
    let head: String = line.chars().take(MAX_LINE_CHARS).collect();
    format!("{head}…[行超长截断]")
}

fn image_attachment(path: &std::path::Path, bytes: &[u8]) -> Option<(&'static str, String)> {
    const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    let mime = match ext.as_deref() {
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        _ => return None,
    };
    if bytes.len() > MAX_IMAGE_BYTES {
        return None;
    }
    use base64::Engine;
    Some((
        mime,
        base64::engine::general_purpose::STANDARD.encode(bytes),
    ))
}

// ---------------------------------------------------------------------------
// write_file
// ---------------------------------------------------------------------------

pub struct WriteFileTool;

#[async_trait::async_trait]
impl Tool for WriteFileTool {
    fn def(&self) -> ToolDefinition {
        ToolDefinition {
            name: "write_file".into(),
            description: "写入文件（整体覆盖或新建）。覆盖已存在的文件前必须先用 read_file 读取。\
                父目录不存在时自动创建。"
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "文件路径" },
                    "content": { "type": "string", "description": "完整文件内容" }
                },
                "required": ["path", "content"]
            }),
            is_read_only: false,
        }
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> Result<ToolResult> {
        let path_str = input["path"].as_str().unwrap_or_default();
        let content = input["content"].as_str().unwrap_or("");
        if path_str.is_empty() {
            return Ok(ToolResult::err("write_file: path 不能为空"));
        }
        let path = contain_to_workspace(&ctx.cwd, path_str)?;
        // read-before-write
        {
            let shared = ctx.shared.lock().expect("工具状态锁");
            if let Err(e) = shared.files.ensure_writable(&path) {
                return Ok(ToolResult::err(format!("write_file: {e}")));
            }
        }
        let old = std::fs::read_to_string(&path).unwrap_or_default();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建目录 {} 失败", parent.display()))?;
        }
        if let Err(e) = std::fs::write(&path, content) {
            return Ok(ToolResult::err(format!("write_file {path_str}: {e}")));
        }
        let lines = super::unified_diff(&old, content, path_str);
        let (add, del) = super::diff_stats(&lines);
        {
            let mut shared = ctx.shared.lock().expect("工具状态锁");
            shared.files.record_write(&path, content);
        }
        let verb = if old.is_empty() { "新建" } else { "覆盖" };
        Ok(ToolResult::ok(format!(
            "{verb} {path_str}（+{add}/-{del} 行，共 {} 字节）",
            content.len()
        ))
        .with_display(DisplayBlock::Diff {
            path: path_str.into(),
            lines,
        }))
    }
}

// ---------------------------------------------------------------------------
// edit_file
// ---------------------------------------------------------------------------

pub struct EditFileTool;

#[async_trait::async_trait]
impl Tool for EditFileTool {
    fn def(&self) -> ToolDefinition {
        ToolDefinition {
            name: "edit_file".into(),
            description: "精确替换文件内容。old_string 必须与文件内容完全一致且唯一；\
                替换多处时设 replace_all=true。写入前必须先用 read_file 读取。"
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "文件路径" },
                    "old_string": { "type": "string", "description": "要替换的原文（必须精确匹配且唯一，除非 replace_all）" },
                    "new_string": { "type": "string", "description": "替换后的内容" },
                    "replace_all": { "type": "boolean", "description": "替换全部出现（默认 false）" }
                },
                "required": ["path", "old_string", "new_string"]
            }),
            is_read_only: false,
        }
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> Result<ToolResult> {
        let path_str = input["path"].as_str().unwrap_or_default();
        let old_string = input["old_string"].as_str().unwrap_or_default();
        let new_string = input["new_string"].as_str().unwrap_or_default();
        let replace_all = input["replace_all"].as_bool().unwrap_or(false);
        if path_str.is_empty() || old_string.is_empty() {
            return Ok(ToolResult::err("edit_file: path 与 old_string 不能为空"));
        }
        if old_string == new_string {
            return Ok(ToolResult::err(
                "edit_file: old_string 与 new_string 相同，无需编辑",
            ));
        }
        let path = contain_to_workspace(&ctx.cwd, path_str)?;
        {
            let shared = ctx.shared.lock().expect("工具状态锁");
            if let Err(e) = shared.files.ensure_writable(&path) {
                return Ok(ToolResult::err(format!("edit_file: {e}")));
            }
        }
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => return Ok(ToolResult::err(format!("edit_file {path_str}: {e}"))),
        };
        let count = content.matches(old_string).count();
        if count == 0 {
            return Ok(ToolResult::err(format!(
                "edit_file: old_string 在 {path_str} 中未找到。请精确复制文件内容（含缩进）"
            )));
        }
        if count > 1 && !replace_all {
            return Ok(ToolResult::err(format!(
                "edit_file: old_string 在 {path_str} 中出现 {count} 次。请扩大上下文使其唯一，或设 replace_all=true"
            )));
        }
        let updated = if replace_all {
            content.replace(old_string, new_string)
        } else {
            content.replacen(old_string, new_string, 1)
        };
        if let Err(e) = std::fs::write(&path, &updated) {
            return Ok(ToolResult::err(format!("edit_file {path_str}: {e}")));
        }
        let lines = super::unified_diff(&content, &updated, path_str);
        let (add, del) = super::diff_stats(&lines);
        {
            let mut shared = ctx.shared.lock().expect("工具状态锁");
            shared.files.record_write(&path, &updated);
        }
        let replaced = if replace_all { count } else { 1 };
        Ok(ToolResult::ok(format!(
            "已编辑 {path_str}（替换 {replaced} 处，+{add}/-{del} 行）"
        ))
        .with_display(DisplayBlock::Diff {
            path: path_str.into(),
            lines,
        }))
    }
}

// ---------------------------------------------------------------------------
// glob
// ---------------------------------------------------------------------------

pub struct GlobTool;

#[async_trait::async_trait]
impl Tool for GlobTool {
    fn def(&self) -> ToolDefinition {
        ToolDefinition {
            name: "glob".into(),
            description: "按 glob 模式匹配文件路径（如 \"src/**/*.rs\"）。结果按修改时间降序\
                （最近改动的在前）。"
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "glob 模式，如 **/*.ts" },
                    "path": { "type": "string", "description": "搜索根目录（默认工作区根）" }
                },
                "required": ["pattern"]
            }),
            is_read_only: true,
        }
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> Result<ToolResult> {
        let pattern = input["pattern"].as_str().unwrap_or_default();
        if pattern.is_empty() {
            return Ok(ToolResult::err("glob: pattern 不能为空"));
        }
        let root = match input["path"].as_str() {
            Some(p) if !p.is_empty() => contain_to_workspace(&ctx.cwd, p)?,
            _ => ctx.cwd.canonicalize().unwrap_or_else(|_| ctx.cwd.clone()),
        };
        let matcher = match globset::GlobBuilder::new(pattern)
            .literal_separator(false)
            .build()
        {
            Ok(g) => g,
            Err(e) => return Ok(ToolResult::err(format!("glob: 非法模式 '{pattern}': {e}"))),
        };
        let set = globset::GlobSetBuilder::new()
            .add(matcher)
            .build()
            .map_err(|e| anyhow::anyhow!("glob: {e}"))?;

        let mut matches: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
        for entry in walkdir::WalkDir::new(&root)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if !entry.file_type().is_file() {
                continue;
            }
            let rel = match entry.path().strip_prefix(&root) {
                Ok(r) => r,
                Err(_) => continue,
            };
            let rel_str = rel.to_string_lossy().replace('\\', "/");
            if set.is_match(&rel_str) || set.is_match(entry.path()) {
                let mtime = entry
                    .metadata()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                matches.push((mtime, entry.path().to_path_buf()));
                if matches.len() >= 500 {
                    break;
                }
            }
        }
        // mtime 降序
        matches.sort_by_key(|(t, _)| std::cmp::Reverse(*t));
        let mut out = String::new();
        for (_, p) in &matches {
            let rel = p.strip_prefix(&root).unwrap_or(p);
            out.push_str(&rel.to_string_lossy().replace('\\', "/"));
            out.push('\n');
        }
        if out.is_empty() {
            return Ok(ToolResult::ok(format!("glob '{pattern}': 无匹配文件")));
        }
        Ok(ToolResult::ok(out))
    }
}

// ---------------------------------------------------------------------------
// grep
// ---------------------------------------------------------------------------

pub struct GrepTool;

#[async_trait::async_trait]
impl Tool for GrepTool {
    fn def(&self) -> ToolDefinition {
        ToolDefinition {
            name: "grep".into(),
            description: "在工作区内按正则搜索文件内容。output_mode: content（带行号内容，默认）\
                / files_with_matches（仅文件名）/ count（每文件计数）。支持 include glob 过滤\
                （如 \"*.rs\"）与 context 上下文行数。"
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "正则表达式" },
                    "include": { "type": "string", "description": "文件名 glob 过滤，如 *.ts" },
                    "path": { "type": "string", "description": "搜索根目录（默认工作区根）" },
                    "output_mode": { "type": "string", "description": "content | files_with_matches | count" },
                    "context": { "type": "number", "description": "上下文行数（仅 content 模式）" }
                },
                "required": ["pattern"]
            }),
            is_read_only: true,
        }
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> Result<ToolResult> {
        let pattern = input["pattern"].as_str().unwrap_or_default();
        if pattern.is_empty() {
            return Ok(ToolResult::err("grep: pattern 不能为空"));
        }
        let re = match regex::RegexBuilder::new(pattern)
            .size_limit(10 * 1024 * 1024)
            .build()
        {
            Ok(r) => r,
            Err(e) => return Ok(ToolResult::err(format!("grep: 非法正则 '{pattern}': {e}"))),
        };
        let include = input["include"].as_str().and_then(|s| {
            globset::GlobBuilder::new(s)
                .literal_separator(false)
                .build()
                .ok()
        });
        let include_set = match include {
            Some(g) => globset::GlobSetBuilder::new()
                .add(g)
                .build()
                .ok()
                .map(Arc::new),
            None => None,
        };
        let root = match input["path"].as_str() {
            Some(p) if !p.is_empty() => contain_to_workspace(&ctx.cwd, p)?,
            _ => ctx.cwd.canonicalize().unwrap_or_else(|_| ctx.cwd.clone()),
        };
        let mode = input["output_mode"].as_str().unwrap_or("content");
        let context_lines = input["context"].as_u64().unwrap_or(0).min(10) as usize;

        let mut files_matched: Vec<String> = Vec::new();
        let mut counts: Vec<(String, usize)> = Vec::new();
        let mut content_out = String::new();
        let mut total_hits = 0usize;
        let max_files = 2000;

        for entry in walkdir::WalkDir::new(&root)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if !entry.file_type().is_file() {
                continue;
            }
            if files_matched.len() >= max_files {
                content_out.push_str("\n[已达文件数上限，结果不完整]\n");
                break;
            }
            let abs = entry.path();
            if let Some(set) = &include_set {
                let name = entry.file_name().to_string_lossy();
                if !set.is_match(name.as_ref()) {
                    continue;
                }
            }
            let bytes = match std::fs::read(abs) {
                Ok(b) => b,
                Err(_) => continue,
            };
            if crate::util::paths::is_probably_binary(&bytes) {
                continue;
            }
            let text = String::from_utf8_lossy(&bytes);
            let lines: Vec<&str> = text.lines().collect();
            let hit_lines: Vec<usize> = lines
                .iter()
                .enumerate()
                .filter(|(_, l)| re.is_match(l))
                .map(|(i, _)| i)
                .collect();
            if hit_lines.is_empty() {
                continue;
            }
            let rel = abs.strip_prefix(&root).unwrap_or(abs);
            let rel_str = rel.to_string_lossy().replace('\\', "/");
            total_hits += hit_lines.len();
            match mode {
                "files_with_matches" => files_matched.push(rel_str),
                "count" => counts.push((rel_str, hit_lines.len())),
                _ => {
                    // content 模式：合并相邻命中的上下文窗口
                    let shown = render_content(&lines, &hit_lines, context_lines);
                    content_out.push_str(&format!("{rel_str}:\n{shown}\n"));
                }
            }
        }

        let out = match mode {
            "files_with_matches" => {
                files_matched.sort();
                if files_matched.is_empty() {
                    format!("grep '{pattern}': 无匹配")
                } else {
                    let mut s = format!("[{total_hits} 处命中]\n");
                    s.push_str(&files_matched.join("\n"));
                    s.push('\n');
                    s
                }
            }
            "count" => {
                counts.sort();
                if counts.is_empty() {
                    format!("grep '{pattern}': 无匹配")
                } else {
                    let mut s = format!("[{total_hits} 处命中]\n");
                    for (f, c) in &counts {
                        s.push_str(&format!("{f}: {c}\n"));
                    }
                    s
                }
            }
            _ => {
                if content_out.is_empty() {
                    format!("grep '{pattern}': 无匹配")
                } else {
                    content_out
                }
            }
        };
        Ok(ToolResult::ok(super::clamp_output(
            &ctx.shared.lock().expect("工具状态锁"),
            out,
        )))
    }
}

/// 合并相邻命中行为窗口渲染，带 `N-` / `N:` 前缀与窗口分隔 `--`。
fn render_content(lines: &[&str], hits: &[usize], context: usize) -> String {
    let mut windows: Vec<(usize, usize)> = Vec::new();
    for &h in hits {
        let start = h.saturating_sub(context);
        let end = (h + context).min(lines.len().saturating_sub(1));
        match windows.last_mut() {
            Some((_, last_end)) if start <= *last_end + 1 => {
                *last_end = (*last_end).max(end);
            }
            _ => windows.push((start, end)),
        }
    }
    let mut out = String::new();
    for (i, (start, end)) in windows.iter().enumerate() {
        if i > 0 {
            out.push_str("--\n");
        }
        for (l, line) in lines.iter().enumerate().take(*end + 1).skip(*start) {
            let prefix = if hits.contains(&l) { ":" } else { "-" };
            out.push_str(&format!("{l}{}{line}\n", prefix));
        }
    }
    out
}

pub fn all_fs_tools() -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(ReadFileTool),
        Arc::new(WriteFileTool),
        Arc::new(EditFileTool),
        Arc::new(GlobTool),
        Arc::new(GrepTool),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn ctx_in(dir: &std::path::Path) -> ToolContext {
        let (ctx, _tx) = ToolContext::for_tests(dir.to_path_buf());
        ctx
    }

    async fn run(t: &dyn Tool, input: Value, ctx: &ToolContext) -> ToolResult {
        t.execute(input, ctx).await.unwrap()
    }

    fn unique_dir(tag: &str) -> std::path::PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::env::temp_dir()
            .join(format!(
                "mcode-fstools-{}-{n}-{}",
                std::process::id(),
                std::thread::current()
                    .name()
                    .unwrap_or("t")
                    .replace("::", "-")
            ))
            .with_extension(tag)
    }

    async fn setup_workspace() -> (std::path::PathBuf, ToolContext) {
        let dir = unique_dir("ws");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("src/main.rs"),
            "fn main() {\n    println!(\"hello\");\n}\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("src/lib.rs"),
            "pub fn add(a: u32, b: u32) -> u32 {\n    a + b\n}\n",
        )
        .unwrap();
        std::fs::write(dir.join("README.md"), "# demo\n\nhello world\n").unwrap();
        let ctx = ctx_in(&dir).await;
        (dir, ctx)
    }

    #[tokio::test]
    async fn read_file_numbers_lines_and_registers_state() {
        let (dir, ctx) = setup_workspace().await;
        let out = run(&ReadFileTool, json!({"path": "src/main.rs"}), &ctx).await;
        assert!(out.success);
        assert!(out.output.contains("     1\tfn main() {"));
        assert!(out.output.contains("     2\t    println!(\"hello\");"));
        let p = dir.join("src/main.rs").canonicalize().unwrap();
        assert!(
            ctx.shared.lock().unwrap().files.has_read(&p),
            "读取必须登记进 FileStateTracker"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn read_file_offset_limit_and_tail_hint() {
        let (dir, ctx) = setup_workspace().await;
        let out = run(
            &ReadFileTool,
            json!({"path": "src/main.rs", "offset": 2, "limit": 1}),
            &ctx,
        )
        .await;
        assert!(out.output.contains("     2\t"));
        assert!(!out.output.contains("     1\t"));
        assert!(out.output.contains("offset 参数继续读取"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn write_file_requires_read_first_then_allows() {
        let (dir, ctx) = setup_workspace().await;
        // 未读直接覆盖 → 拒绝
        let out = run(
            &WriteFileTool,
            json!({"path": "src/main.rs", "content": "x"}),
            &ctx,
        )
        .await;
        assert!(!out.success);
        assert!(out.output.contains("尚未读取"));
        // 读后覆盖 → 成功 + diff
        run(&ReadFileTool, json!({"path": "src/main.rs"}), &ctx).await;
        let out = run(
            &WriteFileTool,
            json!({"path": "src/main.rs", "content": "fn main() {}\n"}),
            &ctx,
        )
        .await;
        assert!(out.success, "{}", out.output);
        assert!(out.output.contains("覆盖"));
        // 新文件无需读
        let out = run(
            &WriteFileTool,
            json!({"path": "src/new.rs", "content": "pub fn f() {}"}),
            &ctx,
        )
        .await;
        assert!(out.success, "{}", out.output);
        assert!(out.output.contains("新建"));
        assert!(dir.join("src/new.rs").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn edit_file_uniqueness_and_replace_all() {
        let (dir, ctx) = setup_workspace().await;
        run(&ReadFileTool, json!({"path": "src/lib.rs"}), &ctx).await;
        // 多处出现且未 replace_all → 拒绝
        let out = run(
            &EditFileTool,
            json!({"path": "src/lib.rs", "old_string": "u32", "new_string": "i64"}),
            &ctx,
        )
        .await;
        assert!(!out.success);
        assert!(out.output.contains("3 次"), "{}", out.output);
        // replace_all → 全替换
        let out = run(
            &EditFileTool,
            json!({"path": "src/lib.rs", "old_string": "u32", "new_string": "i64", "replace_all": true}),
            &ctx,
        )
        .await;
        assert!(out.success, "{}", out.output);
        let after = std::fs::read_to_string(dir.join("src/lib.rs")).unwrap();
        assert!(!after.contains("u32"));
        assert!(after.contains("pub fn add(a: i64, b: i64) -> i64"));
        // old=new 拒绝
        let out = run(
            &EditFileTool,
            json!({"path": "src/lib.rs", "old_string": "i64", "new_string": "i64"}),
            &ctx,
        )
        .await;
        assert!(!out.success);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn edit_file_after_external_change_requires_reread() {
        let (dir, ctx) = setup_workspace().await;
        run(&ReadFileTool, json!({"path": "src/main.rs"}), &ctx).await;
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(dir.join("src/main.rs"), "changed externally\n").unwrap();
        let out = run(
            &EditFileTool,
            json!({"path": "src/main.rs", "old_string": "changed externally", "new_string": "x"}),
            &ctx,
        )
        .await;
        assert!(!out.success);
        assert!(out.output.contains("重新 read_file"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn glob_sorts_by_mtime_and_filters() {
        let (dir, ctx) = setup_workspace().await;
        std::thread::sleep(std::time::Duration::from_millis(20));
        // 后改 lib.rs → glob 应把它排前面
        std::fs::write(
            dir.join("src/lib.rs"),
            "pub fn add(a: u32, b: u32) -> u32 {\n    a + b\n}\n// touched\n",
        )
        .unwrap();
        let out = run(&GlobTool, json!({"pattern": "src/**/*.rs"}), &ctx).await;
        assert!(out.success);
        let first = out.output.lines().next().unwrap();
        assert_eq!(first, "src/lib.rs", "mtime 降序，实际: {}", out.output);
        assert!(out.output.contains("src/main.rs"));
        // md 不匹配
        let out = run(&GlobTool, json!({"pattern": "src/**/*.rs"}), &ctx).await;
        assert!(!out.output.contains("README.md"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn grep_content_mode_with_context() {
        let (dir, ctx) = setup_workspace().await;
        let out = run(
            &GrepTool,
            json!({"pattern": "hello", "include": "*.rs"}),
            &ctx,
        )
        .await;
        assert!(out.success);
        assert!(out.output.contains("src/main.rs"));
        assert!(out.output.contains("1:    println!(\"hello\");"));
        // include 过滤掉 md
        let out_all = run(&GrepTool, json!({"pattern": "hello"}), &ctx).await;
        assert!(out_all.output.contains("README.md"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn grep_count_and_files_modes() {
        let (dir, ctx) = setup_workspace().await;
        let out = run(
            &GrepTool,
            json!({"pattern": "fn ", "output_mode": "count"}),
            &ctx,
        )
        .await;
        assert!(out.output.contains("src/main.rs: 1"));
        assert!(out.output.contains("src/lib.rs: 1"));
        let out = run(
            &GrepTool,
            json!({"pattern": "pub ", "output_mode": "files_with_matches"}),
            &ctx,
        )
        .await;
        assert!(out.output.contains("src/lib.rs"), "{}", out.output);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn read_rejects_directory_and_missing() {
        let (dir, ctx) = setup_workspace().await;
        let out = run(&ReadFileTool, json!({"path": "src"}), &ctx).await;
        assert!(!out.success);
        assert!(out.output.contains("目录"));
        let out = run(&ReadFileTool, json!({"path": "nope.txt"}), &ctx).await;
        assert!(!out.success);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn numbered_line_format_is_six_wide() {
        assert_eq!(format_numbered_line(0, "abc"), "     1\tabc");
        assert_eq!(format_numbered_line(999, "x"), "  1000\tx");
    }

    #[test]
    fn content_windows_merge_adjacent_hits() {
        let lines: Vec<&str> = (0..10)
            .map(|i| match i {
                3 => "hit A",
                5 => "hit B",
                _ => "filler",
            })
            .collect();
        let out = render_content(&lines, &[3, 5], 1);
        // 相邻窗口应合并（3 与 5 的 ±1 窗口重叠），且只有一个 -- 分隔都没有
        assert!(!out.contains("--"));
        assert!(out.contains("3:hit A"));
        assert!(out.contains("4-filler"));
        assert!(out.contains("5:hit B"));
    }
}
