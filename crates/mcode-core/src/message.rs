//! 统一消息模型。
//!
//! 一份 [`Message`] 同时服务三个消费者：
//! 1. **API 请求**：[`normalize_for_api`] 折叠成协议无关的 [`ApiMessage`] 序列，
//!    由各协议后端再翻译为自家 wire format；
//! 2. **持久化**：直接 serde 到会话 JSONL（含 parent 链，支持 rewind/分叉）；
//! 3. **UI**：transcript 渲染直接读 content blocks，无需第二套类型。
//!
//! 与旧版（mcode-core 0.1）的本质差异：工具轮（ToolUse/ToolResult）留在
//! 会话主路径上，模型每轮都能看到完整历史——这是多步任务质量的根基。

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

// ---------------------------------------------------------------------------
// 基础枚举
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Assistant => "assistant",
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 回合结束原因（协议无关）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// 模型自然收尾（无工具调用）。
    EndTurn,
    /// 输出因 max_output_tokens 截断——主循环的恢复 continue 点之一。
    MaxTokens,
    /// 模型请求工具调用。
    ToolUse,
    Refusal,
    Other,
}

/// 一次请求的用量快照（各协议后端归一化后填充）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_read_input_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
}

impl Usage {
    pub fn context_tokens(&self) -> u64 {
        self.input_tokens + self.cache_read_input_tokens + self.cache_creation_input_tokens
    }
}

// ---------------------------------------------------------------------------
// 内容块
// ---------------------------------------------------------------------------

/// 协议无关的内容块。字段语义对齐 Anthropic Messages API（作为规范形），
/// 其他协议后端负责降级映射。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    /// 推理内容。`signature` 由 Anthropic 类协议原样回传；
    /// 不支持思考回传的后端直接丢弃。
    Thinking {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    /// 加密思考块（Anthropic 特有），透传用。
    RedactedThinking {
        data: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default)]
        is_error: bool,
    },
    /// base64 编码的图片。
    Image {
        media_type: String,
        data: String,
    },
}

impl ContentBlock {
    pub fn text(s: impl Into<String>) -> Self {
        ContentBlock::Text { text: s.into() }
    }

    pub fn tool_result(tool_use_id: impl Into<String>, content: impl Into<String>) -> Self {
        ContentBlock::ToolResult {
            tool_use_id: tool_use_id.into(),
            content: content.into(),
            is_error: false,
        }
    }

    pub fn tool_result_error(tool_use_id: impl Into<String>, content: impl Into<String>) -> Self {
        ContentBlock::ToolResult {
            tool_use_id: tool_use_id.into(),
            content: content.into(),
            is_error: true,
        }
    }

