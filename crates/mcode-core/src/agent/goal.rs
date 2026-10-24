// MCode — AI coding agent
// Copyright (c) 2026 Materin-tech. All rights reserved.
// SPDX-License-Identifier: MIT

//! goal 外层循环：提示词 + 完成裁判（CLI 与 Desktop 共用）。
//!
//! core 的 `run_agent_turn` 是单回合引擎；goal 是外层「裁判-续跑」循环：
//! 每回合结束问一次裁判，只有显式 GOAL_COMPLETE 判词才收官，
//! 最多 [`MAX_GOAL_ROUNDS`] 轮。
//!
//! 守护栏：轮数上限防失控；裁判 fail-open（判官调用失败 → 继续干，
//! 不让一次网络抖动杀死整个 goal）；判官调用可被用户中断（cancel watch）。
//! 职责边界：单回合执行与取消在 [`crate::agent::runner`]，本模块只管「跑不跑下一轮」。

use crate::api::{StreamEvent, StreamRequest};
use crate::message::{normalize_for_api, Message};
use futures::StreamExt;

/// goal 外层循环轮数上限（对齐 CLI cli.tsx runTurn）
pub const MAX_GOAL_ROUNDS: usize = 12;

/// /goal 持续指令（对齐 CLI goal.ts buildGoalPrompt）
pub fn build_goal_prompt(goal: &str) -> String {
    [
        format!("[goal] {goal}"),
        "Continue working toward the goal. Use tools to make real progress.".into(),
        "If the goal is already fully achieved, reply with exactly GOAL_COMPLETE and no tool calls."
            .into(),
    ]
    .join("\n")
}

/// goal 完成裁判：独立单发流式调用（无工具、空历史）。
/// fail-open：裁判出错/被打断 → 返回 false 继续干（对齐 CLI goal.ts checkGoalComplete）。
pub async fn check_goal_complete(
    backend: &dyn crate::api::ModelBackend,
    model: &str,
    goal: &str,
    cancel: tokio::sync::watch::Receiver<bool>,
) -> bool {
    let system = [
        "You are a strict completion judge. Given a goal and the work done so far (summarized by the conversation), decide:",
        "- If the goal is FULLY achieved: reply with exactly GOAL_COMPLETE",
        "- Otherwise: reply with exactly GOAL_CONTINUE",
        "Reply with one word only.",
    ]
    .join("\n");
    let input = format!(
        "Goal: {goal}\nJudge whether the goal is fully achieved. Answer GOAL_COMPLETE only if the visible work demonstrably completes the goal."
    );
    let req = StreamRequest {
        model: model.to_string(),
        system: vec![system],
        messages: normalize_for_api(&[Message::user_text(input)]),
        tools: vec![],
        max_output_tokens: 1_000,
        thinking: None,
    };
    let Ok(stream) = backend.stream(req).await else {
        return false;
    };
    let mut reply = String::new();
    let mut stream = std::pin::pin!(stream);
    loop {
        let next = tokio::select! {
            _ = wait_cancel(cancel.clone()) => None,
            ev = stream.next() => ev,
        };
        match next {
            Some(Ok(StreamEvent::TextDelta(t))) => reply.push_str(&t),
            Some(Ok(StreamEvent::Done { .. })) | None => break,
            Some(Ok(_)) => {}
            Some(Err(_)) => return false,
        }
    }
    reply.to_lowercase().contains("goal_complete")
}

async fn wait_cancel(mut rx: tokio::sync::watch::Receiver<bool>) {
    loop {
        if *rx.borrow() {
            return;
        }
        if rx.changed().await.is_err() {
            // 发送端已 drop = 永不取消（对齐 runner wait_cancel 语义）
            std::future::pending::<()>().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::ModelBackend;

    struct JudgeBackend {
        reply: &'static str,
    }

    #[async_trait::async_trait]
    impl ModelBackend for JudgeBackend {
        fn backend_name(&self) -> &'static str {
            "judge"
        }

        async fn stream(
            &self,
            _req: StreamRequest,
        ) -> Result<
            futures::stream::BoxStream<'static, Result<StreamEvent, crate::api::ApiError>>,
            crate::api::ApiError,
        > {
            use futures::stream;
            let reply = self.reply;
            Ok(Box::pin(stream::iter(vec![Ok(StreamEvent::TextDelta(
                reply.into(),
            ))])))
        }
    }

    #[tokio::test]
    async fn judge_accepts_only_explicit_verdict() {
        let (_tx, rx) = tokio::sync::watch::channel(false);
        let yes = JudgeBackend {
            reply: "GOAL_COMPLETE",
        };
        assert!(check_goal_complete(&yes, "m", "ship", rx.clone()).await);
        let no = JudgeBackend {
            reply: "GOAL_CONTINUE",
        };
        assert!(!check_goal_complete(&no, "m", "ship", rx.clone()).await);
        let junk = JudgeBackend {
            reply: "I think it is done",
        };
        assert!(!check_goal_complete(&junk, "m", "ship", rx).await);
    }

    #[test]
    fn goal_prompt_shape() {
        let p = build_goal_prompt("ship it");
        assert!(p.starts_with("[goal] ship it"));
        assert!(p.contains("GOAL_COMPLETE"));
    }
}
