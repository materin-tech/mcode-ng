// MCode_Desktop — AI coding agent desktop app
// Copyright (c) 2026 Materin-tech. All rights reserved.
// SPDX-License-Identifier: MIT

//! 交互场景测试矩阵（M2 全接线回归）— gpui TestAppContext（#[gpui::test]）无头驱动 App 状态机：
//! 直接调 send()/on_ui_event()/interrupt()（按键级 IME 路径不在本矩阵范围），
//! MockBackend 按调用序脚本化返回流事件，事件经 take_rx 的 channel 由测试泵回 on_ui_event。

use crate::app::App;
use crate::app::View;
use crate::app::msg::ChatMsg;
use crate::app::runtime::{UiEvent, build_harness_with_backend};
use crate::views::file_explorer::scan_file_tree;
use futures::StreamExt;
use futures::stream::BoxStream;
use gpui::{AppContext, Entity, TestAppContext};
use mcode_core::agent::AgentEvent;
use mcode_core::api::{ApiError, ApiErrorKind, ModelBackend, StreamEvent, StreamRequest};
use mcode_core::config::{Config, Protocol, Provider};
use mcode_core::message::{Message, StopReason};
use mcode_core::permission::{Decision, Mode, PermCheck, Verdict};
use mcode_core::sessions::SessionStore;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

// ---------- 脚本化 MockBackend ----------

#[derive(Clone)]
pub(crate) enum Step {
    /// 正常一轮流式结果（reply 会以 TextDelta 发出，EndTurn 收口）
    Turn(String),
    /// 一轮 bash 工具调用（runner 执行 `echo hi` 后再发起下一轮流）
    ToolTurn(String),
    /// 直接报错（Auth 类 = 不可重试，立即失败；文案避免触发重试词表）
    Fail(String),
}

/// 纯文本回合的事件序列
fn text_events(reply: &str, stop: StopReason) -> Vec<Result<StreamEvent, ApiError>> {
    let mut evs: Vec<Result<StreamEvent, ApiError>> = vec![];
    if !reply.is_empty() {
        evs.push(Ok(StreamEvent::TextDelta(reply.to_string())));
    }
    evs.push(Ok(StreamEvent::Done { stop_reason: stop }));
    evs
}

/// 工具回合的事件序列：ToolUseStart → ToolUseDone（runner 见 Done{ToolUse} 后执行工具）
fn tool_events(reply: &str) -> Vec<Result<StreamEvent, ApiError>> {
    let mut evs = vec![Ok(StreamEvent::ToolUseStart {
        id: "c1".into(),
        name: "bash".into(),
    })];
    evs.extend(text_events(reply, StopReason::ToolUse));
    evs.insert(
        1,
        Ok(StreamEvent::ToolUseDone {
            id: "c1".into(),
            input: json!({"command": "echo hi"}),
        }),
    );
    evs
}

struct ScriptMock {
    steps: Vec<Step>,
    calls: AtomicUsize,
}

/// case03 专用：hang 开关由测试翻转（被打断的任务若尚未启动不会消耗脚本步骤，
/// 按调用序的脚本会错位；开关式 mock 与任务启动时序解耦）。
/// 挂起实现 = 永不产出事件的流；runner 的 stream_once select 在 cancel watch 上，
/// 打断即 drop 整个 future。
struct HangMock {
    hang: std::sync::atomic::AtomicBool,
    replies: std::sync::Mutex<Vec<String>>,
}

impl HangMock {
    fn new(replies: Vec<&str>) -> Self {
        Self {
            hang: std::sync::atomic::AtomicBool::new(true),
            replies: std::sync::Mutex::new(replies.into_iter().map(String::from).collect()),
        }
    }

