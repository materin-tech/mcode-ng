//! print 模式：`mcode -p "<prompt>"` 一次性执行——非交互路径，同时是
//! 交互层与核心装配的 E2E 冒烟通道（CI 里用假 backend 走全链）。

use crate::session_setup::{build_runtime, SessionRuntime};
use anyhow::Result;
use mcode_core::agent::runner::{run_agent_turn, RunOpts};
use mcode_core::agent::{AgentEvent, EventSink};
use mcode_core::message::{Conversation, Message};
use mcode_core::sessions::SessionStore;
use std::sync::Arc;

/// 系统提示（TUI 复用；对齐 CC 的「以当前工作区为根干活」基线）。
pub fn system_prompt(cwd: &std::path::Path) -> Vec<String> {
    vec![format!(
        "你是 MCode，运行在用户机器上的终端编码代理。\
         工作区：{}。\
         - 用工具读代码/改代码/跑命令，给出小步、可验证的改动；\
         - 改动前先看现状（read-before-edit 已在工具层强制）；\
         - 危险或不可逆操作先向用户确认；\
         - 回复保持简洁，中文优先。",
        cwd.display()
    )]
}

/// 组回合参数：系统提示 + 全量工具清单。`model_override` 供交互层 /model。
pub fn turn_opts(
    rt: &SessionRuntime,
    cwd: &std::path::Path,
    model_override: Option<&str>,
) -> RunOpts {
    let mut opts = RunOpts::new(system_prompt(cwd))
        .with_tools(
            rt.deps
                .registry
                .definitions()
                .iter()
                .map(|d| d.to_spec())
                .collect(),
        )
        // 0 = 不限轮数（CC 交互态）；print 模式同交互态。
        .with_max_turns(0);
    if let Some(m) = model_override {
        opts = opts.with_model(m);
    }
    opts
}

/// 跑一轮：装配 → push 用户消息（落盘 checkpoint）→ 回合循环。
/// 返回最终文本。`emit` 交给调用方（print 打 stdout / 测试收集断言）。
pub async fn run_turn(
    rt: &SessionRuntime,
    conv: &mut Conversation,
    prompt: &str,
    emit: EventSink,
) -> mcode_core::agent::runner::RunOutcome {
    conv.push(Message::user_text(prompt));
    // 用户输入消息由调用方落盘（checkpoint = 回合入口的 user 消息）。
    rt.deps
        .record_message(conv.messages.last().expect("刚 push 必有"));
    run_prepared(rt, conv, None, emit).await
}

/// 回合执行（用户消息已由调用方 push + 落盘）。TUI 自己 push 以拿到
/// checkpoint id 供 rewind 用；`model_override` 对应 /model 切换。
pub async fn run_prepared(
    rt: &SessionRuntime,
    conv: &mut Conversation,
    model_override: Option<&str>,
    emit: EventSink,
) -> mcode_core::agent::runner::RunOutcome {
    run_agent_turn(
        &rt.deps,
        conv,
        &turn_opts(rt, &rt.deps.tool_ctx.cwd, model_override),
        &emit,
    )
    .await
}

/// print 入口：`-p` 一次性回合。`resume` 支持 Fresh/Continue/Id（Pick 只在 TUI）。
pub async fn run_print(
    prompt: &str,
    model: Option<&str>,
    fallback: Option<Vec<String>>,
    resume: crate::tui::Resume,
) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let config = mcode_core::config::Config::load()?;
    let desktop = mcode_core::config::DesktopConfig::load()?;
    let mut rt = build_runtime(&cwd, &config, &desktop, model, fallback, None).await?;
    // 非交互态无人应答确认弹窗：headless 下需要确认的工具直接跳过。
    rt.deps.confirm = None;

    let mut conv = match resume {
        crate::tui::Resume::Fresh => Conversation::new(),
        crate::tui::Resume::Continue => {
            let store = SessionStore::for_project(&cwd)?;
            let recent = store.list().first().map(|s| s.session_id.clone());
            match recent {
                Some(id) => store.load(&id)?.0,
                None => Conversation::new(),
            }
        }
        crate::tui::Resume::Id(id) => SessionStore::for_project(&cwd)?.load(&id)?.0,
        crate::tui::Resume::Pick => {
            anyhow::bail!("print 模式不支持会话选择：请用 --resume <id>")
        }
    };
    // 文本已随 TextDelta 流式上屏 → 收尾不再重打 reply（否则输出重复一遍）
    let streamed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = streamed.clone();
    let sink: EventSink = Arc::new(move |e| {
        if matches!(e, AgentEvent::TextDelta(_)) {
            flag.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        print_sink(e);
    });
    let outcome = run_turn(&rt, &mut conv, prompt, sink).await;
    if let Some(err) = outcome.error {
        eprintln!("错误：{err}");
        std::process::exit(1);
    }
    if !outcome.reply.is_empty() && !streamed.load(std::sync::atomic::Ordering::Relaxed) {
        println!("{}", outcome.reply);
    }
    Ok(())
}

