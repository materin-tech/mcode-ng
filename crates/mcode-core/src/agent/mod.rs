//! agent —— 回合循环、事件流、流聚合与恢复 continue 点。
//!
//! 模块布局：
//! - [`mod`]（本文件）：AgentEvent 事件流、StreamAggregator（StreamEvent →
//!   Turn 聚合）、模型上下文窗口表；
//! - [`runner`]：主循环 + 恢复状态机（retry / fallback-model /
//!   compact-and-retry / output-escalation / abort）；
//! - [`compact`]：上下文压缩（阈值门控 + LLM 结构化摘要）。

pub mod compact;
pub mod goal;
pub mod runner;

use crate::message::{StopReason, Usage};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// 事件汇：loop 把 AgentEvent 推给 UI（task 工具冒泡子 agent 事件时复用）。
pub type EventSink = Arc<dyn Fn(AgentEvent) + Send + Sync>;

/// 空事件汇（工具单测用）。
pub fn noop_sink() -> EventSink {
    Arc::new(|_| {})
}

/// UI 可见的回合事件流。`Done` 是回合收尾信号——**任何**退出路径
/// （正常/错误/打断）都必须以 Done 结束，否则 UI 的 Running 占位滞留。
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// 模型文本增量
    TextDelta(String),
    /// 思考增量（scope = 子 agent 名；None = 主会话）
    Thinking { text: String, scope: Option<String> },
    /// 开始调用工具
    ToolUseStart {
        id: String,
        name: String,
        scope: Option<String>,
    },
    /// 工具输入增量（流式参数）
    ToolInputDelta {
        id: String,
        partial_json: String,
        scope: Option<String>,
    },
    /// 工具执行完成（`display` = 结构化富卡片数据：Diff/输出/文件引用，
    /// UI 层直接渲染；`output` 是 ≤2000 字符的文本预览）
    ToolResult {
        name: String,
        success: bool,
        output: String,
        #[allow(clippy::type_complexity)]
        display: Option<crate::tools::DisplayBlock>,
        scope: Option<String>,
    },
    /// usage 记账（每次 API 往返一条）
    Usage(Usage),
    /// 瞬时错误重试中
    Retrying {
        attempt: usize,
        max: usize,
        reason: String,
    },
    /// 恢复动作（降级/压缩/扩容）——continue 点对 UI 的可见性
    Recovering { action: String },
    /// 上下文压缩完成
    Compacted { note: String },
    /// hook 非阻断警告（stderr/启动失败/超时）——只进 UI，不进模型上下文
    HookNotice { message: String },
    /// 权限确认请求（UI 弹窗，经 oneshot 回传 Decision）
    Confirm { tool: String, args_summary: String },
    /// 终态错误
    Error { message: String },
    /// 回合收尾（reply = 最终文本；空串 = 打断或错误）
    Done { reply: String },
}

// ---------------------------------------------------------------------------
// 模型上下文窗口表（token 预算与压缩门控）
// ---------------------------------------------------------------------------

/// 已知模型的上下文窗口（tokens）。未知模型回落 128k（保守值）。
///
/// 估算策略对 auto-compact 是**单向安全**的：窗口高估 → 压缩来得太晚 →
/// ContextOverflow；窗口低估 → 压缩稍早 → 只浪费一点。所以拿不准的
/// 变体一律取保守值（如 glm-5.3-flash 在部分 Coding Plan 里被降到 200k，
/// 就按 200k 算；deepseek-v4 不带 `[1m]` 后缀时默认窗口较短，按 128k 算）。
pub fn context_window_for_model(model: &str) -> usize {
    let m = model.to_lowercase();
    // `[1m]` / `-1m` 后缀变体显式请求百万窗口（DeepSeek 等的命名约定）
    if name_requests_1m(&m) {
        return 1_000_000;
    }
    // 顺序敏感：长名优先（glm-5.3-flash 在 glm-5.3 之前，glm-4.6 在 glm 之前）
    const TABLE: &[(&str, usize)] = &[
        ("claude-opus-4", 200_000),
        ("claude-sonnet-4", 200_000),
        ("claude-haiku-4", 200_000),
        ("claude-3", 200_000),
        ("claude", 200_000),
        ("gpt-5", 400_000),
        ("gpt-4.1", 1_000_000),
        ("gpt-4o", 128_000),
        ("o3", 200_000),
        ("o4", 200_000),
        ("glm-5.3-flash", 200_000),
        ("glm-5.3", 1_000_000),
        ("glm-5.2", 1_000_000),
        ("glm-5", 1_000_000),
        ("glm-4.6", 200_000),
        ("glm-4.5", 128_000),
        ("glm", 128_000),
        ("deepseek-chat", 128_000),
        ("deepseek-reasoner", 128_000),
        ("deepseek-v4.1", 1_000_000),
        ("deepseek-v4-pro", 1_000_000),
        ("deepseek-v4", 128_000),
        ("deepseek", 64_000),
        ("qwen3", 256_000),
        ("qwen", 128_000),
        ("kimi", 256_000),
        ("minimax-m1", 1_000_000),
        ("minimax", 200_000),
        ("gemini-2", 1_000_000),
        ("gemini", 128_000),
    ];
    for (prefix, window) in TABLE {
        if m.starts_with(prefix) {
            return *window;
        }
    }
    128_000
}