    fn disarm(&self) {
        self.hang.store(false, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl ModelBackend for HangMock {
    fn backend_name(&self) -> &'static str {
        "hang-mock"
    }

    async fn stream(
        &self,
        _req: StreamRequest,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ApiError>>, ApiError> {
        if self.hang.load(Ordering::SeqCst) {
            return Ok(futures::stream::pending().boxed());
        }
        let reply = self.replies.lock().unwrap().pop().unwrap_or_default();
        Ok(futures::stream::iter(text_events(&reply, StopReason::EndTurn)).boxed())
    }
}

#[async_trait::async_trait]
impl ModelBackend for ScriptMock {
    fn backend_name(&self) -> &'static str {
        "script-mock"
    }

    async fn stream(
        &self,
        _req: StreamRequest,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ApiError>>, ApiError> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let step = self
            .steps
            .get(n)
            .cloned()
            .unwrap_or_else(|| Step::Turn(String::new()));
        match step {
            Step::Fail(msg) => Err(ApiError::new(ApiErrorKind::Auth, msg)),
            Step::Turn(reply) => {
                Ok(futures::stream::iter(text_events(&reply, StopReason::EndTurn)).boxed())
            }
            Step::ToolTurn(reply) => Ok(futures::stream::iter(tool_events(&reply)).boxed()),
        }
    }
}

// ---------- 测试环境搭建 ----------

pub(crate) fn test_config() -> Config {
    Config {
        active_provider: Some("test".into()),
        active_model: Some("test-model".into()),
        providers: vec![Provider {
            name: "test".into(),
            base_url: "http://127.0.0.1:9/v1".into(),
            // 非合法变量名 → 按明文 key 兜底，provider_ready 通过
            api_key_env: "sk-literal-test".into(),
            models: vec!["test-model".into()],
            multimodal_models: vec![],
            protocol: Protocol::default(),
        }],
        ..Default::default()
    }
}

pub(crate) static DIR_SEQ: AtomicUsize = AtomicUsize::new(0);

pub(crate) struct Ctx {
    pub app: Entity<App>,
    pub rx: tokio::sync::mpsc::UnboundedReceiver<UiEvent>,
    pub tmp: PathBuf,
}

impl Drop for Ctx {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.tmp);
    }
}

pub(crate) fn turn(reply: &str) -> Step {
    Step::Turn(reply.to_string())
}

pub(crate) fn make_temp_dir() -> PathBuf {
    let n = DIR_SEQ.fetch_add(1, Ordering::SeqCst);
    let tmp = std::env::temp_dir().join(format!("mcode-ui-test-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    tmp
}

fn setup_backend(cx: &mut TestAppContext, backend: Arc<dyn ModelBackend>) -> Ctx {
    let tmp = make_temp_dir();
    let app = cx.new(App::new_for_test);
    app.update(cx, |a, _cx| {
        a.cfg = test_config();
        a.cwd = tmp.clone();
        a.file_tree = scan_file_tree(&a.cwd, a.show_hidden);
        let store = SessionStore::for_project(&a.cwd).unwrap();
        let (h, hist) = build_harness_with_backend(&a.cfg, &a.cwd, &store, &a.rt, backend).unwrap();
        a.hist = hist;
        a.store = Some(store);
        a.harness = Some(Arc::new(h));
        a.refresh_sessions();
    });
    let rx = app.read_with(cx, |a, _| a.harness.as_ref().unwrap().take_rx().unwrap());
    Ctx { app, rx, tmp }
}

pub(crate) fn setup(cx: &mut TestAppContext, steps: Vec<Step>) -> Ctx {
    setup_backend(
        cx,
        Arc::new(ScriptMock {
            steps,
            calls: AtomicUsize::new(0),
        }),
    )
}

/// 泵事件：channel 里所有 UiEvent → on_ui_event，直到条件满足或超时
pub(crate) fn wait(
    cx: &mut TestAppContext,
    ctx: &mut Ctx,
    cond: impl Fn(&App) -> bool,
    ms: u64,
) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(ms);
    loop {
        while let Ok(ev) = ctx.rx.try_recv() {
            ctx.app.update(cx, |a, cx| a.on_ui_event(ev, cx));
        }
        if ctx.app.read_with(cx, |a, _| cond(a)) {
            return true;
        }
        if std::time::Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(15));
    }
}

