//! bash 工具：cwd 跨调用持久、超时、后台任务（run_in_background）与收割。
//!
//! 对齐 CC 关键语义：
//! - `cd` 在同会话内生效（BashSession.cwd 持久化），命令失败不更新；
//! - `run_in_background=true` 立即返回任务 id，输出落盘可增量读；
//! - `bash_output` 收割已结束任务（完整输出 + 退出码），运行中的返回增量；
//! - 超时（默认 120s，可 timeout 参数）杀进程树并报告；
//! - Windows 下经由 cmd /C 执行（对齐 CC 跨平台策略），Unix 走 sh。

use super::{state::TaskStatus, DisplayBlock, Tool, ToolContext, ToolDefinition, ToolResult};
use anyhow::Result;
use serde_json::{json, Value};
use std::path::Path;
use std::process::Stdio;

const DEFAULT_TIMEOUT_SECS: u64 = 120;
const MAX_TIMEOUT_SECS: u64 = 600;
const MAX_OUTPUT_BYTES: usize = 256 * 1024;

pub struct BashTool;

impl BashTool {
    fn shell_command(cmd: &str, cwd: &Path) -> std::process::Command {
        #[cfg(windows)]
        {
            let mut c = build_windows_cmd(cmd, cwd);
            crate::util::hide_console_window_sync(&mut c);
            c
        }
        #[cfg(not(windows))]
        {
            let mut c = std::process::Command::new("sh");
            c.arg("-c").arg(cmd).current_dir(cwd);
            c
        }
    }
}

#[cfg(windows)]
fn build_windows_cmd(cmd: &str, cwd: &Path) -> std::process::Command {
    use std::os::windows::process::CommandExt;
    let mut c = std::process::Command::new("cmd");
    // 同 hooks::shell_command：raw_arg 避免 MSVCRT 引号转义 × cmd 引号剥离互坑
    c.arg("/C").raw_arg(cmd).current_dir(cwd);
    c
}

/// 同步执行一条命令并收集输出（前台/后台共用；在 blocking 线程上调用）。
fn run_sync(
    cmd: &str,
    cwd: &Path,
    timeout: std::time::Duration,
    cancel: &tokio::sync::watch::Receiver<bool>,
) -> (String, String, Option<i32>, bool) {
    let mut command = BashTool::shell_command(cmd, cwd);
    command.stdin(Stdio::null());
    match command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => wait_with_timeout_and_cancel(child, timeout, cancel),
        Err(_) => (String::new(), String::new(), None, true),
    }
}

/// 管道收割预算：进程结束后等 reader 线程吐残余的最长时间（孙进程
/// 继承管道写端的极端情况下兜底，不 hang）。
const REAP_BUDGET: std::time::Duration = std::time::Duration::from_secs(2);

/// 带超时与取消的等待。管道读交给独立线程——管道 read 是阻塞语义，
/// 在轮询线程里读会让「无输出的长命令」饿死 deadline 检查（超时永不
/// 触发，sleep 类命令跑满全程还报成功）。主线程只轮询退出状态/超时/
/// 取消/截断；kill 或自然退出后写端关闭，reader 线程到 EOF 自然收尾。
fn wait_with_timeout_and_cancel(
    mut child: std::process::Child,
    timeout: std::time::Duration,
    cancel: &tokio::sync::watch::Receiver<bool>,
) -> (String, String, Option<i32>, bool) {
    let (out_tx, out_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let (err_tx, err_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let capped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    spawn_pipe_reader(child.stdout.take(), out_tx, capped.clone());
    spawn_pipe_reader(child.stderr.take(), err_tx, capped.clone());

    let deadline = std::time::Instant::now() + timeout;
    let mut killed = false;
    let mut code: Option<i32> = None;
    loop {
        if !killed && (*cancel.borrow() || capped.load(std::sync::atomic::Ordering::Relaxed)) {
            let _ = child.kill();
            killed = true;
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                code = status.code();
                break;
            }
            Ok(None) => {
                if std::time::Instant::now() >= deadline && !killed {
                    let _ = child.kill();
                    killed = true;
                    let _ = child.wait(); // kill 后收尸
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(_) => break,
        }
    }

    // 收割残余输出（带预算；Disconnected = reader 已收尾，立即返回）
    let collect = |rx: std::sync::mpsc::Receiver<Vec<u8>>| {
        let start = std::time::Instant::now();
        let mut buf = Vec::new();
        loop {
            let elapsed = start.elapsed();
            if elapsed >= REAP_BUDGET {
                break;
            }
            match rx.recv_timeout(REAP_BUDGET - elapsed) {
                Ok(chunk) => buf.extend_from_slice(&chunk),
                Err(_) => break,
            }
        }
        buf
    };
    let stdout_buf = collect(out_rx);
    let stderr_buf = collect(err_rx);
    (
        String::from_utf8_lossy(&stdout_buf).into_owned(),
        String::from_utf8_lossy(&stderr_buf).into_owned(),
        code,
        killed,
    )
}

/// 管道读线程：增量把字节发回主线程；单流读满上限后置位 capped
/// （由主线程统一 kill），随后停止读取。
fn spawn_pipe_reader<R: std::io::Read + Send + 'static>(
    pipe: Option<R>,
    tx: std::sync::mpsc::Sender<Vec<u8>>,
    capped: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    std::thread::spawn(move || {
        let Some(mut s) = pipe else { return };
        let mut total = 0usize;
        let mut chunk = [0u8; 8192];
        loop {
            match s.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    total += n;
                    if tx.send(chunk[..n].to_vec()).is_err() {
                        break; // 主线程已放弃收割
                    }
                    if total >= MAX_OUTPUT_BYTES {
                        capped.store(true, std::sync::atomic::Ordering::Relaxed);
                        break;
                    }
                }
            }
        }
    });
}

