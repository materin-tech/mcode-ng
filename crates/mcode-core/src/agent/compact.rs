//! 上下文压缩 —— proactive 门控 + reactive continue 点。
//!
//! 对齐 CC 的 auto-compact：
//! - **proactive**：回合开始前估算 tokens ≥ 窗口 80% → 压缩后再跑（runner 调 [`should_compact`]）；
//! - **reactive**：API 返回 ContextOverflow → 压缩 → 重放（runner 的恢复路径）。
//!
//! 压缩动作（日志追加式，不删历史）：
//! 活跃主链切成「摘要 + 最近 [`KEEP_RECENT`] 条」。摘要消息用
//! `synthetic` 与 `summarizes=[from,to]` 标记（UI 折叠条渲染），作为**新根**插入。
//! 分界后的第一条消息 reparent 到摘要下——active_path 从此不经过被概括
//! 的历史，但日志里全都还在（transcript 可回看）。
//!
//! 摘要由**同一个后端**用结构化指令生成（8 段式）；LLM 失败时回落机械
//! 折叠——压缩是恢复路径，绝不能因摘要失败把会话打死。

use crate::agent::runner::AgentDeps;
use crate::agent::{auto_compact_threshold, AgentEvent, EventSink};
use crate::message::{ContentBlock, Conversation, Message, Role};
use anyhow::Result;

/// 压缩后保留的最近消息条数下限（避免摘要后立刻又不够上下文）。
pub const KEEP_RECENT: usize = 12;

/// 单条消息被送进摘要 prompt 的最大字符数（超长工具结果折叠）。
const MAX_MSG_CHARS_FOR_SUMMARY: usize = 4_000;

/// proactive 门控：活跃主链的估算 tokens 是否已达压缩阈值。
pub fn should_compact(deps: &AgentDeps, conv: &Conversation) -> bool {
    let model = deps.current_model();
    let used = (deps.token_estimator)(&conv.active_messages());
    used >= auto_compact_threshold(&model)
}

/// 生成压缩指令（对齐 CC 的 /compact 摘要段结构）。
pub fn compact_instructions() -> String {
    "你是一个对话压缩器。请把下面的对话历史压缩成一份结构化摘要，供后续对话作为唯一上下文使用。\
     按以下段落组织（没有内容的段落省略）：\n\
     1. 任务与目标：用户要做什么，验收标准\n\
     2. 关键决策：已确定的技术方案/约定（含为什么）\n\
     3. 已完成：按文件/模块列出改动\n\
     4. 进行中：当前卡在哪一步，下一步动作\n\
     5. 重要文件：路径 + 一句话作用\n\
     6. 工具教训：失败的命令/路径/参数，避免重复踩坑\n\
     7. 用户偏好：明确表达过的要求\n\
     8. 待办：剩余工作清单\n\
     用简洁的要点列表，保留具体路径/命令/数字，不要泛泛而谈。直接输出摘要，不要开场白。"
        .to_string()
}

/// 摘要请求的用户内容：把历史渲染成带序号的文本。
pub fn render_history_for_summary(messages: &[Message]) -> String {
    let mut out = String::new();
    for (i, m) in messages.iter().enumerate() {
        let role = match m.role {
            Role::User => "USER",
            Role::Assistant => "ASSISTANT",
        };
        let mut body = m.text();
        // 工具块补充结构信息（text() 只拼纯文本）
        for b in &m.content {
            if let ContentBlock::ToolUse { id, name, input } = b {
                body.push_str(&format!(
                    "\n[tool_use id={id} name={name} args={}]",
                    serde_json::to_string(input).unwrap_or_default()
                ));
            }
        }
        if body.chars().count() > MAX_MSG_CHARS_FOR_SUMMARY {
            let head: String = body.chars().take(MAX_MSG_CHARS_FOR_SUMMARY).collect();
            body = format!("{head}\n…[本条超长，已折叠]");
        }
        out.push_str(&format!("--- [{i}] {role} ---\n{body}\n"));
    }
    out
}