pub(crate) fn is_notice(m: &ChatMsg, contains: &str) -> bool {
    matches!(m, ChatMsg::Notice { text, .. } if text.contains(contains))
}

pub(crate) fn has_notice(a: &App, contains: &str) -> bool {
    a.messages.iter().any(|m| is_notice(m, contains))
}

// ---------- 用例 1：普通输入 ----------
#[gpui::test]
async fn case01_plain_turn_streams_and_finishes(cx: &mut TestAppContext) {
    let mut c = setup(cx, vec![turn("你好，世界！")]);
    c.app.update(cx, |a, cx| a.send("hello".into(), cx));
    assert!(
        wait(
            cx,
            &mut c,
            |a| !a.running
                && matches!(a.messages.last(), Some(ChatMsg::Assistant { text, .. }) if text.contains("你好，世界")),
            5_000
        ),
        "回合未完成: {:?}",
        c.app.read_with(cx, |a, _| a.messages.clone())
    );
    c.app.read_with(cx, |a, _| {
        assert!(matches!(&a.messages[0], ChatMsg::User { text, .. } if text == "hello"));
        assert!(a.current_session_id().is_some(), "会话未落盘");
        assert!(
            !a.messages.iter().any(|m| matches!(m, ChatMsg::Running)),
            "Running 残留"
        );
    });
}

// ---------- 用例 2：工具回合 ----------
#[gpui::test]
async fn case02_tool_turn_runs_and_feeds_back(cx: &mut TestAppContext) {
    let mut c = setup(cx, vec![Step::ToolTurn(String::new()), turn("done")]);
    c.app.update(cx, |a, cx| a.send("run echo".into(), cx));
    assert!(wait(cx, &mut c, |a| !a.running, 5_000), "工具回合未完成");
    c.app.read_with(cx, |a, _| {
        let card = a.messages.iter().find_map(|m| match m {
            ChatMsg::ToolCard {
                name,
                running,
                success,
                summary,
                ..
            } => Some((name.clone(), *running, *success, summary.clone())),
            _ => None,
        });
        let (name, running, success, summary) = card.expect("无 ToolCard");
        assert_eq!(name, "bash");
        assert!(!running, "工具卡未回填");
        assert!(success, "工具失败");
        assert!(summary.contains("hi"), "输出摘要缺失: {summary}");
        assert!(
            matches!(a.messages.last(), Some(ChatMsg::Assistant { text, .. }) if text == "done")
        );
        // bash 的 display Output → Notice（exit 0）
        assert!(
            has_notice(a, "exit 0"),
            "bash Output display 未渲染为 Notice"
        );
    });
}

// ---------- 用例 3：打断后输入（本批 bug 回归） ----------
#[gpui::test]
async fn case03_interrupt_during_hang_then_recover(cx: &mut TestAppContext) {
    let mock = Arc::new(HangMock::new(vec!["second-ok"]));
    let mut c = setup_backend(cx, mock.clone());
    c.app.update(cx, |a, cx| a.send("first".into(), cx));
    assert!(wait(cx, &mut c, |a| a.running, 5_000), "回合未启动");
    c.app.update(cx, |a, cx| a.interrupt(cx));
    // 预算放宽：全套件并行运行时 tokio 线程与真实 bash 进程竞争调度，
    // 5s 在高负载机器上可能不够（实测 flaky）
    assert!(
        wait(
            cx,
            &mut c,
            |a| !a.running && !a.messages.iter().any(|m| matches!(m, ChatMsg::Running)),
            20_000
        ),
        "ESC 后未收尾"
    );
    // 解除挂起（旧任务可能已在流中挂起、也可能尚未启动即被打断——两种时序都覆盖）
    mock.disarm();
    // 打断后的空回复不落 hist/会话；第二轮正常完成
    c.app.update(cx, |a, cx| a.send("second".into(), cx));
    assert!(
        wait(
            cx,
            &mut c,
            |a| !a.running
                && a.messages
                    .iter()
                    .any(|m| matches!(m, ChatMsg::Assistant { text, .. } if text == "second-ok")),
            5_000
        ),
        "打断后新回合未完成（回归失败）: messages={:?}",
        c.app.read_with(cx, |a, _| a.messages.clone())
    );
}