#[async_trait::async_trait]
impl Tool for BashTool {
    fn def(&self) -> ToolDefinition {
        ToolDefinition {
            name: "bash".into(),
            description: "在工作区执行 shell 命令。会话内 cd 持久生效；run_in_background=true \
                让命令后台执行并立即返回任务 id（后续用 bash_output 读取输出）。"
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "要执行的命令" },
                    "timeout": { "type": "number", "description": "超时秒数（默认 120，上限 600）" },
                    "run_in_background": { "type": "boolean", "description": "后台执行（默认 false）" },
                    "cwd": { "type": "string", "description": "执行目录（相对会话 cwd；省略沿用会话 cwd）" }
                },
                "required": ["command"]
            }),
            is_read_only: false,
        }
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> Result<ToolResult> {
        let command = input["command"].as_str().unwrap_or_default().to_string();
        if command.trim().is_empty() {
            return Ok(ToolResult::err("bash: command 不能为空"));
        }
        let background = input["run_in_background"].as_bool().unwrap_or(false);
        let timeout_secs = input["timeout"]
            .as_u64()
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .clamp(1, MAX_TIMEOUT_SECS);

        // 会话 cwd（持久化）
        let (run_dir, task_id) = {
            let mut shared = ctx.shared.lock().expect("工具状态锁");
            let dir = shared.bash.effective_cwd(&ctx.cwd, input["cwd"].as_str())?;
            let id = if background {
                Some(shared.bash.spawn_task(&command)?.id)
            } else {
                None
            };
            (dir, id)
        };

        if background {
            let task = {
                let shared = ctx.shared.lock().expect("工具状态锁");
                shared.bash.task(task_id.as_deref().unwrap_or("")).cloned()
            };
            let Some(task) = task else {
                return Ok(ToolResult::err("bash: 后台任务创建失败"));
            };
            // 后台线程跑命令，完成后写回任务状态
            let log_path = task.output_path.clone();
            let log_path_for_msg = log_path.clone();
            let cancel = ctx.cancel.clone();
            let task_id_clone = task.id.clone();
            let shared_ref = ctx.shared.clone();
            let run_dir_clone = run_dir.clone();
            let ctx_cwd = ctx.cwd.clone();
            std::thread::spawn(move || {
                let (stdout, _stderr, code, _killed) = run_sync(
                    &command,
                    &run_dir_clone,
                    std::time::Duration::from_secs(MAX_TIMEOUT_SECS * 10),
                    &cancel,
                );
                let _ = std::fs::write(&log_path, &stdout);
                let status = match code {
                    Some(0) => TaskStatus::Completed { code: 0 },
                    Some(c) => TaskStatus::Completed { code: c },
                    None => TaskStatus::Failed {
                        reason: "进程被信号终止".into(),
                    },
                };
                let ok = matches!(status, TaskStatus::Completed { code: 0 });
                let mut shared = shared_ref.lock().expect("工具状态锁");
                shared.bash.finish_task(&task_id_clone, status.clone());
                if ok {
                    shared
                        .bash
                        .sync_cwd_after_command(&command, &run_dir_clone, &ctx_cwd, true);
                }
            });
            return Ok(ToolResult::ok(format!(
                "后台任务已启动: {}（输出: {}）\n用 bash_output 收割结果",
                task.id,
                log_path_for_msg.display()
            )));
        }

        // 前台：阻塞线程跑，tokio 不卡
        let cancel = ctx.cancel.clone();
        let dir = run_dir.clone();
        let cmd = command.clone();
        let (stdout, stderr, code, killed) = tokio::task::spawn_blocking(move || {
            run_sync(
                &cmd,
                &dir,
                std::time::Duration::from_secs(timeout_secs),
                &cancel,
            )
        })
        .await
        .map_err(|e| anyhow::anyhow!("bash 任务调度失败: {e}"))?;

        let success = code == Some(0) && !killed;
        {
            let mut shared = ctx.shared.lock().expect("工具状态锁");
            shared
                .bash
                .sync_cwd_after_command(&command, &run_dir, &ctx.cwd, success);
        }
        let mut out = String::new();
        if !stdout.trim().is_empty() {
            out.push_str(&stdout);
            if !stdout.ends_with('\n') {
                out.push('\n');
            }
        }
        if !stderr.trim().is_empty() {
            out.push_str("[stderr]\n");
            out.push_str(&stderr);
            if !stderr.ends_with('\n') {
                out.push('\n');
            }
        }
        if out.is_empty() {
            out.push_str("（无输出）\n");
        }
        match (code, killed) {
            (_, true) => out.push_str(&format!(
                "\n[命令被终止：超时 {timeout_secs}s 或用户取消] cwd 保持: {}",
                run_dir.display()
            )),
            (Some(c), false) if c != 0 => {
                out.push_str(&format!("\n[退出码 {c}] cwd: {}", run_dir.display()))
            }
            _ => {}
        }
        Ok(ToolResult {
            success,
            output: super::clamp_output(&ctx.shared.lock().expect("工具状态锁"), out),
            display: Some(DisplayBlock::Output {
                stdout,
                stderr,
                code: code.unwrap_or(-1),
            }),
            images: vec![],
        })
    }
}