/// 执行压缩：LLM 摘要 + 保留最近 [`KEEP_RECENT`] 条。
///
/// 就地改写 `conv`：摘要消息入日志（synthetic），分界后第一条 reparent。
/// 返回摘要文本。
pub async fn compact_conversation(
    deps: &AgentDeps,
    conv: &mut Conversation,
    emit: &EventSink,
) -> Result<String> {
    let active = conv.active_messages();
    let total = active.len();
    anyhow::ensure!(
        total > KEEP_RECENT,
        "消息数 {total} ≤ {KEEP_RECENT}，无需压缩"
    );
    let split = total - KEEP_RECENT;
    let from_id = active[0].id.clone();
    let to_id = active[split - 1].id.clone();
    let first_tail_id = active[split].id.clone();
    let to_summarize: Vec<Message> = active[..split].to_vec();
    let before_est = (deps.token_estimator)(&active);

    // 1) LLM 结构化摘要（失败 → 机械折叠，压缩永不失败会话）
    let model = deps.current_model();
    let summary = match summarize_via_llm(deps, &to_summarize, &model).await {
        Ok(s) if !s.trim().is_empty() => s,
        _ => mechanical_summary(&to_summarize),
    };

    // 2) 摘要消息：新根（parent=None），synthetic + summarizes 标记
    let mut sum_msg = Message::user_text(format!(
        "<context-summary>\n以下是此前对话的结构化摘要（原始消息已压缩，本摘要是唯一上下文）：\n\n{summary}\n\n</context-summary>\n\n请基于以上摘要继续任务。"
    ));
    sum_msg.synthetic = true;
    sum_msg.summarizes = Some((from_id, to_id));

    // 3) 分界后第一条 reparent 到摘要下；叶保持指向旧尾——
    //    active_path 从叶回溯时经 first_tail 改道进摘要，不再经过被概括历史
    if let Some(m) = conv.messages.iter_mut().find(|m| m.id == first_tail_id) {
        m.parent_id = Some(sum_msg.id.clone());
    }
    conv.messages.push(sum_msg);

    let after_est = (deps.token_estimator)(&conv.active_messages());
    emit(AgentEvent::Compacted {
        note: format!(
            "上下文压缩完成：{total} 条消息 → 摘要 + {} 条最近消息（估算 {before_est} → {after_est} tokens）",
            total - split
        ),
    });
    Ok(summary)
}