    /// 纯文本预览（用于估算/日志/UI 摘要行）。
    pub fn as_text(&self) -> Option<&str> {
        match self {
            ContentBlock::Text { text } => Some(text),
            ContentBlock::ToolResult { content, .. } => Some(content),
            ContentBlock::Thinking { text, .. } => Some(text),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// 消息
// ---------------------------------------------------------------------------

/// 单条会话消息。
///
/// `parent_id` 构成消息树：正常追加时父节点是当前叶；rewind 只移动叶指针，
/// 不删日志——被放弃的分支保留在文件里可回看，但不进 API 请求。
/// `sidechain` 标记子 agent 的消息：落盘可查，但永不进主循环请求。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub sidechain: bool,
    pub role: Role,
    pub content: Vec<ContentBlock>,
    /// Unix 毫秒时间戳。
    pub timestamp_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<StopReason>,
    /// 系统合成消息（压缩摘要、状态重播附件等）：进 API 请求，但 UI 上
    /// 以折叠条渲染而非普通气泡。
    #[serde(default)]
    pub synthetic: bool,
    /// 压缩摘要消息专用：被概括的历史区间 `[from_id, to_id]`（闭区间语义，
    /// 用于 UI 折叠与二次压缩时的边界识别）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summarizes: Option<(String, String)>,
}

impl Message {
    pub fn new(role: Role, content: Vec<ContentBlock>) -> Self {
        Message {
            id: new_message_id(role),
            parent_id: None,
            sidechain: false,
            role,
            content,
            timestamp_ms: chrono::Utc::now().timestamp_millis(),
            model: None,
            usage: None,
            stop_reason: None,
            synthetic: false,
            summarizes: None,
        }
    }

    pub fn user_text(text: impl Into<String>) -> Self {
        Message::new(Role::User, vec![ContentBlock::text(text)])
    }

    pub fn assistant_text(text: impl Into<String>) -> Self {
        Message::new(Role::Assistant, vec![ContentBlock::text(text)])
    }

    pub fn with_sidechain(mut self, sidechain: bool) -> Self {
        self.sidechain = sidechain;
        self
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    pub fn with_usage(mut self, usage: Usage) -> Self {
        self.usage = Some(usage);
        self
    }

    pub fn with_stop_reason(mut self, reason: StopReason) -> Self {
        self.stop_reason = Some(reason);
        self
    }

    pub fn set_parent(&mut self, parent_id: impl Into<String>) {
        self.parent_id = Some(parent_id.into());
    }

    /// 拼接全部文本类 block（Text 优先，ToolResult 用于只读展示场景）。
    pub fn text(&self) -> String {
        let mut out = String::new();
        for block in &self.content {
            if let ContentBlock::Text { text } = block {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(text);
            }
        }
        out
    }

    pub fn tool_uses(&self) -> Vec<&ContentBlock> {
        self.content
            .iter()
            .filter(|b| matches!(b, ContentBlock::ToolUse { .. }))
            .collect()
    }

    pub fn tool_results(&self) -> Vec<&ContentBlock> {
        self.content
            .iter()
            .filter(|b| matches!(b, ContentBlock::ToolResult { .. }))
            .collect()
    }

    pub fn has_tool_use(&self) -> bool {
        self.content
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolUse { .. }))
    }

    /// 供 token 估算用的可读序列化（结构化但紧凑）。
    pub fn estimate_source(&self) -> String {
        let mut out = String::with_capacity(256);
        out.push_str(self.role.as_str());
        out.push_str(":\n");
        for block in &self.content {
            match block {
                ContentBlock::Text { text }
                | ContentBlock::Thinking { text, .. }
                | ContentBlock::ToolResult { content: text, .. } => {
                    out.push_str(text);
                    out.push('\n');
                }
                ContentBlock::ToolUse { id, name, input } => {
                    out.push_str(&format!("[tool_use id={id} name={name} args={input}]"));
                    out.push('\n');
                }
                ContentBlock::Image { .. } => {
                    // 图片按固定预算计（视觉 token 粗估），不计字符。
                    out.push_str("[image ~1500 tokens]\n");
                }
                ContentBlock::RedactedThinking { .. } => {}
            }
        }
        out
    }
}

pub fn new_message_id(role: Role) -> String {
    let prefix = match role {
        Role::User => "u",
        Role::Assistant => "a",
    };
    format!("{prefix}_{}", uuid::Uuid::new_v4().simple())
}

pub fn new_tool_use_id() -> String {
    format!("toolu_{}", uuid::Uuid::new_v4().simple())
}

// ---------------------------------------------------------------------------
// 会话（消息树 + 叶指针）
// ---------------------------------------------------------------------------

/// 会话 = 追加式消息日志 + 叶指针。
///
/// - 正常对话：`push` 把新消息挂到叶下，叶前移；
/// - rewind：只把叶指回历史某条，日志原样保留（UI 可展示被放弃分支）；
/// - 子 agent：`push_sidechain` 落盘但不改变主路径。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Conversation {
    pub messages: Vec<Message>,
    #[serde(default)]
    pub leaf_id: Option<String>,
}

impl Conversation {
    pub fn new() -> Self {
        Conversation::default()
    }