/// bash_output：读取/收割后台任务输出（独立工具，CC 的 BashOutput 对齐）。
pub struct BashOutputTool;

#[async_trait::async_trait]
impl Tool for BashOutputTool {
    fn def(&self) -> ToolDefinition {
        ToolDefinition {
            name: "bash_output".into(),
            description: "读取后台 bash 任务的输出。任务已结束时返回完整输出与退出码并释放任务槽;\
                仍在运行时返回当前增量输出。"
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "task_id": { "type": "string", "description": "bash 返回的后台任务 id（如 bg_1）" }
                },
                "required": ["task_id"]
            }),
            is_read_only: true,
        }
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> Result<ToolResult> {
        let id = input["task_id"].as_str().unwrap_or_default().to_string();
        if id.is_empty() {
            return Ok(ToolResult::err("bash_output: task_id 不能为空"));
        }
        let (task, status) = {
            let shared = ctx.shared.lock().expect("工具状态锁");
            match shared.bash.task(&id) {
                Some(t) => (t.clone(), t.status.clone()),
                None => {
                    return Ok(ToolResult::err(format!(
                        "bash_output: 任务 {id} 不存在（可能已被收割）。运行中任务: {:?}",
                        shared.bash.running_ids()
                    )))
                }
            }
        };
        let output = std::fs::read_to_string(&task.output_path).unwrap_or_default();
        match status {
            TaskStatus::Running => {
                let tail: String = output
                    .chars()
                    .rev()
                    .take(8000)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
                Ok(ToolResult::ok(format!(
                    "[任务 {id} 运行中] 最近输出:\n{tail}"
                )))
            }
            TaskStatus::Completed { code } => {
                // 收割：删除任务槽
                let reaped = {
                    let mut shared = ctx.shared.lock().expect("工具状态锁");
                    shared.bash.reap_task(&id)
                };
                if let Some(t) = reaped {
                    let _ = std::fs::remove_file(&t.output_path);
                }
                Ok(ToolResult::ok(format!(
                    "[任务 {id} 已结束，退出码 {code}]\n{}",
                    super::clamp_output(&ctx.shared.lock().expect("工具状态锁"), output)
                )))
            }
            TaskStatus::Failed { reason } => Ok(ToolResult::ok(format!(
                "[任务 {id} 失败: {reason}]\n{output}"
            ))),
            TaskStatus::Killed => {
                let _ = {
                    let mut shared = ctx.shared.lock().expect("工具状态锁");
                    shared.bash.reap_task(&id)
                };
                Ok(ToolResult::ok(format!("[任务 {id} 已被终止]\n{output}")))
            }
        }
    }
}