/// 模型名是否显式请求 1M 窗口变体：`deepseek-v4-flash [1m]`、`xx-1m`。
/// `'1'` 前是 `.` 或数字时不算（`qwen2.1moe` 是版本号小数，不是窗口标记）。
fn name_requests_1m(m: &str) -> bool {
    let b = m.as_bytes();
    for i in 0..b.len().saturating_sub(1) {
        if b[i] == b'1' && b[i + 1] == b'm' {
            let prev_is_frac = i > 0 && (b[i - 1] == b'.' || b[i - 1].is_ascii_digit());
            if !prev_is_frac {
                return true;
            }
        }
    }
    false
}

/// 自动压缩阈值：窗口的 80%。
pub fn auto_compact_threshold(model: &str) -> usize {
    context_window_for_model(model) * 4 / 5
}

// ---------------------------------------------------------------------------
// 流聚合：StreamEvent → Turn
// ---------------------------------------------------------------------------

/// 一次 API 往返聚合出的回合（工具调用已收口、参数已解析）。
#[derive(Debug, Default, Clone)]
pub struct Turn {
    /// 助手文本（部分回复保留：流中断时也带上已收到的部分）
    pub text: String,
    pub thinking: String,
    /// 思考块签名（Anthropic 协议回传思考时必需；取本回合最后一个非空签名）
    pub thinking_signature: Option<String>,
    pub tool_calls: Vec<PlannedToolCall>,
    pub usage: Option<Usage>,
    pub stop_reason: Option<StopReason>,
    /// 流被取消/错误打断（text/thinking 是部分内容）
    pub interrupted: bool,
    /// stop = MaxTokens（output-escalation 触发依据）
    pub max_tokens_hit: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannedToolCall {
    pub id: String,
    pub name: String,
    /// 解析后的参数（收口时 JSON 解析失败 → 空对象，由工具层报参数错误）
    pub input: serde_json::Value,
    /// 参数 JSON 修复不了时的原文（诊断）
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub raw_arguments: String,
}

/// 把后端 StreamEvent 流聚合成 [`Turn`]。
///
/// 聚合规则（流式协议层）：
/// - `ToolUseStart` 开新槽位；`ToolInputDelta` 按 id 追加 partial_json；
///   `ToolUseDone` 收口（覆盖 input；缺失 delta 的后端靠它兜底）；
/// - `Done` 记录 stop_reason 并终止；流自然结束没等到 Done →
///   interrupted = true（部分内容仍保留）。
#[derive(Default)]
pub struct StreamAggregator {
    turn: Turn,
    /// tool_use id → (slot index, accumulated partial json)
    pending: std::collections::HashMap<String, usize>,
}

impl StreamAggregator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn feed(&mut self, ev: crate::api::StreamEvent) {
        use crate::api::StreamEvent;
        match ev {
            StreamEvent::TextDelta(t) => self.turn.text.push_str(&t),
            StreamEvent::ThinkingDelta { text } => self.turn.thinking.push_str(&text),
            StreamEvent::ThinkingDone { signature } => {
                if signature.is_some() {
                    self.turn.thinking_signature = signature;
                }
            }
            StreamEvent::ToolUseStart { id, name } => {
                self.pending.insert(id.clone(), self.turn.tool_calls.len());
                self.turn.tool_calls.push(PlannedToolCall {
                    id,
                    name,
                    input: serde_json::Value::Null, // 收口时填
                    raw_arguments: String::new(),
                });
            }
            StreamEvent::ToolInputDelta { id, partial_json } => {
                if let Some(&idx) = self.pending.get(&id) {
                    self.turn.tool_calls[idx]
                        .raw_arguments
                        .push_str(&partial_json);
                }
            }
            StreamEvent::ToolUseDone { id, input } => {
                if let Some(&idx) = self.pending.get(&id) {
                    self.turn.tool_calls[idx].input = input;
                }
            }
            StreamEvent::Usage(u) => self.turn.usage = Some(u),
            StreamEvent::Done { stop_reason } => {
                self.turn.stop_reason = Some(stop_reason);
                self.finalize_inputs();
            }
        }
    }

