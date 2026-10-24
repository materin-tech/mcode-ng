// MCode_Desktop — AI coding agent desktop app
// Copyright (c) 2026 Materin-tech. All rights reserved.
// SPDX-License-Identifier: MIT

//! 交互场景测试矩阵（M2 全接线回归）— gpui TestAppContext（#[gpui::test]）无头驱动 App 状态机：
//! 直接调 send()/on_ui_event()/interrupt()（按键级 IME 路径不在本矩阵范围），
//! MockBackend 按调用序脚本化返回流事件，事件经 take_rx 的 channel 由测试泵回 on_ui_event。

use crate::app::App;
use crate::app::View;
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
use mcode_core::transcript::{Block, ToolStatus};
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

pub(crate) fn has_notice(a: &App, contains: &str) -> bool {
    a.transcript
        .blocks
        .iter()
        .any(|b| matches!(b, Block::Notice { text, .. } if text.contains(contains)))
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
                && matches!(a.transcript.blocks.last(), Some(Block::Assistant { text, .. }) if text.contains("你好，世界")),
            5_000
        ),
        "回合未完成: {:?}",
        c.app
            .read_with(cx, |a, _| format!("{:?}", a.transcript.blocks))
    );
    c.app.read_with(cx, |a, _| {
        assert!(matches!(&a.transcript.blocks[0], Block::User { text } if text == "hello"));
        assert!(a.current_session_id().is_some(), "会话未落盘");
        assert!(!a.transcript.busy, "spinner 未收尾");
    });
}