/// kill 后台任务。
pub struct KillShellTool;

#[async_trait::async_trait]
impl Tool for KillShellTool {
    fn def(&self) -> ToolDefinition {
        ToolDefinition {
            name: "kill_shell".into(),
            description: "终止一个后台 bash 任务。".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "task_id": { "type": "string" }
                },
                "required": ["task_id"]
            }),
            is_read_only: false,
        }
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> Result<ToolResult> {
        let id = input["task_id"].as_str().unwrap_or_default();
        let mut shared = ctx.shared.lock().expect("工具状态锁");
        if shared.bash.task(id).is_none() {
            return Ok(ToolResult::err(format!("kill_shell: 任务 {id} 不存在")));
        }
        shared.bash.finish_task(id, TaskStatus::Killed);
        if let Some(t) = shared.bash.reap_task(id) {
            let _ = std::fs::remove_file(&t.output_path);
        }
        Ok(ToolResult::ok(format!("已终止 {id}")))
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
    async fn runs_command_and_returns_output() {
        let ctx = ctx().await;
        let out = BashTool
            .execute(json!({"command": "echo hello-mcode"}), &ctx)
            .await
            .unwrap();
        assert!(out.success, "{}", out.output);
        assert!(out.output.contains("hello-mcode"));
    }

    #[tokio::test]
    async fn reports_nonzero_exit_code() {
        let ctx = ctx().await;
        let out = BashTool
            .execute(json!({"command": "sh -c 'echo oops >&2; exit 3'"}), &ctx)
            .await
            .unwrap();
        assert!(!out.success);
        assert!(out.output.contains("退出码 3"), "{}", out.output);
        assert!(out.output.contains("oops"));
    }

    #[tokio::test]
    async fn timeout_kills_long_command() {
        let ctx = ctx().await;
        let out = BashTool
            .execute(json!({"command": "sleep 30", "timeout": 1}), &ctx)
            .await
            .unwrap();
        assert!(!out.success);
        assert!(out.output.contains("被终止"), "{}", out.output);
    }

    #[tokio::test]
    async fn cd_persists_across_calls() {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("mcode-bashtool-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub/deep")).unwrap();
        let (ctx, _tx) = ToolContext::for_tests(dir.clone());
        let out = BashTool
            .execute(json!({"command": "cd sub && pwd"}), &ctx)
            .await
            .unwrap();
        assert!(out.success);
        // 下一条命令的 pwd 应在 sub 下
        let out = BashTool
            .execute(json!({"command": "pwd"}), &ctx)
            .await
            .unwrap();
        assert!(
            out.output.contains("sub"),
            "cd 必须持久化，实际: {}",
            out.output
        );
        // 显式相对 cwd 基于 persisted cwd
        let out = BashTool
            .execute(json!({"command": "pwd", "cwd": "deep"}), &ctx)
            .await
            .unwrap();
        assert!(out.output.contains("deep"), "{}", out.output);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn background_task_lifecycle_via_tools() {
        let ctx = ctx().await;
        let out = BashTool
            .execute(
                json!({"command": "echo bg-done", "run_in_background": true}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.success, "{}", out.output);
        assert!(out.output.contains("bg_1"));
        // 轮询收割（任务线程异步完成）
        let mut result = None;
        for _ in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            let r = BashOutputTool
                .execute(json!({"task_id": "bg_1"}), &ctx)
                .await
                .unwrap();
            if r.output.contains("已结束") {
                result = Some(r);
                break;
            }
        }
        let r = result.expect("后台任务未在 5s 内完成");
        assert!(r.output.contains("退出码 0"), "{}", r.output);
        assert!(r.output.contains("bg-done"), "{}", r.output);
        // 收割后再查 → 不存在
        let r = BashOutputTool
            .execute(json!({"task_id": "bg_1"}), &ctx)
            .await
            .unwrap();
        assert!(!r.success);
    }

    #[tokio::test]
    async fn rejects_empty_command() {
        let ctx = ctx().await;
        let out = BashTool
            .execute(json!({"command": "  "}), &ctx)
            .await
            .unwrap();
        assert!(!out.success);
    }
}