    /// 收口：没等到 ToolUseDone 的槽位用累积的 partial_json 兜底解析；
    /// stop_reason 缺失（流提前断）→ interrupted。
    pub fn finish(mut self) -> Turn {
        self.finalize_inputs();
        if self.turn.stop_reason.is_none() {
            self.turn.interrupted = true;
        }
        if self.turn.stop_reason == Some(StopReason::MaxTokens) {
            self.turn.max_tokens_hit = true;
        }
        self.turn
    }

    fn finalize_inputs(&mut self) {
        for call in &mut self.turn.tool_calls {
            if call.input.is_null() {
                call.input = if call.raw_arguments.trim().is_empty() {
                    serde_json::json!({})
                } else {
                    serde_json::from_str(&call.raw_arguments)
                        .unwrap_or(serde_json::Value::Object(Default::default()))
                };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::StreamEvent;

    fn agg(events: Vec<StreamEvent>) -> Turn {
        let mut a = StreamAggregator::new();
        for e in events {
            a.feed(e);
        }
        a.finish()
    }

    #[test]
    fn aggregates_text_thinking_and_usage() {
        let t = agg(vec![
            StreamEvent::ThinkingDelta { text: "想".into() },
            StreamEvent::TextDelta("你".into()),
            StreamEvent::TextDelta("好".into()),
            StreamEvent::Usage(Usage {
                input_tokens: 10,
                output_tokens: 5,
                ..Default::default()
            }),
            StreamEvent::Done {
                stop_reason: StopReason::EndTurn,
            },
        ]);
        assert_eq!(t.text, "你好");
        assert_eq!(t.thinking, "想");
        assert_eq!(t.usage.unwrap().output_tokens, 5);
        assert_eq!(t.stop_reason, Some(StopReason::EndTurn));
        assert!(!t.interrupted);
        assert!(t.tool_calls.is_empty());
    }

    #[test]
    fn aggregates_split_tool_call_with_delta_and_done() {
        let t = agg(vec![
            StreamEvent::ToolUseStart {
                id: "c1".into(),
                name: "bash".into(),
            },
            StreamEvent::ToolInputDelta {
                id: "c1".into(),
                partial_json: "{\"command\":".into(),
            },
            StreamEvent::ToolInputDelta {
                id: "c1".into(),
                partial_json: "\"ls\"}".into(),
            },
            StreamEvent::ToolUseDone {
                id: "c1".into(),
                input: serde_json::json!({"command": "ls"}),
            },
            StreamEvent::Done {
                stop_reason: StopReason::ToolUse,
            },
        ]);
        assert_eq!(t.stop_reason, Some(StopReason::ToolUse));
        assert_eq!(t.tool_calls.len(), 1);
        assert_eq!(t.tool_calls[0].name, "bash");
        assert_eq!(t.tool_calls[0].input["command"], "ls");
    }

    #[test]
    fn missing_done_input_falls_back_to_accumulated_json() {
        // 后端不发 ToolUseDone → finish() 用 partial_json 兜底解析
        let t = agg(vec![
            StreamEvent::ToolUseStart {
                id: "c1".into(),
                name: "grep".into(),
            },
            StreamEvent::ToolInputDelta {
                id: "c1".into(),
                partial_json: "{\"pattern\": \"x\"}".into(),
            },
            StreamEvent::Done {
                stop_reason: StopReason::ToolUse,
            },
        ]);
        assert_eq!(t.tool_calls[0].input["pattern"], "x");
    }

    #[test]
    fn unparseable_arguments_become_empty_object() {
        let t = agg(vec![
            StreamEvent::ToolUseStart {
                id: "c1".into(),
                name: "bash".into(),
            },
            StreamEvent::ToolInputDelta {
                id: "c1".into(),
                partial_json: "{broken".into(),
            },
            StreamEvent::Done {
                stop_reason: StopReason::ToolUse,
            },
        ]);
        assert_eq!(t.tool_calls[0].input, serde_json::json!({}));
        assert_eq!(t.tool_calls[0].raw_arguments, "{broken");
    }

    #[test]
    fn missing_done_marks_interrupted_but_keeps_partial_text() {
        let t = agg(vec![StreamEvent::TextDelta("部分回复".into())]);
        assert!(t.interrupted);
        assert_eq!(t.text, "部分回复");
    }

    #[test]
    fn max_tokens_hit_flagged() {
        let t = agg(vec![StreamEvent::Done {
            stop_reason: StopReason::MaxTokens,
        }]);
        assert!(t.max_tokens_hit);
    }

    #[test]
    fn multiple_tool_calls_tracked_independently() {
        let t = agg(vec![
            StreamEvent::ToolUseStart {
                id: "a".into(),
                name: "grep".into(),
            },
            StreamEvent::ToolUseStart {
                id: "b".into(),
                name: "glob".into(),
            },
            StreamEvent::ToolInputDelta {
                id: "b".into(),
                partial_json: "{\"pattern\": \"*.rs\"}".into(),
            },
            StreamEvent::ToolUseDone {
                id: "a".into(),
                input: serde_json::json!({"pattern": "y"}),
            },
            StreamEvent::ToolUseDone {
                id: "b".into(),
                input: serde_json::json!({"pattern": "*.rs"}),
            },
            StreamEvent::Done {
                stop_reason: StopReason::ToolUse,
            },
        ]);
        assert_eq!(t.tool_calls.len(), 2);
        assert_eq!(t.tool_calls[0].id, "a");
        assert_eq!(t.tool_calls[0].input["pattern"], "y");
        assert_eq!(t.tool_calls[1].id, "b");
        assert_eq!(t.tool_calls[1].input["pattern"], "*.rs");
    }

    #[test]
    fn context_windows_and_threshold() {
        assert_eq!(context_window_for_model("claude-opus-4-5"), 200_000);
        assert_eq!(context_window_for_model("glm-4.6"), 200_000);
        assert_eq!(context_window_for_model("glm-4.5-air"), 128_000);
        assert_eq!(context_window_for_model("deepseek-chat"), 128_000);
        assert_eq!(context_window_for_model("mystery-model"), 128_000);
        assert_eq!(auto_compact_threshold("glm-4.6"), 160_000);
    }

    #[test]
    fn context_windows_2026_flagships() {
        // GLM-5.x 旗舰 1M（官方文档）；flash 变体保守按 200k
        assert_eq!(context_window_for_model("glm-5.3"), 1_000_000);
        assert_eq!(context_window_for_model("glm-5.3-flash"), 200_000);
        assert_eq!(context_window_for_model("glm-5.2"), 1_000_000);
        // DeepSeek v4.1 原生 1M；v4 无后缀默认较短（128k 保守）
        assert_eq!(context_window_for_model("deepseek-v4.1-flash"), 1_000_000);
        assert_eq!(context_window_for_model("deepseek-v4-pro"), 1_000_000);
        assert_eq!(context_window_for_model("deepseek-v4-flash"), 128_000);
        assert_eq!(context_window_for_model("minimax-m1"), 1_000_000);
    }

    #[test]
    fn one_m_suffix_requests_million_window() {
        assert_eq!(
            context_window_for_model("deepseek-v4-flash [1m]"),
            1_000_000
        );
        assert_eq!(context_window_for_model("some-model-1m"), 1_000_000);
        // 版本号小数不算：qwen2.1moe 走 qwen 前缀
        assert_eq!(context_window_for_model("qwen2.1moe"), 128_000);
        assert_eq!(context_window_for_model("gpt-4.1-mini"), 1_000_000); // gpt-4.1 前缀命中
    }
}