// ---------- 用例 4：运行中排队 ----------
#[gpui::test]
async fn case04_queue_drains_in_order(cx: &mut TestAppContext) {
    let mut c = setup(cx, vec![turn("r1"), turn("r2"), turn("r3")]);
    c.app.update(cx, |a, cx| a.send("a".into(), cx));
    c.app.update(cx, |a, cx| a.send("b".into(), cx));
    c.app.update(cx, |a, cx| a.send("c".into(), cx));
    c.app.read_with(cx, |a, _| {
        assert_eq!(a.queue.len(), 2, "排队数不对");
        assert!(a.running);
    });
    assert!(
        wait(cx, &mut c, |a| !a.running && a.queue.is_empty(), 8_000),
        "队列未出清"
    );
    c.app.read_with(cx, |a, _| {
        assert!(matches!(a.messages.last(), Some(ChatMsg::Assistant { text, .. }) if text == "r3"));
        let users = a
            .messages
            .iter()
            .filter(|m| matches!(m, ChatMsg::User { .. }))
            .count();
        assert_eq!(users, 3);
    });
}

// ---------- 用例 5：错误恢复 ----------
#[gpui::test]
async fn case05_error_then_recover(cx: &mut TestAppContext) {
    let mut c = setup(cx, vec![Step::Fail("boom".into()), turn("ok-now")]);
    c.app.update(cx, |a, cx| a.send("x".into(), cx));
    assert!(
        wait(cx, &mut c, |a| !a.running && has_notice(a, "boom"), 5_000),
        "错误未收尾"
    );
    c.app.read_with(cx, |a, _| {
        assert!(
            !a.messages.iter().any(|m| matches!(m, ChatMsg::Running)),
            "Running 残留"
        );
    });
    c.app.update(cx, |a, cx| a.send("y".into(), cx));
    assert!(
        wait(
            cx,
            &mut c,
            |a| matches!(a.messages.last(), Some(ChatMsg::Assistant { text, .. }) if text == "ok-now"),
            5_000
        ),
        "错误后无法恢复"
    );
}

// ---------- 用例 6：goal 循环 ----------
#[gpui::test]
async fn case06_goal_loop_reaches_completion(cx: &mut TestAppContext) {
    let mut c = setup(
        cx,
        vec![
            turn("r1"),
            turn("GOAL_CONTINUE"),
            turn("r2"),
            turn("GOAL_COMPLETE"),
        ],
    );
    c.app.update(cx, |a, _| {
        a.goal = Some("ship it".into());
    });
    c.app.update(cx, |a, cx| a.send("start".into(), cx));
    assert!(
        wait(cx, &mut c, |a| a.goal.is_none() && !a.running, 10_000),
        "goal 循环未收官: rounds={:?}",
        c.app.read_with(cx, |a, _| (a.goal.clone(), a.goal_rounds))
    );
    c.app.read_with(cx, |a, _| {
        assert!(has_notice(a, "goal 第 1 轮"), "缺续轮 Notice");
        assert!(has_notice(a, "goal 已达成"), "缺达成 Notice");
        assert!(
            a.messages
                .iter()
                .any(|m| matches!(m, ChatMsg::Assistant { text, .. } if text == "r2")),
            "第二轮回复缺失: {:?}",
            a.messages
        );
        assert!(
            matches!(a.messages.last(), Some(ChatMsg::Notice { .. })),
            "收官后末尾应是 Notice"
        );
    });
}

