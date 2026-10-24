//! hooks —— 8 事件生命周期钩子，双协议输出解析。
//!
//! 每个事件可挂多条命令，命中匹配器的命令以子进程
//! 方式执行，事件载荷（JSON）写 stdin，**输出双协议**：
//!
//! - **exit-code 文本协议**：exit 0 = 放行（stdout 仅进 transcript 详细视图）；
//!   exit 2 = **阻断**，stderr 作为 reason 回喂模型（PreToolUse 拒绝工具、
//!   Stop 阻止停机强迫继续、UserPromptSubmit 拦下 prompt）；其余非零 =
//!   非阻断错误，stderr 只给用户看，流程继续。
//! - **JSON 结构化协议**：stdout 为 JSON 时按字段裁决——`decision`
//!   （"block"/"approve"）、`continue:false`、PreToolUse 专用
//!   `hookSpecificOutput.permissionDecision`（"allow"/"deny"/"ask"）与
//!   `updatedInput`（改写工具参数）。JSON 字段优先级高于 exit code
//!   （exit 2 + JSON reason → 用 JSON reason）。
//!
//! runner 集成三个 continue 点：PreToolUse（权限判定**之前**，hook deny
//! 直接拒绝 / allow 跳过权限询问）、PostToolUse（block 把工具结果改写为
//! 错误回喂）、Stop（block 把 reason 作为新 user 消息注入，强迫继续）。
//! 其余事件（UserPromptSubmit/Notification/SessionStart/…）由交互层调用
//! [`HookRunner::run_event`]。

use crate::config::{HookConfig, HookEvent};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::AsyncWriteExt;

/// 单条 hook 命令的默认超时。
pub const DEFAULT_HOOK_TIMEOUT_SECS: u64 = 60;

// ---------------------------------------------------------------------------
// 裁决结果
// ---------------------------------------------------------------------------

/// 一轮事件（可能多条 hook 命令）聚合后的裁决。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HookOutcome {
    /// 阻断：`feedback` 作为模型可见 reason（拒绝工具/强迫继续/拦 prompt）。
    pub blocked: bool,
    /// 阻断原因（exit 2 的 stderr 或 JSON reason）；blocked 时非空。
    pub feedback: Option<String>,
    /// PreToolUse：hook 要求强制走权限询问（permissionDecision=ask）。
    pub force_ask: bool,
    /// PreToolUse：hook 已批准（permissionDecision=allow / decision=approve），
    /// 跳过权限询问。
    pub allow_skip_permission: bool,
    /// PreToolUse：hook 改写后的工具参数（updatedInput）。
    pub updated_input: Option<Value>,
    /// 非阻断 stderr（exit 非 0/2 等）——只给用户看，不进模型上下文。
    pub warnings: Vec<String>,
    /// exit 0 且未被 suppressOutput 的 stdout（transcript 详细视图展示）。
    pub stdout: Option<String>,
}

impl HookOutcome {
    fn proceed() -> Self {
        Self::default()
    }

    fn blocked(reason: String) -> Self {
        Self {
            blocked: true,
            feedback: Some(reason),
            ..Self::default()
        }
    }

    /// 合并另一条 hook 的结果到聚合态：阻断优先（首个阻断即短路）、
    /// ask 压过 allow、updatedInput 取首个。
    fn merge(&mut self, other: HookOutcome) {
        if other.blocked {
            // 阻断覆盖聚合态，但保留此前已收集的非阻断警告（不吞信号）
            let warnings = std::mem::take(&mut self.warnings);
            *self = other;
            self.warnings = warnings;
            return;
        }
        self.warnings.extend(other.warnings);
        if other.force_ask {
            self.force_ask = true;
            self.allow_skip_permission = false;
        } else if other.allow_skip_permission && !self.force_ask {
            self.allow_skip_permission = true;
        }
        if self.updated_input.is_none() {
            self.updated_input = other.updated_input;
        }
        if self.stdout.is_none() {
            self.stdout = other.stdout;
        }
    }
}