    /// 从既有日志恢复（resume）：叶取参数指定值，缺省取最后一条主链消息。
    pub fn from_messages(messages: Vec<Message>, leaf_id: Option<String>) -> Self {
        let leaf_id = leaf_id.or_else(|| {
            messages
                .iter()
                .rev()
                .find(|m| !m.sidechain)
                .map(|m| m.id.clone())
        });
        Conversation { messages, leaf_id }
    }

    fn index_of(&self, id: &str) -> Option<usize> {
        self.messages.iter().rposition(|m| m.id == id)
    }

    /// 追加主链消息：自动挂父、叶前移。返回消息 id。
    pub fn push(&mut self, mut msg: Message) -> String {
        msg.sidechain = false;
        if let Some(leaf) = self.leaf_id.clone() {
            msg.set_parent(leaf);
        }
        let id = msg.id.clone();
        self.leaf_id = Some(id.clone());
        self.messages.push(msg);
        id
    }

    /// 落盘子 agent 消息：进日志（transcript 可查），不动主链叶。
    pub fn push_sidechain(&mut self, mut msg: Message) -> String {
        msg.sidechain = true;
        let id = msg.id.clone();
        self.messages.push(msg);
        id
    }

    /// rewind：叶指回目标消息。目标不存在时返回 false（不改动）。
    pub fn rewind_to(&mut self, message_id: &str) -> bool {
        if self.index_of(message_id).is_some() {
            self.leaf_id = Some(message_id.to_string());
            true
        } else {
            false
        }
    }

    /// rewind 到目标消息**之前**：叶指回目标的 parent（目标本身也移出
    /// 活跃路径，用户重新输入）。目标不存在 → false；目标是首条（无 parent
    /// 或 parent 已不在消息集）→ 叶清空，回到空会话。
    pub fn rewind_before(&mut self, message_id: &str) -> bool {
        match self.index_of(message_id) {
            Some(idx) => {
                let parent = self.messages[idx].parent_id.clone();
                self.leaf_id = parent.filter(|p| self.index_of(p).is_some());
                true
            }
            None => false,
        }
    }

    /// 当前活跃主链（叶 → 根回溯，再反转为时间序），排除 sidechain。
    pub fn active_path(&self) -> Vec<&Message> {
        let mut chain: Vec<&Message> = Vec::new();
        let mut cursor = self.leaf_id.clone();
        let mut guard = 0usize;
        while let Some(id) = cursor {
            guard += 1;
            if guard > self.messages.len() + 1 {
                break; // 防御环：parent 链损坏时截断而不是死循环
            }
            match self.index_of(&id) {
                Some(idx) => {
                    let msg = &self.messages[idx];
                    if msg.sidechain {
                        // 理论不该出现（sidechain 不入叶链），防御性跳过
                        cursor = msg.parent_id.clone();
                        continue;
                    }
                    chain.push(msg);
                    cursor = msg.parent_id.clone();
                }
                None => break,
            }
        }
        chain.reverse();
        chain
    }

    /// 活跃主链的 owned 克隆（API 请求/压缩输入用）。
    pub fn active_messages(&self) -> Vec<Message> {
        self.active_path().into_iter().cloned().collect()
    }

    /// 最后一条主链用户真实输入（不含合成消息）。
    pub fn last_user_text(&self) -> Option<String> {
        self.active_path()
            .iter()
            .rev()
            .find(|m| m.role == Role::User && !m.synthetic)
            .map(|m| m.text())
    }

    /// 全会话用量累计（含 sidechain，供 /cost）。
    pub fn total_usage(&self) -> Usage {
        let mut total = Usage::default();
        for msg in &self.messages {
            if let Some(u) = msg.usage {
                total.input_tokens += u.input_tokens;
                total.output_tokens += u.output_tokens;
                total.cache_read_input_tokens += u.cache_read_input_tokens;
                total.cache_creation_input_tokens += u.cache_creation_input_tokens;
            }
        }
        total
    }