// ---------- 用例 7：goal 上限 ----------
#[gpui::test]
async fn case07_goal_rounds_cap_terminates(cx: &mut TestAppContext) {
    // 13 轮 × (回合 + 裁判) 交替；第 13 次裁判结果到达时 rounds==12 → 终止
    let mut steps = vec![];
    for i in 0..13 {
        steps.push(turn(&format!("r{i}")));
        steps.push(turn("CONTINUE"));
    }
    let mut c = setup(cx, steps);
    c.app.update(cx, |a, _| {
        a.goal = Some("never done".into());
    });
    c.app.update(cx, |a, cx| a.send("start".into(), cx));
    assert!(
        wait(cx, &mut c, |a| a.goal.is_none() && !a.running, 15_000),
        "goal 上限未终止"
    );
    c.app.read_with(cx, |a, _| {
        assert!(has_notice(a, "12 轮上限"), "缺上限 Notice");
    });
}

// ---------- 用例 8：/clear 与新建会话 ----------
#[gpui::test]
async fn case08_clear_and_new_chat_reset_state(cx: &mut TestAppContext) {
    let mut c = setup(
        cx,
        vec![turn(
            "这是一个足够长的回复内容，用来让历史 token 估算大于零。",
        )],
    );
    c.app.update(cx, |a, cx| {
        a.send(
            "这是一个足够长的提问，用来让历史 token 估算大于零。".into(),
            cx,
        )
    });
    assert!(wait(cx, &mut c, |a| !a.running, 5_000), "回合未完成");
    let est_before = c.app.read_with(cx, |a, _| {
        a.hist
            .try_lock()
            .map(|h| mcode_core::token::raw_estimate_messages(&h.active_messages()))
            .unwrap_or(1)
    });
    assert!(est_before > 0);
    c.app.update(cx, |a, cx| {
        assert!(a.handle_command("/clear", cx));
    });
    c.app.read_with(cx, |a, _| {
        assert!(a.messages.is_empty(), "/clear 未清消息");
        assert_eq!(
            a.hist
                .try_lock()
                .map(|h| mcode_core::token::raw_estimate_messages(&h.active_messages()))
                .unwrap_or(9),
            0
        );
        assert_eq!(a.view, View::Chat);
    });
    c.app.update(cx, |a, cx| {
        a.new_chat(cx);
        assert!(a.messages.len() == 1, "new_chat 立即入列缺可见反馈");
        assert!(is_notice(&a.messages[0], "已新建会话"));
        // 会话在 harness 构建时即落盘创建并入列（用户实测反馈：点击新建后列表无新会话）
        let cur_id = a.current_session_id();
        assert!(cur_id.is_some(), "new_chat 无活动会话");
        assert!(
            a.sessions
                .iter()
                .any(|m| Some(&m.session_id) == cur_id.as_ref()),
            "新建会话未立即入列"
        );
    });
    // 清理 new_chat 落盘的会话（store.dir() 即 bucket 本身）
    if let Ok(store) = SessionStore::for_project(&c.tmp) {
        let _ = std::fs::remove_dir_all(store.dir());
    }
}

// ---------- 用例 9：/compact ----------
#[gpui::test]
async fn case09_compact_shrinks_history(cx: &mut TestAppContext) {
    // 压缩经 LLM 总结，会消耗一次 mock 流（返回短摘要即可缩估计值）
    let mut c = setup(cx, vec![turn("压缩摘要")]);
    c.app.update(cx, |a, _| {
        let mut h = a.hist.try_lock().unwrap();
        for i in 0..10 {
            h.push(Message::user_text(format!("u{i}-{}", "长".repeat(1000))));
            h.push(Message::assistant_text(format!(
                "a{i}-{}",
                "复".repeat(1000)
            )));
        }
    });
    let before = c.app.read_with(cx, |a, _| {
        a.hist
            .try_lock()
            .map(|h| mcode_core::token::raw_estimate_messages(&h.active_messages()))
            .unwrap_or(0)
    });
    assert!(before > 4000, "前置 estimate 异常: {before}");
    c.app.update(cx, |a, cx| {
        assert!(a.handle_command("/compact", cx));
    });
    assert!(
        wait(cx, &mut c, |a| has_notice(a, "已压缩历史"), 10_000),
        "压缩未完成: {:?}",
        c.app.read_with(cx, |a, _| a.messages.clone())
    );
    let after = c.app.read_with(cx, |a, _| {
        a.hist
            .try_lock()
            .map(|h| mcode_core::token::raw_estimate_messages(&h.active_messages()))
            .unwrap_or(0)
    });
    assert!(after < before, "压缩未生效: {before} → {after}");
}