// ---------------------------------------------------------------------------
// HookRunner
// ---------------------------------------------------------------------------

/// hooks 运行时：按事件过滤配置并执行子进程。`Clone`（Arc 共享进 AgentDeps）。
#[derive(Clone)]
pub struct HookRunner {
    hooks: Vec<HookConfig>,
    session_id: String,
    /// transcript JSONL 路径（sessions 里程碑接入后非 None；载荷字段）。
    transcript_path: Option<PathBuf>,
}

impl HookRunner {
    pub fn new(hooks: Vec<HookConfig>, session_id: impl Into<String>) -> Self {
        Self {
            hooks,
            session_id: session_id.into(),
            transcript_path: None,
        }
    }

    pub fn set_transcript_path(&mut self, p: PathBuf) {
        self.transcript_path = Some(p);
    }

    /// 已配置的 hook 清单（/hooks 展示用）。
    pub fn configs(&self) -> &[HookConfig] {
        &self.hooks
    }

    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty()
    }

    /// 该事件是否有已启用的 hook（调用方据此跳过 await）。
    pub fn has_hooks_for(&self, event: HookEvent) -> bool {
        self.hooks.iter().any(|h| h.event == event)
    }

    /// PreToolUse：权限判定之前调用。工具名过匹配器。
    pub async fn run_pre_tool_use(&self, cwd: &Path, tool: &str, input: &Value) -> HookOutcome {
        self.dispatch(
            HookEvent::PreToolUse,
            cwd,
            json!({ "tool_name": tool, "tool_input": input }),
            Some(tool),
        )
        .await
    }

    /// PostToolUse：工具执行完调用；block 时调用方把工具结果改写为错误。
    pub async fn run_post_tool_use(
        &self,
        cwd: &Path,
        tool: &str,
        input: &Value,
        response: &str,
    ) -> HookOutcome {
        self.dispatch(
            HookEvent::PostToolUse,
            cwd,
            json!({ "tool_name": tool, "tool_input": input, "tool_response": response }),
            Some(tool),
        )
        .await
    }

    /// Stop：回合自然收尾前调用；block 时调用方注入 reason 强迫继续。
    pub async fn run_stop(&self, cwd: &Path, stop_hook_active: bool) -> HookOutcome {
        self.dispatch(
            HookEvent::Stop,
            cwd,
            json!({ "stop_hook_active": stop_hook_active }),
            None,
        )
        .await
    }

    /// 通用事件发射（UserPromptSubmit / Notification / SessionStart /
    /// SessionEnd / SubagentStop 由交互层与 subagent 运行时调用）。
    /// `extra` 的事件专属字段原样并入载荷。
    pub async fn run_event(&self, event: HookEvent, cwd: &Path, extra: Value) -> HookOutcome {
        self.dispatch(event, cwd, extra, None).await
    }

    /// 执行一条事件的所有命中 hook。`tool` 非 None 时匹配器按工具名过滤。
    async fn dispatch(
        &self,
        event: HookEvent,
        cwd: &Path,
        mut payload: Value,
        tool: Option<&str>,
    ) -> HookOutcome {
        let configs: Vec<HookConfig> = self
            .hooks
            .iter()
            .filter(|h| h.event == event)
            .filter(|h| match tool {
                Some(t) => matcher_matches(&h.matcher, t),
                None => true, // 非工具事件：匹配器不参与
            })
            .cloned()
            .collect();
        let mut out = HookOutcome::proceed();
        for cfg in configs {
            inject_envelope(
                &mut payload,
                event,
                &self.session_id,
                &self.transcript_path,
                cwd,
            );
            let parsed = parse_hook_result(&cfg, run_hook_command(&cfg, &payload).await);
            let blocked = parsed.blocked;
            out.merge(parsed);
            if blocked {
                break; // 首个阻断即短路（后续 hook 不再执行）
            }
        }
        out
    }
}