    /// 主链上最近一次 usage（上下文水位真值锚点）。
    pub fn last_context_usage(&self) -> Option<Usage> {
        self.active_path().iter().rev().find_map(|m| m.usage)
    }

    /// 找到最近的压缩摘要消息（主链上最后一个 synthetic && summarizes 非空）。
    pub fn last_compact_marker(&self) -> Option<&Message> {
        self.active_path()
            .iter()
            .rev()
            .find(|m| m.synthetic && m.summarizes.is_some())
            .copied()
    }
}

// ---------------------------------------------------------------------------
// API 折叠：Message 树 → 协议无关消息序列
// ---------------------------------------------------------------------------

/// 发给后端的折叠形态：role + 内容块。各协议后端再翻译。
#[derive(Clone, Debug, PartialEq)]
pub enum ApiMessage {
    User {
        blocks: Vec<ContentBlock>,
    },
    Assistant {
        blocks: Vec<ContentBlock>,
        /// 折叠前的源消息 id（供 cache 断点/日志定位）。
        source_ids: Vec<String>,
    },
}

impl ApiMessage {
    pub fn role(&self) -> Role {
        match self {
            ApiMessage::User { .. } => Role::User,
            ApiMessage::Assistant { .. } => Role::Assistant,
        }
    }
}

/// 把活跃主链折叠成可发送序列。
///
/// 规则：
/// 1. 丢弃空消息（无内容块）；
/// 2. 合并相邻同角色消息（块顺序保持）；
/// 3. 每个 ToolUse 必须有紧随的 ToolResult——缺失时合成错误结果
///    （"工具调用被中断"），避免协议层 400；
/// 4. 序列必须以 User 开头——首条若是 Assistant，降级为 User 文本；
/// 5. synthetic 消息正常参与折叠（压缩摘要就是要发给模型的）。
pub fn normalize_for_api(messages: &[Message]) -> Vec<ApiMessage> {
    let mut merged: Vec<ApiMessage> = Vec::with_capacity(messages.len());

    for msg in messages {
        if msg.content.is_empty() {
            continue;
        }
        match (msg.role, merged.last_mut()) {
            // 合并相邻同角色
            (Role::User, Some(ApiMessage::User { blocks })) => {
                blocks.extend(msg.content.iter().cloned());
            }
            (Role::Assistant, Some(ApiMessage::Assistant { blocks, source_ids })) => {
                blocks.extend(msg.content.iter().cloned());
                source_ids.push(msg.id.clone());
            }
            (role, _) => merged.push(match role {
                Role::User => ApiMessage::User {
                    blocks: msg.content.clone(),
                },
                Role::Assistant => ApiMessage::Assistant {
                    blocks: msg.content.clone(),
                    source_ids: vec![msg.id.clone()],
                },
            }),
        }
    }

    // 补齐孤儿 ToolUse 的 ToolResult
    let mut result: Vec<ApiMessage> = Vec::with_capacity(merged.len());
    let mut pending_tool_uses: Vec<String> = Vec::new();
    for msg in merged {
        match &msg {
            ApiMessage::Assistant { blocks, .. } => {
                for block in blocks {
                    if let ContentBlock::ToolUse { id, .. } = block {
                        pending_tool_uses.push(id.clone());
                    }
                }
                result.push(msg);
            }
            ApiMessage::User { blocks } => {
                let mut blocks = blocks.clone();
                if !pending_tool_uses.is_empty() {
                    let answered: Vec<&str> = blocks
                        .iter()
                        .filter_map(|b| match b {
                            ContentBlock::ToolResult { tool_use_id, .. } => {
                                Some(tool_use_id.as_str())
                            }
                            _ => None,
                        })
                        .collect();
                    let orphaned: Vec<String> = pending_tool_uses
                        .iter()
                        .filter(|id| !answered.contains(&id.as_str()))
                        .cloned()
                        .collect();
                    for id in orphaned {
                        blocks.push(ContentBlock::tool_result_error(
                            id,
                            "[tool call interrupted: no result recorded]".to_string(),
                        ));
                    }
                    pending_tool_uses.clear();
                }
                result.push(ApiMessage::User { blocks });
            }
        }
    }
    // 尾部孤儿（模型发出 tool_use 后回合被截断）
    if !pending_tool_uses.is_empty() {
        let blocks = pending_tool_uses
            .into_iter()
            .map(|id| {
                ContentBlock::tool_result_error(
                    id,
                    "[tool call interrupted: no result recorded]".to_string(),
                )
            })
            .collect();
        result.push(ApiMessage::User { blocks });
    }

    // 首条必须 User
    if matches!(result.first(), Some(ApiMessage::Assistant { .. })) {
        result = demote_first_assistant(result);
    }

    result
}