// ---------- 用例 2：工具回合 ----------
#[gpui::test]
async fn case02_tool_turn_runs_and_feeds_back(cx: &mut TestAppContext) {
    let mut c = setup(cx, vec![Step::ToolTurn(String::new()), turn("done")]);
    c.app.update(cx, |a, cx| a.send("run echo".into(), cx));
    assert!(wait(cx, &mut c, |a| !a.running, 5_000), "工具回合未完成");
    c.app.read_with(cx, |a, _| {
        let card = a.transcript.blocks.iter().find_map(|b| match b {
            Block::Tool(t) if t.tool == "bash" => Some(t),
            _ => None,
        });
        let t = card.expect("无 bash 工具块");
        assert_eq!(t.tool, "bash");
        match &t.status {
            ToolStatus::Resolved { success, output, .. } => {
                assert!(success, "工具失败");
                assert!(output.contains("hi"), "输出缺失: {output}");
            }
            other => panic!("工具卡未回填: {other:?}"),
        }
        assert!(
            matches!(a.transcript.blocks.last(), Some(Block::Assistant { text, .. }) if text == "done")
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
        wait(cx, &mut c, |a| !a.running && !a.transcript.busy, 20_000),
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
                && a.transcript
                    .blocks
                    .iter()
                    .any(|b| matches!(b, Block::Assistant { text, .. } if text == "second-ok")),
            5_000
        ),
        "打断后新回合未完成（回归失败）: blocks={:?}",
        c.app
            .read_with(cx, |a, _| format!("{:?}", a.transcript.blocks))
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
        assert!(matches!(a.transcript.blocks.last(), Some(Block::Assistant { text, .. }) if text == "r3"));
        let users = a
            .transcript
            .blocks
            .iter()
            .filter(|b| matches!(b, Block::User { .. }))
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
        assert!(!a.transcript.busy, "spinner 未收尾");
    });
    c.app.update(cx, |a, cx| a.send("y".into(), cx));
    assert!(
        wait(
            cx,
            &mut c,
            |a| matches!(a.transcript.blocks.last(), Some(Block::Assistant { text, .. }) if text == "ok-now"),
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
            a.transcript
                .blocks
                .iter()
                .any(|b| matches!(b, Block::Assistant { text, .. } if text == "r2")),
            "第二轮回复缺失: {:?}",
            a.transcript.blocks
        );
        assert!(
            matches!(a.transcript.blocks.last(), Some(Block::Notice { .. })),
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
        assert!(a.transcript.blocks.is_empty(), "/clear 未清消息");
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
        assert!(a.transcript.blocks.len() == 1, "new_chat 立即入列缺可见反馈");
        assert!(matches!(&a.transcript.blocks[0], Block::Notice { text, .. } if text.contains("已新建会话")));
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
        wait(cx, &mut c, |a| has_notice(a, "已压缩"), 10_000),
        "压缩未完成: {:?}",
        c.app
            .read_with(cx, |a, _| format!("{:?}", a.transcript.blocks))
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
        assert!(a.pending_permission.is_none(), "沉淀后新增工具调用不再弹确认");
        assert!(matches!(a.transcript.blocks.last(), Some(Block::Assistant { text, .. }) if text == "d3"));
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
            a.candidates.iter().any(|c| c.name == "clear"),
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
        assert!(!a.transcript.busy, "spinner 未收尾");
    });
    drop(guard);
    // 锁释放后新回合正常完成
    c.app.update(cx, |a, cx| a.send("after".into(), cx));
    assert!(
        wait(
            cx,
            &mut c,
            |a| matches!(a.transcript.blocks.last(), Some(Block::Assistant { text, .. }) if text == "later"),
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

// ---------- 用例 16：Thinking 增量累计为单块 ----------
#[gpui::test]
async fn case16_thinking_streams_into_single_block(cx: &mut TestAppContext) {
    let c = setup(cx, vec![turn("done")]);
    c.app.update(cx, |a, cx| {
        a.on_agent_event(
            AgentEvent::Thinking {
                text: "先读源码".into(),
                scope: None,
            },
            cx,
        );
        a.on_agent_event(
            AgentEvent::Thinking {
                text: "，再写测试".into(),
                scope: None,
            },
            cx,
        );
    });
    c.app.read_with(cx, |a, _| {
        let t = a.transcript.blocks.iter().find_map(|b| match b {
            Block::Thinking { text, .. } => Some(text.clone()),
            _ => None,
        });
        let text = t.expect("无 Thinking 块");
        assert_eq!(text, "先读源码，再写测试", "Thinking 增量未累计");
    });
}

// ---------- 用例 17：detail_view 切换（CLI ctrl+o 对应交互） ----------
#[gpui::test]
async fn case17_detail_view_toggle(cx: &mut TestAppContext) {
    let c = setup(cx, vec![turn("x")]);
    c.app.read_with(cx, |a, _| {
        assert!(!a.transcript.detail_view, "详情默认关闭");
    });
    c.app.update(cx, |a, cx| {
        a.on_agent_event(
            AgentEvent::Thinking {
                text: "可见性只影响渲染层".into(),
                scope: None,
            },
            cx,
        );
    });
    c.app.update(cx, |a, _| {
        a.toggle_detail();
        assert!(a.transcript.detail_view, "切换后应展开");
    });
    c.app.update(cx, |a, _| {
        a.toggle_detail();
        assert!(!a.transcript.detail_view, "二次切换应收起");
    });
    // 详情开关不动块数据：块内全文保留（收起/展开是渲染口径，非数据口径）
    c.app.read_with(cx, |a, _| {
        assert!(
            a.transcript
                .blocks
                .iter()
                .any(|b| matches!(b, Block::Thinking { text, .. } if text == "可见性只影响渲染层")),
            "详情开关丢失块数据"
        );
    });
}

// ---------- 用例 18：interrupt 产生 Interrupted 块 ----------
#[gpui::test]
async fn case18_interrupt_appends_interrupted_block(cx: &mut TestAppContext) {
    let mock = Arc::new(HangMock::new(vec![]));
    let mut c = setup_backend(cx, mock.clone());
    c.app.update(cx, |a, cx| a.send("first".into(), cx));
    assert!(wait(cx, &mut c, |a| a.running, 5_000), "回合未启动");
    c.app.update(cx, |a, cx| a.interrupt(cx));
    assert!(
        wait(cx, &mut c, |a| !a.running && !a.transcript.busy, 20_000),
        "ESC 后未收尾"
    );
    c.app.read_with(cx, |a, _| {
        assert!(
            a.transcript
                .blocks
                .iter()
                .any(|b| matches!(b, Block::Interrupted)),
            "打断后缺 Interrupted 块"
        );
    });
}

// ---------- 用例 19：TextDelta 流式累计为单条 Assistant ----------
#[gpui::test]
async fn case19_text_delta_accumulates(cx: &mut TestAppContext) {
    let c = setup(cx, vec![turn("x")]);
    c.app.update(cx, |a, cx| {
        a.on_agent_event(AgentEvent::TextDelta("第一段".into()), cx);
        a.on_agent_event(AgentEvent::TextDelta(" 第二段".into()), cx);
    });
    c.app.read_with(cx, |a, _| {
        let n = a
            .transcript
            .blocks
            .iter()
            .filter(|b| matches!(b, Block::Assistant { .. }))
            .count();
        assert_eq!(n, 1, "流式增量应进同一块");
        assert!(
            matches!(
                a.transcript.blocks.last(),
                Some(Block::Assistant { text, .. }) if text == "第一段 第二段"
            ),
            "增量丢失: {:?}",
            a.transcript.blocks
        );
    });
}

// ---------- 用例 20：Usage 事件累计 tokens ----------
#[gpui::test]
async fn case20_usage_totals(cx: &mut TestAppContext) {
    use mcode_core::message::Usage;
    let c = setup(cx, vec![turn("x")]);
    c.app.update(cx, |a, cx| {
        a.on_agent_event(
            AgentEvent::Usage(Usage {
                input_tokens: 100,
                output_tokens: 20,
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 0,
            }),
            cx,
        );
    });
    c.app.read_with(cx, |a, _| {
        assert_eq!(a.tokens_total, 120, "Usage 未累计");
    });
}