/// 匹配器：`*` / 空 = 全部；否则精确或子串匹配工具名（对齐 config 注释）。
pub fn matcher_matches(matcher: &str, tool: &str) -> bool {
    let m = matcher.trim();
    m.is_empty() || m == "*" || tool.contains(m)
}

fn inject_envelope(
    payload: &mut Value,
    event: HookEvent,
    session_id: &str,
    transcript_path: &Option<PathBuf>,
    cwd: &Path,
) {
    let obj = payload.as_object_mut().expect("hook payload 必须是对象");
    obj.insert(
        "hook_event_name".into(),
        serde_json::to_value(event).expect("HookEvent 序列化不会失败"),
    );
    obj.insert("session_id".into(), Value::String(session_id.into()));
    obj.insert("cwd".into(), Value::String(cwd.display().to_string()));
    if let Some(p) = transcript_path {
        obj.insert(
            "transcript_path".into(),
            Value::String(p.display().to_string()),
        );
    }
}

// ---------------------------------------------------------------------------
// 子进程执行
// ---------------------------------------------------------------------------

/// 一条 hook 命令的原始执行产物。
#[derive(Debug)]
struct HookExec {
    code: Option<i32>,
    stdout: String,
    stderr: String,
    timed_out: bool,
    spawn_error: Option<String>,
}

/// 跨平台 shell 包装：Unix `sh -c`；Windows `cmd /S /C`（并隐藏控制台窗口）。
/// Windows 必须 `raw_arg` 原样传命令：`arg()` 按 MSVCRT 规则把 `"` 转义成
/// `\"`，再经 cmd /S 剥掉首尾引号后，命令文本里的引号全成字面 `\"`——
/// echo 出的 JSON 解析必败、重定向目标非法（曾致 CI windows 全红 + 死循环）。
#[cfg(windows)]
fn shell_command(command: &str) -> tokio::process::Command {
    let mut c = tokio::process::Command::new("cmd");
    c.arg("/S").arg("/C").raw_arg(command);
    crate::util::hide_console_window(&mut c);
    c
}

#[cfg(not(windows))]
fn shell_command(command: &str) -> tokio::process::Command {
    let mut c = tokio::process::Command::new("sh");
    c.arg("-c").arg(command);
    c
}

async fn run_hook_command(cfg: &HookConfig, payload: &Value) -> HookExec {
    let body = serde_json::to_vec(payload).expect("序列化 hook 载荷不会失败");
    let mut cmd = shell_command(&cfg.command);
    cmd.current_dir(cwd_of(payload))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return HookExec {
                code: None,
                stdout: String::new(),
                stderr: String::new(),
                timed_out: false,
                spawn_error: Some(e.to_string()),
            }
        }
    };

    // 载荷先写完再等结果（载荷为 KB 级，不会撑爆管道缓冲）。
    // stdin 在 spawn 后必然存在（Stdio::piped）。
    if let Some(mut si) = child.stdin.take() {
        if si.write_all(&body).await.is_ok() {
            let _ = si.shutdown().await;
        }
    }

    let timeout = Duration::from_secs(cfg.timeout_secs.unwrap_or(DEFAULT_HOOK_TIMEOUT_SECS));
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(out)) => HookExec {
            code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            timed_out: false,
            spawn_error: None,
        },
        Ok(Err(e)) => HookExec {
            code: None,
            stdout: String::new(),
            stderr: e.to_string(),
            timed_out: false,
            spawn_error: Some(e.to_string()),
        },
        // 超时：future 被 drop → child 被 drop → kill_on_drop 兜底杀进程
        Err(_) => HookExec {
            code: None,
            stdout: String::new(),
            stderr: format!("hook 超时（{}s）", timeout.as_secs()),
            timed_out: true,
            spawn_error: None,
        },
    }
}