// ---------- 用例 10：权限三路（允许 / 总是允许；拒绝在用例 11） ----------
#[gpui::test]
async fn case10_permission_allow_and_always(cx: &mut TestAppContext) {
    let mut c = setup(
        cx,
        vec![
            Step::ToolTurn(String::new()),
            turn("d1"),
            Step::ToolTurn(String::new()),
            turn("d2"),
            Step::ToolTurn(String::new()),
            turn("d3"),
        ],
    );
    c.app.update(cx, |a, _| {
        a.harness
            .as_ref()
            .unwrap()
            .deps
            .perms
            .try_lock()
            .unwrap()
            .set_mode(Mode::Manual);
        a.mode = Mode::Manual;
    });
    // cycle1：Confirm → 允许
    c.app.update(cx, |a, cx| a.send("go1".into(), cx));
    assert!(
        wait(cx, &mut c, |a| a.pending_permission.is_some(), 5_000),
        "未弹权限确认"
    );
    c.app.read_with(cx, |a, _| {
        assert_eq!(a.pending_permission.as_ref().unwrap().tool, "bash")
    });
    c.app
        .update(cx, |a, cx| a.resolve_permission(Decision::Allow, cx));
    assert!(
        wait(
            cx,
            &mut c,
            |a| !a.running && a.pending_permission.is_none(),
            5_000
        ),
        "允许后未收尾"
    );
    // cycle2：Confirm → 总是允许（会话内沉淀）
    c.app.update(cx, |a, cx| a.send("go2".into(), cx));
    assert!(
        wait(cx, &mut c, |a| a.pending_permission.is_some(), 5_000),
        "第二次未弹确认"
    );
    c.app
        .update(cx, |a, cx| a.resolve_permission(Decision::AllowAlways, cx));
    assert!(wait(cx, &mut c, |a| !a.running, 5_000), "总是允许后未收尾");
    c.app.read_with(cx, |a, _| {
        let v = a
            .harness
            .as_ref()
            .unwrap()
            .deps
            .perms
            .try_lock()
            .unwrap()
            .evaluate(&PermCheck {
                tool: "bash",
                is_read_only: false,
                command: Some(""),
                target: None,
            });
        assert!(matches!(v, Verdict::Allow), "AllowAlways 未沉淀");
    });
    // cycle3：不再弹确认
    c.app.update(cx, |a, cx| a.send("go3".into(), cx));
    assert!(wait(cx, &mut c, |a| !a.running, 5_000), "沉淀后回合未完成");
    c.app.read_with(cx, |a, _| {
        assert!(a.pending_permission.is_none(), "沉淀后仍弹确认");
        assert!(matches!(a.messages.last(), Some(ChatMsg::Assistant { text, .. }) if text == "d3"));
    });
}

// ---------- 用例 11：ESC 优先级链（补全 → 权限拒绝） ----------
#[gpui::test]
async fn case11_esc_priority_chain(cx: &mut TestAppContext) {
    let c = setup(cx, vec![turn("x")]);
    // ① 补全开 → ESC 只关补全
    c.app.update(cx, |a, cx| {
        a.input.update(cx, |i, cx| i.set_text("/", cx));
    });
    c.app.update(cx, |a, _| {
        assert!(a.completion.is_open(), "补全未触发");
        assert!(
            a.candidates.iter().any(|c| c.name == "/clear"),
            "指令候选缺失"
        );
    });
    c.app.update(cx, |a, cx| a.interrupt(cx));
    c.app.read_with(cx, |a, _| {
        assert!(!a.completion.is_open(), "ESC 未关补全");
        assert!(!a.running);
        assert!(a.pending_permission.is_none());
    });
    // ② 权限弹窗 → ESC = Deny 回传
    let (tx, mut rx) = tokio::sync::oneshot::channel();
    c.app.update(cx, |a, _| {
        a.pending_permission = Some(crate::app::msg::PendingPermission {
            tool: "bash".into(),
            args_summary: "echo hi".into(),
            respond: Some(tx),
        });
    });
    c.app.update(cx, |a, cx| a.interrupt(cx));
    c.app.read_with(cx, |a, _| {
        assert!(a.pending_permission.is_none(), "ESC 未清弹窗")
    });
    let decision = match rx.try_recv() {
        Ok(d) => d,
        Err(_) => panic!("Deny 未回传"),
    };
    assert!(matches!(decision, Decision::Deny));
    // ③ running → ESC 打断（协作用于完整链路，见用例 3；此处只断言优先级末位不误触发视图切换）
    c.app.read_with(cx, |a, _| assert_eq!(a.view, View::Chat));
}