/// 首条 Assistant 降级为 User：文本保留，ToolUse/Thinking 转为说明文本。
fn demote_first_assistant(messages: Vec<ApiMessage>) -> Vec<ApiMessage> {
    let mut out = messages;
    if let Some(ApiMessage::Assistant { blocks, .. }) = out.first_mut() {
        let mut user_blocks: Vec<ContentBlock> = Vec::with_capacity(blocks.len());
        for block in blocks.drain(..) {
            match block {
                ContentBlock::Text { text } => user_blocks.push(ContentBlock::Text { text }),
                ContentBlock::ToolUse { name, input, .. } => user_blocks.push(ContentBlock::text(
                    format!("[previous tool call: {name}({})]", input),
                )),
                ContentBlock::Thinking { text, .. } => {
                    user_blocks.push(ContentBlock::text(format!("[previous thinking: {text}]")))
                }
                other => user_blocks.push(other),
            }
        }
        out[0] = ApiMessage::User {
            blocks: user_blocks,
        };
    }
    out
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use ContentBlock::*;

    fn user(text: &str) -> Message {
        Message::user_text(text)
    }

    fn assistant_with_tools(calls: &[(&str, &str)]) -> Message {
        let mut msg = Message::new(Role::Assistant, vec![]);
        for (id, name) in calls {
            msg.content.push(ToolUse {
                id: id.to_string(),
                name: name.to_string(),
                input: serde_json::json!({}),
            });
        }
        msg
    }

    fn tool_results(pairs: &[(&str, &str)]) -> Message {
        let mut msg = Message::new(Role::User, vec![]);
        for (id, out) in pairs {
            msg.content.push(ToolResult {
                tool_use_id: id.to_string(),
                content: out.to_string(),
                is_error: false,
            });
        }
        msg
    }

    #[test]
    fn push_builds_parent_chain() {
        let mut conv = Conversation::new();
        let a = conv.push(user("hi"));
        let b = conv.push(Message::assistant_text("hello"));
        let path = conv.active_path();
        assert_eq!(path.len(), 2);
        assert_eq!(path[0].id, a);
        assert_eq!(path[1].id, b);
        assert_eq!(path[1].parent_id.as_deref(), Some(a.as_str()));
    }

    #[test]
    fn rewind_keeps_log_but_repaths() {
        let mut conv = Conversation::new();
        let first = conv.push(user("hi"));
        conv.push(Message::assistant_text("bad answer"));
        assert!(conv.rewind_to(&first));
        assert_eq!(conv.active_path().len(), 1);
        assert_eq!(conv.messages.len(), 2); // 日志保留
        let _ = conv.push(Message::assistant_text("better answer"));
        assert_eq!(conv.active_path().len(), 2);
        assert_eq!(conv.active_path()[1].text(), "better answer");
    }

    #[test]
    fn rewind_before_drops_target_from_active_path() {
        let mut conv = Conversation::new();
        let u1 = conv.push(user("一"));
        let _a1 = conv.push(Message::assistant_text("答一"));
        let u2 = conv.push(user("二"));
        let _a2 = conv.push(Message::assistant_text("答二"));

        // 回到 u2 之前：u2 及其后移出活跃路径，日志保留
        assert!(conv.rewind_before(&u2));
        assert_eq!(conv.active_path().len(), 2);
        assert_eq!(conv.active_path().last().unwrap().text(), "答一");

        // 新消息挂到新叶（u1→a1 分支上），旧分支仍在日志
        let _u3 = conv.push(user("三"));
        assert_eq!(conv.messages.len(), 5, "4 旧 + 1 新");
        assert_eq!(conv.active_path().len(), 3);

        // 回到首条之前 → 叶清空（空会话）
        assert!(conv.rewind_before(&u1));
        assert!(conv.active_path().is_empty());
        // 目标不存在 → 不动
        assert!(!conv.rewind_before("nope"));
        assert!(conv.active_path().is_empty());
    }

    #[test]
    fn sidechain_never_enters_active_path() {
        let mut conv = Conversation::new();
        let root = conv.push(user("task"));
        let leaf = conv.push(Message::assistant_text("done"));
        let mut sub = Message::assistant_text("sub-agent thinking");
        sub.set_parent(leaf.clone());
        conv.push_sidechain(sub);
        let path = conv.active_path();
        assert_eq!(path.len(), 2);
        assert_eq!(path.last().unwrap().id, leaf);
        assert_eq!(conv.messages.len(), 3);
        assert_eq!(path[0].id, root);
    }

    #[test]
    fn normalize_merges_adjacent_same_role() {
        let msgs = vec![user("a"), user("b"), Message::assistant_text("c")];
        let out = normalize_for_api(&msgs);
        assert_eq!(out.len(), 2);
        match &out[0] {
            ApiMessage::User { blocks } => {
                assert_eq!(blocks.len(), 2);
                assert_eq!(blocks[0], Text { text: "a".into() });
            }
            _ => panic!("expected user"),
        }
    }

    #[test]
    fn normalize_synthesizes_orphan_tool_results() {
        let mut conv = Conversation::new();
        conv.push(user("run it"));
        conv.push(assistant_with_tools(&[("t1", "bash")]));
        // 模拟回合截断：没有 tool_result
        let msgs = conv.active_messages();
        let out = normalize_for_api(&msgs);
        let last = out.last().unwrap();
        match last {
            ApiMessage::User { blocks } => match &blocks[0] {
                ToolResult {
                    tool_use_id,
                    is_error,
                    ..
                } => {
                    assert_eq!(tool_use_id, "t1");
                    assert!(is_error);
                }
                _ => panic!("expected synthesized tool result"),
            },
            _ => panic!("expected user"),
        }
    }

    #[test]
    fn normalize_keeps_paired_tool_results_and_drops_duplicates() {
        let msgs = vec![
            user("go"),
            assistant_with_tools(&[("t1", "read_file")]),
            tool_results(&[("t1", "contents")]),
            Message::assistant_text("done"),
        ];
        let out = normalize_for_api(&msgs);
        assert_eq!(out.len(), 4);
        match &out[2] {
            ApiMessage::User { blocks } => assert_eq!(blocks.len(), 1),
            _ => panic!("expected tool result user msg"),
        }
    }

    #[test]
    fn normalize_demotes_leading_assistant() {
        let msgs = vec![Message::assistant_text("hi"), user("hello")];
        let out = normalize_for_api(&msgs);
        assert_eq!(out.first().unwrap().role(), Role::User);
    }

    #[test]
    fn usage_accumulates() {
        let mut conv = Conversation::new();
        conv.push(user("x"));
        let mut a = Message::assistant_text("y");
        a.usage = Some(Usage {
            input_tokens: 100,
            output_tokens: 10,
            cache_read_input_tokens: 50,
            cache_creation_input_tokens: 0,
        });
        conv.push(a);
        let total = conv.total_usage();
        assert_eq!(total.input_tokens, 100);
        assert_eq!(total.output_tokens, 10);
        assert_eq!(total.cache_read_input_tokens, 50);
    }
}