fn cwd_of(payload: &Value) -> &Path {
    payload
        .get("cwd")
        .and_then(Value::as_str)
        .map(Path::new)
        .unwrap_or_else(|| Path::new("."))
}

// ---------------------------------------------------------------------------
// 双协议解析
// ---------------------------------------------------------------------------

/// stdout JSON 结构化输出。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HookJsonOutput {
    #[serde(rename = "continue")]
    is_continue: Option<bool>,
    stop_reason: Option<String>,
    decision: Option<String>,
    reason: Option<String>,
    suppress_output: Option<bool>,
    hook_specific_output: Option<HookSpecificOutput>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HookSpecificOutput {
    permission_decision: Option<String>,
    updated_input: Option<Value>,
}

/// 单条 hook 执行结果 → 裁决（双协议解析）。
fn parse_hook_result(cfg: &HookConfig, exec: HookExec) -> HookOutcome {
    if let Some(err) = exec.spawn_error {
        return HookOutcome {
            warnings: vec![format!("hook 启动失败（{}）: {err}", cfg.command)],
            ..HookOutcome::proceed()
        };
    }
    if exec.timed_out {
        return HookOutcome {
            warnings: vec![format!("hook 超时被终止（{}）", cfg.command)],
            ..HookOutcome::proceed()
        };
    }

    // stdout JSON 结构化协议（整段解析失败按文本协议处理）
    let json_out: Option<HookJsonOutput> = serde_json::from_str(exec.stdout.trim()).ok();
    let json = json_out.as_ref();

    let mut out = HookOutcome::proceed();

    // 1) 阻断判定：exit 2（stderr 为 reason）或 JSON block 语义
    if exec.code == Some(2) {
        let reason = json
            .and_then(|j| j.reason.clone())
            .unwrap_or_else(|| stderr_or_default(&exec.stderr));
        return HookOutcome::blocked(reason);
    }
    if json.and_then(|j| j.is_continue) == Some(false) {
        let reason = json
            .and_then(|j| j.reason.clone())
            .or(json.and_then(|j| j.stop_reason.clone()))
            .unwrap_or_else(|| "hook 请求停止".into());
        return HookOutcome::blocked(reason);
    }
    match json.and_then(|j| j.decision.as_deref()) {
        Some("block") => {
            return HookOutcome::blocked(
                json.and_then(|j| j.reason.clone())
                    .unwrap_or_else(|| "hook 阻断".into()),
            )
        }
        Some("approve") => out.allow_skip_permission = true,
        _ => {}
    }
    if let Some(hs) = json.and_then(|j| j.hook_specific_output.as_ref()) {
        match hs.permission_decision.as_deref() {
            Some("deny") => {
                return HookOutcome::blocked(
                    json.and_then(|j| j.reason.clone())
                        .unwrap_or_else(|| "hook 拒绝执行".into()),
                )
            }
            Some("ask") => out.force_ask = true,
            Some("allow") => out.allow_skip_permission = true,
            _ => {}
        }
        if let Some(updated) = &hs.updated_input {
            out.updated_input = Some(updated.clone());
        }
    }

    // 2) 其余非零退出码 = 非阻断错误（stderr 只给用户）
    if let Some(code) = exec.code {
        if code != 0 && !exec.stderr.trim().is_empty() {
            out.warnings.push(format!(
                "hook（{}）退出码 {code}: {}",
                cfg.command,
                exec.stderr.trim()
            ));
        }
    }

    // 3) exit 0 的 stdout 进 transcript（suppressOutput 可隐藏）
    if exec.code == Some(0) && json.and_then(|j| j.suppress_output) != Some(true) {
        let text = exec.stdout.trim();
        if !text.is_empty() {
            out.stdout = Some(text.to_string());
        }
    }
    out
}