// ---------- 用例 12：hist 锁超时防御 ----------
#[gpui::test]
async fn case12_hist_lock_timeout_defense(cx: &mut TestAppContext) {
    let mut c = setup(cx, vec![turn("later")]);
    // 测试线程持有 hist 锁，模拟"上游任务异常持锁"
    let hist = c.app.read_with(cx, |a, _| a.hist.clone());
    let guard = hist.try_lock().unwrap();
    c.app.update(cx, |a, cx| a.send("blocked".into(), cx));
    assert!(
        wait(
            cx,
            &mut c,
            |a| !a.running && has_notice(a, "锁被占用"),
            8_000
        ),
        "锁超时防御未生效"
    );
    c.app.read_with(cx, |a, _| {
        assert!(
            !a.messages.iter().any(|m| matches!(m, ChatMsg::Running)),
            "Running 残留"
        );
    });
    drop(guard);
    // 锁释放后新回合正常完成
    c.app.update(cx, |a, cx| a.send("after".into(), cx));
    assert!(
        wait(
            cx,
            &mut c,
            |a| matches!(a.messages.last(), Some(ChatMsg::Assistant { text, .. }) if text == "later"),
            5_000
        ),
        "锁释放后新回合未完成"
    );
}

#[test]
fn watch_isolation() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let (tx, rx) = tokio::sync::watch::channel(false);
    let h = rt.spawn(async move {
        let mut c = rx;
        loop {
            if *c.borrow() {
                return 1u8;
            }
            let _ = c.changed().await;
        }
    });
    tx.send(true).unwrap();
    let r = rt.block_on(async { tokio::time::timeout(std::time::Duration::from_secs(2), h).await });
    match r {
        Ok(Ok(1)) => println!("WATCH-ISOLATION-OK"),
        other => println!("WATCH-ISOLATION-FAIL: {other:?}"),
    }
}

// ---------- 用例 16：思考块默认展开 + 流式累计 ----------
#[gpui::test]
async fn case16_thinking_block_streams_expanded(cx: &mut TestAppContext) {
    let c = setup(cx, vec![turn("done")]);
    // 直接注入 Thinking 事件（泵已在事件链路，等价 on_agent_event 路径）
    c.app.update(cx, |a, cx| {
        a.on_agent_event(
            AgentEvent::Thinking {
                text: "先读源码".into(),
                scope: None,
            },
            cx,
        );
    });
    c.app.update(cx, |a, cx| {
        a.on_agent_event(
            AgentEvent::Thinking {
                text: "，再写测试".into(),
                scope: None,
            },
            cx,
        );
    });
    c.app.read_with(cx, |a, _| {
        let t = a.messages.iter().find_map(|m| match m {
            ChatMsg::Thinking { text, fold } => Some((text.clone(), *fold)),
            _ => None,
        });
        let (text, fold) = t.expect("无 Thinking 块");
        assert_eq!(
            fold,
            crate::app::msg::FoldState::Expanded,
            "默认应展开（thinking_default_expanded=true）"
        );
        assert_eq!(text, "先读源码，再写测试", "Thinking 增量未累计");
    });
}