async fn summarize_via_llm(deps: &AgentDeps, history: &[Message], model: &str) -> Result<String> {
    use crate::api::{StreamEvent, StreamRequest};
    use futures::StreamExt;

    let backend = (deps.make_backend)(model)?;
    let req = StreamRequest {
        model: model.to_string(),
        system: vec![compact_instructions()],
        messages: vec![crate::message::ApiMessage::User {
            blocks: vec![ContentBlock::Text {
                text: render_history_for_summary(history),
            }],
        }],
        tools: vec![],
        max_output_tokens: 8_000,
        thinking: None,
    };
    let mut stream = backend.stream(req).await?;
    let mut text = String::new();
    while let Some(ev) = stream.next().await {
        match ev {
            Ok(StreamEvent::TextDelta(t)) => text.push_str(&t),
            Ok(StreamEvent::Done { .. }) => break,
            Ok(_) => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(text)
}

/// 机械摘要（LLM 不可用时的兜底）：保留各角色的开头片段。
pub fn mechanical_summary(messages: &[Message]) -> String {
    let mut out = String::from("[自动摘要（LLM 摘要失败，机械折叠）]\n");
    for m in messages {
        let role = match m.role {
            Role::User => "USER",
            Role::Assistant => "ASSISTANT",
        };
        let text = m.text();
        let text: String = text.chars().take(300).collect();
        if !text.trim().is_empty() {
            out.push_str(&format!("- {role}: {text}\n"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_instructions_mentions_all_sections() {
        let s = compact_instructions();
        for kw in [
            "任务与目标",
            "关键决策",
            "已完成",
            "进行中",
            "工具教训",
            "待办",
        ] {
            assert!(s.contains(kw));
        }
    }

    #[test]
    fn render_history_includes_tool_use_and_truncates() {
        let m = Message::new(
            Role::Assistant,
            vec![
                ContentBlock::text("我来查"),
                ContentBlock::ToolUse {
                    id: "t1".into(),
                    name: "grep".into(),
                    input: serde_json::json!({"pattern": "x"}),
                },
            ],
        );
        let long = Message::user_text("y".repeat(MAX_MSG_CHARS_FOR_SUMMARY + 500));
        let out = render_history_for_summary(&[m, long]);
        assert!(out.contains("[0] ASSISTANT"));
        assert!(out.contains("name=grep"));
        assert!(out.contains("[1] USER"));
        assert!(out.contains("已折叠"));
    }

    #[test]
    fn mechanical_summary_keeps_role_prefixes() {
        let msgs = vec![
            Message::user_text("帮我修 bug"),
            Message::assistant_text("好的我先看代码"),
        ];
        let s = mechanical_summary(&msgs);
        assert!(s.contains("USER: 帮我修 bug"));
        assert!(s.contains("ASSISTANT: 好的"));
    }

    #[tokio::test]
    async fn compact_rewrites_active_path_but_keeps_log() {
        use crate::agent::{noop_sink, PlannedToolCall, StreamAggregator};
        use crate::api::{ModelBackend, StreamEvent, StreamRequest};
        use std::sync::Arc;

        // 假后端：摘要流直接吐固定文本
        struct FakeBackend;
        #[async_trait::async_trait]
        impl ModelBackend for FakeBackend {
            fn backend_name(&self) -> &'static str {
                "fake"
            }
            async fn stream(
                &self,
                _req: StreamRequest,
            ) -> Result<
                futures::stream::BoxStream<'static, Result<StreamEvent, crate::api::ApiError>>,
                crate::api::ApiError,
            > {
                let out: futures::stream::BoxStream<
                    'static,
                    Result<StreamEvent, crate::api::ApiError>,
                > = Box::pin(futures::stream::iter(vec![Ok(StreamEvent::TextDelta(
                    "- 用户要修 bug\n- 已完成定位".into(),
                ))]));
                Ok(out)
            }
        }

        let deps = AgentDeps::for_tests(
            "test-model",
            vec![],
            Arc::new(|_: &str| Ok(Arc::new(FakeBackend) as Arc<dyn ModelBackend>)),
        );
        let mut conv = Conversation::new();
        let root = conv.push(Message::user_text("task root"));
        for i in 0..20 {
            conv.push(Message::user_text(format!("历史消息 {i}")));
        }
        let old_log_len = conv.messages.len();
        let before_path = conv.active_messages().len();

        compact_conversation(&deps, &mut conv, &noop_sink())
            .await
            .unwrap();

        // 日志追加不删
        assert_eq!(conv.messages.len(), old_log_len + 1);
        // 活跃路径 = 摘要 + 12 条尾部
        let path = conv.active_messages();
        assert_eq!(path.len(), KEEP_RECENT + 1);
        assert!(before_path > path.len());
        // 摘要消息是路径头，synthetic + summarizes 指向被概括区间
        let head = &conv.messages[conv.messages.len() - 1];
        assert!(head.synthetic);
        assert_eq!(head.summarizes.as_ref().unwrap().0, root);
        assert_eq!(path[0].id, head.id);
        assert!(
            path[0].text().contains("已完成定位"),
            "sum_msg.text()={:?}",
            path[0].text()
        );
        // marker 可被 last_compact_marker 找到
        assert!(conv.last_compact_marker().is_some());
        // StreamAggregator 顺带回归（本模块借用其类型）
        let mut agg = StreamAggregator::new();
        agg.feed(StreamEvent::TextDelta("x".into()));
        let _: Vec<PlannedToolCall> = agg.finish().tool_calls;
        let _ = &root;
    }
}