fn stderr_or_default(stderr: &str) -> String {
    let t = stderr.trim();
    if t.is_empty() {
        "hook 阻断执行".into()
    } else {
        t.to_string()
    }
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn hook(event: HookEvent, matcher: &str, command: &str) -> HookConfig {
        HookConfig {
            event,
            matcher: matcher.into(),
            command: command.into(),
            timeout_secs: None,
        }
    }

    // 各平台的阻断命令：stderr 输出 "blocked" 并以 exit 2 退出
    fn blocked_cmd() -> &'static str {
        if cfg!(windows) {
            "echo blocked 1>&2 & exit /b 2"
        } else {
            "echo blocked >&2; exit 2"
        }
    }

    fn deny_json_cmd() -> String {
        if cfg!(windows) {
            "echo {\"decision\":\"block\",\"reason\":\"no-rm\"}".into()
        } else {
            r#"printf '%s' '{"decision":"block","reason":"no-rm"}'"#.into()
        }
    }

    fn ask_json_cmd() -> String {
        if cfg!(windows) {
            "echo {\"hookSpecificOutput\":{\"permissionDecision\":\"ask\",\"updatedInput\":{\"command\":\"ls\"}}}".into()
        } else {
            r#"printf '%s' '{"hookSpecificOutput":{"permissionDecision":"ask","updatedInput":{"command":"ls"}}}'"#.into()
        }
    }

    fn allow_json_cmd() -> String {
        if cfg!(windows) {
            "echo {\"hookSpecificOutput\":{\"permissionDecision\":\"allow\"}}".into()
        } else {
            r#"printf '%s' '{"hookSpecificOutput":{"permissionDecision":"allow"}}'"#.into()
        }
    }

    fn suppress_json_cmd() -> &'static str {
        if cfg!(windows) {
            "echo {\"suppressOutput\":true}"
        } else {
            r#"printf '%s' '{"suppressOutput":true}'"#
        }
    }

    fn stdin_echo_cmd() -> &'static str {
        // 回显 stdin（Unix cat / Windows more），用于验证载荷字段
        if cfg!(windows) {
            "more"
        } else {
            "cat"
        }
    }

    #[tokio::test]
    async fn exit_zero_proceeds_and_captures_stdout() {
        let r = HookRunner::new(vec![hook(HookEvent::Stop, "*", "echo ok")], "s")
            .run_stop(Path::new("/"), false)
            .await;
        assert!(!r.blocked);
        assert_eq!(r.stdout.as_deref(), Some("ok"));
        assert!(r.warnings.is_empty());
    }

    #[tokio::test]
    async fn exit_two_blocks_with_stderr_reason() {
        let r = HookRunner::new(vec![hook(HookEvent::PreToolUse, "*", blocked_cmd())], "s")
            .run_pre_tool_use(Path::new("/"), "bash", &json!({"command": "rm -rf /"}))
            .await;
        assert!(r.blocked);
        assert_eq!(r.feedback.as_deref(), Some("blocked"));
    }

    #[tokio::test]
    async fn json_block_decision_uses_json_reason() {
        let r = HookRunner::new(
            vec![hook(HookEvent::PreToolUse, "bash", &deny_json_cmd())],
            "s",
        )
        .run_pre_tool_use(Path::new("/"), "bash", &json!({"command": "x"}))
        .await;
        assert!(r.blocked);
        assert_eq!(r.feedback.as_deref(), Some("no-rm"));
    }

    #[tokio::test]
    async fn permission_decision_ask_and_updated_input() {
        let r = HookRunner::new(vec![hook(HookEvent::PreToolUse, "*", &ask_json_cmd())], "s")
            .run_pre_tool_use(Path::new("/"), "bash", &json!({"command": "x"}))
            .await;
        assert!(!r.blocked);
        assert!(r.force_ask);
        assert!(!r.allow_skip_permission);
        assert_eq!(r.updated_input.unwrap()["command"], "ls");
    }

    #[tokio::test]
    async fn permission_decision_allow_skips_permission() {
        let r = HookRunner::new(
            vec![hook(HookEvent::PreToolUse, "*", &allow_json_cmd())],
            "s",
        )
        .run_pre_tool_use(Path::new("/"), "bash", &json!({}))
        .await;
        assert!(r.allow_skip_permission);
        assert!(!r.force_ask);
        assert!(!r.blocked);
    }

    #[tokio::test]
    async fn other_nonzero_exit_is_warning_not_block() {
        let cmd = if cfg!(windows) {
            "echo warn 1>&2 & exit /b 3"
        } else {
            "echo warn >&2; exit 3"
        };
        let r = HookRunner::new(vec![hook(HookEvent::PostToolUse, "*", cmd)], "s")
            .run_post_tool_use(Path::new("/"), "bash", &json!({}), "out")
            .await;
        assert!(!r.blocked);
        assert_eq!(r.warnings.len(), 1);
        assert!(r.warnings[0].contains("warn"));
    }

    #[tokio::test]
    async fn matcher_filters_by_tool_name() {
        let runner = HookRunner::new(
            vec![
                hook(HookEvent::PreToolUse, "grep", blocked_cmd()),
                hook(HookEvent::PreToolUse, "bash", &allow_json_cmd()),
            ],
            "s",
        );
        // grep 命中第一条（阻断），第二条不执行
        let r = runner
            .run_pre_tool_use(Path::new("/"), "grep", &json!({}))
            .await;
        assert!(r.blocked);
        // bash 只命中第二条（allow）
        let r = runner
            .run_pre_tool_use(Path::new("/"), "bash", &json!({}))
            .await;
        assert!(r.allow_skip_permission);
        assert!(!r.blocked);
    }

    /// 真实存在的临时目录（hook 子进程以它为 cwd，不存在的目录会 spawn 失败）。
    fn tmp_cwd(tag: &str) -> PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("mcode-hooks-{tag}-{n}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn stdin_payload_carries_event_fields() {
        let cwd = tmp_cwd("stdin");
        let runner = HookRunner::new(
            vec![hook(HookEvent::PreToolUse, "*", stdin_echo_cmd())],
            "sess-42",
        );
        let r = runner
            .run_pre_tool_use(&cwd, "bash", &json!({"command": "ls"}))
            .await;
        assert!(
            r.warnings.is_empty(),
            "意外警告（cwd 不存在会 spawn 失败）: {:?}",
            r.warnings
        );
        let echoed: Value =
            serde_json::from_str(r.stdout.as_deref().expect("cat/more 必须回显载荷")).unwrap();
        assert_eq!(echoed["hook_event_name"], "pre_tool_use");
        assert_eq!(echoed["session_id"], "sess-42");
        assert_eq!(echoed["cwd"], cwd.display().to_string());
        assert_eq!(echoed["tool_name"], "bash");
        assert_eq!(echoed["tool_input"]["command"], "ls");
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[tokio::test]
    async fn timeout_kills_and_warns() {
        let cmd = if cfg!(windows) {
            "ping -n 6 127.0.0.1 >nul"
        } else {
            "sleep 5"
        };
        let cfg = HookConfig {
            event: HookEvent::Stop,
            matcher: "*".into(),
            command: cmd.into(),
            timeout_secs: Some(1),
        };
        let started = std::time::Instant::now();
        let r = HookRunner::new(vec![cfg], "s")
            .run_stop(Path::new("/"), false)
            .await;
        assert!(!r.blocked);
        assert!(r.warnings.iter().any(|w| w.contains("超时")));
        assert!(started.elapsed() < std::time::Duration::from_secs(4));
    }

    #[tokio::test]
    async fn first_block_wins_among_multiple_hooks() {
        let runner = HookRunner::new(
            vec![
                hook(HookEvent::PreToolUse, "*", blocked_cmd()),
                hook(HookEvent::PreToolUse, "*", &deny_json_cmd()),
            ],
            "s",
        );
        let r = runner
            .run_pre_tool_use(Path::new("/"), "bash", &json!({}))
            .await;
        assert!(r.blocked);
        assert_eq!(
            r.feedback.as_deref(),
            Some("blocked"),
            "首个阻断的 reason 胜出"
        );
    }

    #[tokio::test]
    async fn spawn_failure_is_warning() {
        let cfg = hook(HookEvent::Stop, "*", "mcode-definitely-not-a-command-xyz");
        let r = HookRunner::new(vec![cfg], "s")
            .run_stop(Path::new("/"), false)
            .await;
        assert!(!r.blocked);
        assert_eq!(
            r.warnings.len(),
            1,
            "启动失败只警告不阻断: {:?}",
            r.warnings
        );
    }

    #[tokio::test]
    async fn generic_event_payload_includes_envelope() {
        let cwd = tmp_cwd("generic");
        let runner = HookRunner::new(
            vec![hook(HookEvent::UserPromptSubmit, "*", stdin_echo_cmd())],
            "sess-7",
        );
        let r = runner
            .run_event(
                HookEvent::UserPromptSubmit,
                &cwd,
                json!({ "prompt": "你好" }),
            )
            .await;
        let echoed: Value = serde_json::from_str(r.stdout.as_deref().unwrap()).unwrap();
        assert_eq!(echoed["prompt"], "你好");
        assert_eq!(echoed["hook_event_name"], "user_prompt_submit");
        assert_eq!(echoed["session_id"], "sess-7");
        assert_eq!(echoed["cwd"], cwd.display().to_string());
        assert!(echoed.get("tool_name").is_none());
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[tokio::test]
    async fn empty_runner_is_noop() {
        let runner = HookRunner::new(vec![], "s");
        assert!(runner.is_empty());
        assert!(!runner.has_hooks_for(HookEvent::Stop));
        let r = runner
            .run_pre_tool_use(Path::new("/"), "bash", &json!({}))
            .await;
        assert_eq!(r, HookOutcome::proceed());
    }

    #[tokio::test]
    async fn suppress_output_hides_stdout() {
        let r = HookRunner::new(vec![hook(HookEvent::Stop, "*", suppress_json_cmd())], "s")
            .run_stop(Path::new("/"), false)
            .await;
        assert!(
            r.stdout.is_none(),
            "suppressOutput 时 stdout 不进 transcript: {:?}",
            r.stdout
        );
    }

    #[test]
    fn matcher_semantics() {
        assert!(matcher_matches("*", "bash"));
        assert!(matcher_matches("", "bash"));
        assert!(matcher_matches("ba", "bash"), "子串匹配");
        assert!(matcher_matches("bash", "bash"));
        assert!(!matcher_matches("grep", "bash"));
    }

    #[test]
    fn outcome_merge_semantics() {
        // ask 压过 allow
        let mut agg = HookOutcome {
            allow_skip_permission: true,
            ..Default::default()
        };
        agg.merge(HookOutcome {
            force_ask: true,
            ..Default::default()
        });
        assert!(agg.force_ask && !agg.allow_skip_permission);
        // 阻断覆盖聚合但保留已收集 warnings（不吞信号）
        let mut agg = HookOutcome {
            warnings: vec!["w".into()],
            ..Default::default()
        };
        agg.merge(HookOutcome::blocked("no".into()));
        assert!(agg.blocked);
        assert_eq!(agg.warnings, vec!["w"]);
    }

    #[test]
    fn json_continue_false_blocks_with_stop_reason() {
        let exec = HookExec {
            code: Some(0),
            stdout: r#"{"continue":false,"stopReason":"budget exhausted"}"#.into(),
            stderr: String::new(),
            timed_out: false,
            spawn_error: None,
        };
        let out = parse_hook_result(&hook(HookEvent::Stop, "*", "x"), exec);
        assert!(out.blocked);
        assert_eq!(out.feedback.as_deref(), Some("budget exhausted"));
    }
}