// ---------- 用例 17：追加不重置 expanded ----------
#[gpui::test]
async fn case17_thinking_append_keeps_expanded_state(cx: &mut TestAppContext) {
    let c = setup(cx, vec![turn("x")]);
    c.app.update(cx, |a, cx| {
        a.on_agent_event(
            AgentEvent::Thinking {
                text: "a".into(),
                scope: None,
            },
            cx,
        );
    });
    // 用户手动折叠
    c.app.update(cx, |a, _| {
        if let Some(ChatMsg::Thinking { fold, .. }) = a.messages.last_mut() {
            *fold = crate::app::msg::FoldState::Collapsed;
        }
    });
    c.app.update(cx, |a, cx| {
        a.on_agent_event(
            AgentEvent::Thinking {
                text: "b".into(),
                scope: None,
            },
            cx,
        );
    });
    c.app.read_with(cx, |a, _| match a.messages.last() {
        Some(ChatMsg::Thinking { text, fold }) => {
            assert_eq!(
                *fold,
                crate::app::msg::FoldState::Collapsed,
                "追加不应重置折叠状态"
            );
            assert_eq!(text, "ab", "追加文本丢失");
        }
        other => panic!("Thinking 块丢失: {other:?}"),
    });
}

// ---------- 用例 18：三态循环切换 ----------
#[gpui::test]
async fn case18_fold_state_cycle(cx: &mut TestAppContext) {
    let c = setup(cx, vec![turn("x")]);
    c.app.update(cx, |a, _| {
        a.messages.push(ChatMsg::Assistant {
            text: "答案".into(),
            fold: crate::app::msg::FoldState::Expanded,
        });
        use crate::app::msg::FoldState::*;
        assert_eq!(Expanded.next(), Preview);
        assert_eq!(Preview.next(), Collapsed);
        assert_eq!(Collapsed.next(), Expanded);
        // 循环切换经 cycle_message_fold
        a.cycle_message_fold(0);
        assert!(matches!(&a.messages[0], ChatMsg::Assistant { fold, .. } if *fold == Preview));
        a.cycle_message_fold(0);
        a.cycle_message_fold(0);
        assert!(matches!(&a.messages[0], ChatMsg::Assistant { fold, .. } if *fold == Expanded));
    });
}

// ---------- 用例 19：长消息默认 Preview ----------
#[gpui::test]
async fn case19_long_reply_defaults_to_preview(cx: &mut TestAppContext) {
    let long_reply = (0..40)
        .map(|i| format!("第 {i} 行"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut c = setup(cx, vec![turn(&long_reply)]);
    c.app.update(cx, |a, cx| a.send("go".into(), cx));
    assert!(wait(cx, &mut c, |a| !a.running, 5_000), "回合未完成");
    c.app.read_with(cx, |a, _| match a.messages.last() {
        Some(ChatMsg::Assistant { fold, .. }) => {
            assert_eq!(
                *fold,
                crate::app::msg::FoldState::Preview,
                ">30 行默认 Preview"
            );
        }
        other => panic!("无 Assistant: {other:?}"),
    });
}

// ---------- 用例 20：流式追加不重置用户已选折叠态 ----------
#[gpui::test]
async fn case20_streaming_append_keeps_user_fold(cx: &mut TestAppContext) {
    let c = setup(cx, vec![turn("第一段 第二段")]);
    c.app.update(cx, |a, cx| {
        a.on_agent_event(AgentEvent::TextDelta("第一段".into()), cx);
        // 用户在流式期间切到 Preview
        if let Some(ChatMsg::Assistant { fold, .. }) = a.messages.last_mut() {
            *fold = crate::app::msg::FoldState::Preview;
        }
        a.on_agent_event(AgentEvent::TextDelta(" 第二段".into()), cx);
    });
    c.app.read_with(cx, |a, _| match a.messages.last() {
        Some(ChatMsg::Assistant { text, fold }) => {
            assert_eq!(
                *fold,
                crate::app::msg::FoldState::Preview,
                "流式追加不应重置折叠态"
            );
            assert_eq!(text, "第一段 第二段");
        }
        other => panic!("无 Assistant: {other:?}"),
    });
}
