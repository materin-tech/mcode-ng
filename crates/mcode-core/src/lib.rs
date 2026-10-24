//! MCode 新一代核心库。
//!
//! 顶层设计（对齐 Claude Code 的骨架语义，保留 mcode 双协议优势）：
//! - [`message`]：统一消息模型。会话内存、API 请求、持久化 JSONL 三方共用，
//!   消灭旧版「富 Message 仅供 UI / 文本对仅供模型」的双真相源问题。
//! - [`token`]：CJK 感知的 token 估算 + usage 真值滚动校准。
//! - [`api`]：三协议后端（Anthropic / Responses / Chat Completions）、
//!   结构化错误分类、重试与降级策略。
//! - [`agent`]：带恢复路径的 agent 状态机（见 runner 的 continue 点设计）。
//! - [`tools`]：富契约工具系统（并发性/结果落盘/先读后写/权限钩子）。
//! - [`permission`]：deny→ask→allow 有序规则引擎。
//! - [`hooks`]：8 事件 + 双协议（exit-code 文本 / JSON 结构化）。
//! - [`mcp`]：stdio + HTTP 传输、失效即重连、readOnlyHint 映射。
//! - [`sessions`]：parent 链 JSONL、全量 resume、文件快照 rewind、保留期清理。
//! - [`subagents`]：同步 + 异步子 agent，递归深度硬护栏。
//! - [`config`]：兼容既有 `~/.mcode` 配置格式。

pub mod agent;
pub mod api;
pub mod commands;
pub mod config;
pub mod hooks;
pub mod mcp;
pub mod message;
pub mod permission;
pub mod plugins;
pub mod sessions;
pub mod skills;
pub mod subagents;
pub mod token;
pub mod tools;
pub mod util;