/// print sink：文本/工具结果直接上 stdout，状态进 stderr。
fn print_sink(e: AgentEvent) {
    match e {
        AgentEvent::TextDelta(t) => print!("{t}"),
        AgentEvent::ToolResult { name, success, .. } => {
            let mark = if success { "✓" } else { "✗" };
            eprintln!("[{mark} {name}]");
        }
        AgentEvent::Retrying {
            attempt,
            max,
            reason,
        } => {
            eprintln!("[重试 {attempt}/{max}] {reason}");
        }
        AgentEvent::Recovering { action } => eprintln!("[恢复] {action}"),
        AgentEvent::Compacted { note } => eprintln!("[已压缩] {note}"),
        AgentEvent::Error { message } => eprintln!("[错误] {message}"),
        AgentEvent::Done { .. } => println!(),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcode_core::agent::runner::MakeBackend;
    use mcode_core::api::{ModelBackend, StreamEvent};
    use mcode_core::config::{Config, Protocol, Provider};
    use mcode_core::message::StopReason;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    type ScriptQueue = Arc<Mutex<VecDeque<Result<Vec<StreamEvent>, mcode_core::api::ApiError>>>>;

    /// 脚本化后端：首轮调 write_file 工具，次轮文本收尾。
    struct ScriptBackend {
        script: ScriptQueue,
    }

    #[async_trait::async_trait]
    impl ModelBackend for ScriptBackend {
        fn backend_name(&self) -> &'static str {
            "script"
        }
        async fn stream(
            &self,
            _req: mcode_core::api::StreamRequest,
        ) -> std::result::Result<
            futures::stream::BoxStream<
                'static,
                std::result::Result<StreamEvent, mcode_core::api::ApiError>,
            >,
            mcode_core::api::ApiError,
        > {
            let next = self.script.lock().unwrap().pop_front().unwrap_or_else(|| {
                Ok(vec![StreamEvent::Done {
                    stop_reason: StopReason::EndTurn,
                }])
            });
            match next {
                Ok(events) => Ok(Box::pin(futures::stream::iter(events.into_iter().map(Ok)))),
                Err(e) => Err(e),
            }
        }
    }

    fn test_config() -> Config {
        #[allow(clippy::field_reassign_with_default)]
        let mut cfg = Config::default();
        cfg.add_provider(Provider {
            name: "test".into(),
            base_url: "http://127.0.0.1:9".into(),
            api_key_env: "MCODE_TEST_KEY".into(),
            models: vec!["test-model".into()],
            multimodal_models: vec![],
            protocol: Protocol::Anthropic,
        })
        .unwrap();
        cfg.set_active_provider("test").unwrap();
        cfg.active_model = Some("test-model".into());
        cfg
    }

    /// 用临时目录 store + 脚本 backend 走完整 print 链路。
    #[tokio::test]
    async fn print_flow_end_to_end_records_session() {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = std::env::temp_dir().join(format!("mcode-print-e2e-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let script: ScriptQueue = Arc::default();
        script.lock().unwrap().push_back(Ok(vec![
            StreamEvent::TextDelta("改好了".into()),
            StreamEvent::Done {
                stop_reason: StopReason::EndTurn,
            },
        ]));
        let make_backend: MakeBackend = {
            let script = script.clone();
            Arc::new(move |_: &str| {
                Ok(Arc::new(ScriptBackend {
                    script: script.clone(),
                }) as Arc<dyn ModelBackend>)
            })
        };

        let config = test_config();
        let desktop = mcode_core::config::DesktopConfig::default();
        let rt = build_runtime(
            &tmp,
            &config,
            &desktop,
            Some("test-model"),
            None,
            Some(make_backend),
        )
        .await
        .unwrap();

        // 工具表应有：内置 + skill + task 三派发
        let names = rt.deps.registry.names();
        assert!(names.contains(&"task".to_string()), "{names:?}");
        assert!(names.contains(&"skill".to_string()), "{names:?}");
        assert!(names.contains(&"bash".to_string()), "{names:?}");

        let events: Arc<Mutex<Vec<AgentEvent>>> = Arc::default();
        let sink_events = events.clone();
        let emit: EventSink = Arc::new(move |e| sink_events.lock().unwrap().push(e));

        let mut conv = Conversation::new();
        let outcome = run_turn(&rt, &mut conv, "写点什么", emit).await;
        assert!(
            outcome.error.is_none(),
            "{}",
            outcome.error.as_deref().unwrap_or_default()
        );
        assert_eq!(outcome.reply, "改好了");

        // 会话落盘：用户消息 + 助手回复都在 JSONL
        let session = rt.deps.session.as_ref().unwrap();
        let lines = std::fs::read_to_string(session.path()).unwrap();
        assert!(lines.contains("写点什么"), "{lines}");
        assert!(lines.contains("改好了"), "{lines}");

        // 事件契约：Done 收尾
        let ev = events.lock().unwrap();
        assert!(matches!(ev.last(), Some(AgentEvent::Done { .. })), "{ev:?}");

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
