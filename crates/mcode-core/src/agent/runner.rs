//! agent 主循环 + CC 式恢复 continue 点。
//!
//! 职责：把「流式请求 → 聚合 → 工具执行 → 结果回填」循环起来，并在每个
//! 可恢复的失败点上选择与 Claude Code 一致的恢复动作：
//!
//! | 错误类别                          | 恢复动作                                   |
//! |-----------------------------------|--------------------------------------------|
//! | RateLimited/Overloaded/5xx/网络   | 指数退避重试（≤[`MAX_STREAM_RETRIES`]）     |
//! | 重试耗尽                          | 切换 `fallback_models` 链，重置退避计数     |
//! | ContextOverflow                   | reactive compact 后重放（每回合一次）       |
//! | stop=MaxTokens                    | 输出扩容翻倍后续写（每回合一次，封顶 64k）  |
//! | Auth/Billing                      | 优先切换 fallback，无备选则终止             |
//! | InvalidRequest/Protocol           | 立即终止（重试/换模型都无意义）             |
//!
//! 不变式：**任何退出路径都必须以 [`AgentEvent::Done`] 收尾**——UI 的
//! Running 占位、会话持久化、状态机复位都依赖它。

use super::{compact, AgentEvent, EventSink, PlannedToolCall, StreamAggregator, Turn};
use crate::api::{ApiError, ModelBackend, StreamEvent, StreamRequest, ThinkingConfig, ToolSpec};
use crate::config::HookEvent;
use crate::hooks::{HookOutcome, HookRunner};
use crate::message::{ContentBlock, Conversation, Message, Role};
use crate::permission::{Decision, PermCheck, PermissionManager, Verdict};
use crate::tools::{ToolContext, ToolDefinition, ToolRegistry, ToolResult};
use futures::StreamExt;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, watch};

/// 流式请求最大原样重试次数（之后切换 fallback 模型）。
pub const MAX_STREAM_RETRIES: usize = 5;

/// 只读工具并行度（CC 同量级）。
pub const PARALLEL_READ_TOOLS: usize = 10;

/// 默认单回合输出上限（tokens）。
pub const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 32_000;

/// 输出扩容封顶。
pub const MAX_OUTPUT_TOKENS_CAP: u32 = 64_000;

/// 事件里的工具输出预览长度（完整输出在 transcript / 工具落盘里）。
const EVENT_OUTPUT_PREVIEW: usize = 2_000;

// ---------------------------------------------------------------------------
// 依赖注入
// ---------------------------------------------------------------------------

/// 按模型名构造后端（fallback 切换时用不同 provider 重建）。
pub type MakeBackend = Arc<dyn Fn(&str) -> anyhow::Result<Arc<dyn ModelBackend>> + Send + Sync>;

/// 权限确认请求：UI 收到后弹窗，经 `respond` 回传 [`Decision`]。
pub struct ConfirmRequest {
    pub tool: String,
    pub args_summary: String,
    pub respond: oneshot::Sender<Decision>,
}

/// 确认通道发送端（None = headless：需要确认时直接跳过执行）。
pub type ConfirmSender = mpsc::Sender<ConfirmRequest>;

/// token 估算器（压缩门控与 proactive 阈值的数据源）。
pub type TokenEstimator = Box<dyn Fn(&[Message]) -> usize + Send + Sync>;

/// 回合循环的全部外部依赖。
pub struct AgentDeps {
    /// 主模型（RunOutcome.final_model 反映 fallback 后的实际值）。
    pub model: String,
    pub fallback_models: Vec<String>,
    pub make_backend: MakeBackend,
    pub registry: Arc<ToolRegistry>,
    pub perms: Arc<tokio::sync::Mutex<PermissionManager>>,
    pub confirm: Option<ConfirmSender>,
    /// 工具执行上下文（cwd / cancel / 共享状态 / 注入点）。
    pub tool_ctx: ToolContext,
    /// 取消信号发送端（deps 持有保活；调用方克隆后随时触发取消）。
    pub cancel_tx: watch::Sender<bool>,
    /// token 估算器（压缩门控用；默认 [`crate::token::raw_estimate_messages`]，
    /// 接入 TokenCalibrator 后替换）。
    pub token_estimator: TokenEstimator,
    /// 生命周期 hooks（None = 未配置；PreToolUse/PostToolUse/Stop 三个
    /// continue 点的数据源）。
    pub hooks: Option<Arc<HookRunner>>,
    /// 会话落盘句柄（None = 不持久化）。assistant/工具结果消息与文件快照
    /// 由 runner 追加；用户输入消息由调用方在 push 时自行记录。
    pub session: Option<Arc<crate::sessions::SessionHandle>>,
    /// 回合事件总线：runner 在回合入口写入自己的 sink，`task` 工具读取后
    /// 把子代理事件（scope 化）冒泡上来。子代理的 deps 是新构造的独立总线。
    pub task_bus: crate::subagents::TaskEventBus,
}

impl AgentDeps {
    pub fn current_model(&self) -> String {
        self.model.clone()
    }

    /// 消息落盘（未配置会话则忽略；写失败已在句柄内部降级为告警）。
    pub fn record_message(&self, m: &Message) {
        if let Some(s) = &self.session {
            s.append_message(m);
        }
    }

    /// 测试构造：空 registry、Passon 模式、无确认通道、raw 估算器。
    pub fn for_tests(
        model: impl Into<String>,
        fallback_models: Vec<String>,
        make_backend: MakeBackend,
    ) -> Self {
        Self::with_registry(
            model,
            fallback_models,
            make_backend,
            ToolRegistry::new_empty(),
        )
    }

    pub fn with_registry(
        model: impl Into<String>,
        fallback_models: Vec<String>,
        make_backend: MakeBackend,
        registry: ToolRegistry,
    ) -> Self {
        let (tool_ctx, cancel_tx) = ToolContext::for_tests(std::env::temp_dir());
        AgentDeps {
            model: model.into(),
            fallback_models,
            make_backend,
            registry: Arc::new(registry),
            perms: Arc::new(tokio::sync::Mutex::new(PermissionManager::new(
                crate::permission::Mode::Passon,
                vec![],
            ))),
            confirm: None,
            tool_ctx,
            cancel_tx,
            token_estimator: Box::new(crate::token::raw_estimate_messages),
            hooks: None,
            session: None,
            task_bus: Default::default(),
        }
    }
}

// ---------------------------------------------------------------------------
// 回合参数与结果
// ---------------------------------------------------------------------------

/// 一次 `run_agent_turn` 的参数。
pub struct RunOpts {
    /// 系统提示分段（最后一段可能被后端打 cache 断点）。
    pub system: Vec<String>,
    pub tools: Vec<ToolSpec>,
    /// 0 = 不限。
    pub max_turns: usize,
    pub max_output_tokens: u32,
    pub thinking: Option<ThinkingConfig>,
    /// 覆盖 `deps.model`（交互层 `/model` 运行中切换；None = 用 deps.model）。
    pub model: Option<String>,
}

impl RunOpts {
    pub fn new(system: Vec<String>) -> Self {
        RunOpts {
            system,
            tools: vec![],
            max_turns: 0,
            max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
            thinking: None,
            model: None,
        }
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    pub fn with_tools(mut self, tools: Vec<ToolSpec>) -> Self {
        self.tools = tools;
        self
    }

    pub fn with_max_turns(mut self, max_turns: usize) -> Self {
        self.max_turns = max_turns;
        self
    }

    pub fn with_max_output_tokens(mut self, n: u32) -> Self {
        self.max_output_tokens = n;
        self
    }

    pub fn with_thinking(mut self, t: Option<ThinkingConfig>) -> Self {
        self.thinking = t;
        self
    }
}

/// 回合结果（会话层持久化 / UI 状态机复位的数据源）。
#[derive(Debug, Clone)]
pub struct RunOutcome {
    /// 最终文本回复（打断/错误时为空串或部分文本）。
    pub reply: String,
    pub turns: usize,
    pub interrupted: bool,
    pub error: Option<String>,
    /// fallback 切换后的实际模型。
    pub final_model: String,
    pub compacted: bool,
}

impl RunOutcome {
    fn empty(final_model: String) -> Self {
        RunOutcome {
            reply: String::new(),
            turns: 0,
            interrupted: false,
            error: None,
            final_model,
            compacted: false,
        }
    }
}

// ---------------------------------------------------------------------------
// 恢复 continue 点分类
// ---------------------------------------------------------------------------

enum Recovery {
    /// 原样重试（指数退避）。
    Retry,
    /// 切换 fallback 模型并重置退避。
    SwitchModel { model: String },
    /// reactive compact 后重放。
    CompactRetry,
    /// 终止回合。
    Abort(String),
}

fn classify_recovery(
    e: &ApiError,
    attempt: usize,
    fallbacks: &[String],
    already_compacted: bool,
) -> Recovery {
    use crate::api::ApiErrorKind as K;
    match &e.kind {
        K::ContextOverflow => {
            if already_compacted {
                Recovery::Abort("上下文压缩后仍然溢出——请缩小任务或 /clear".into())
            } else {
                Recovery::CompactRetry
            }
        }
        K::RateLimited { .. }
        | K::Overloaded
        | K::Server { .. }
        | K::Network
        | K::Timeout
        | K::StreamAborted => {
            if attempt + 1 < MAX_STREAM_RETRIES {
                Recovery::Retry
            } else if let Some(next) = fallbacks.first() {
                Recovery::SwitchModel {
                    model: next.clone(),
                }
            } else {
                Recovery::Abort(format!("重试 {MAX_STREAM_RETRIES} 次仍失败"))
            }
        }
        K::Auth | K::Billing => {
            if let Some(next) = fallbacks.first() {
                Recovery::SwitchModel {
                    model: next.clone(),
                }
            } else {
                Recovery::Abort("认证/计费错误（无 fallback 模型可用）".into())
            }
        }
        K::InvalidRequest { detail } => Recovery::Abort(format!("请求不合法: {detail}")),
        K::Protocol { detail } => Recovery::Abort(format!("协议不兼容: {detail}")),
        K::Cancelled => Recovery::Abort("已取消".into()),
    }
}

// ---------------------------------------------------------------------------
// 主循环
// ---------------------------------------------------------------------------

/// 跑一个完整回合（可含多轮工具调用），直到模型自然收尾、打断或终态错误。
///
/// 事件契约：正常结束 `Done{reply}`；打断 `Done{""或部分}`；错误先
/// `Error` 再 `Done{""}`——永不漏发 Done。
pub async fn run_agent_turn(
    deps: &AgentDeps,
    conv: &mut Conversation,
    opts: &RunOpts,
    emit: &EventSink,
) -> RunOutcome {
    let mut model = opts.model.clone().unwrap_or_else(|| deps.model.clone());
    let mut fallbacks: Vec<String> = deps.fallback_models.clone();
    let mut max_output = opts.max_output_tokens.max(1_000);
    let mut attempt = 0usize;
    let mut turns = 0usize;
    let mut compacted = false;
    let mut escalated = false;
    // Stop hook 阻断后已强迫继续过（载荷里告知 hook，防无限互搏）。
    let mut stop_hook_active = false;
    let mut outcome = RunOutcome::empty(model.clone());

    // 本回合的文件快照检查点 = 调用方刚推入的 user 消息：
    // rewind 到它应撤销整个回合（含全部工具轮）的文件改动。
    let checkpoint_id = conv
        .active_messages()
        .iter()
        .rev()
        .find(|m| m.role == Role::User && !m.sidechain)
        .map(|m| m.id.clone())
        .unwrap_or_default();

    // 把本回合 sink 挂上事件总线：task 工具执行期间读它冒泡子代理事件。
    // 不清槽——后台子代理完成后仍可经残留 sink 送达进度。
    deps.task_bus.set(emit.clone());

    loop {
        if *deps.tool_ctx.cancel.borrow() {
            outcome.interrupted = true;
            emit(AgentEvent::Done {
                reply: String::new(),
            });
            return outcome;
        }
        if opts.max_turns > 0 && turns >= opts.max_turns {
            let msg = format!("达到最大轮数 {}，回合结束", opts.max_turns);
            outcome.error = Some(msg.clone());
            emit(AgentEvent::Error { message: msg });
            emit(AgentEvent::Done {
                reply: String::new(),
            });
            return outcome;
        }

        // proactive compact：接近窗口上限先压缩再请求
        if !compacted && compact::should_compact(deps, conv) {
            emit(AgentEvent::Recovering {
                action: "上下文接近窗口上限，自动压缩".into(),
            });
            // 消息太少等门控误报：压缩失败不致命，继续请求
            if compact::compact_conversation(deps, conv, emit)
                .await
                .is_ok()
            {
                compacted = true;
            }
        }

        let req = StreamRequest {
            model: model.clone(),
            system: opts.system.clone(),
            messages: crate::message::normalize_for_api(&conv.active_messages()),
            tools: opts.tools.clone(),
            max_output_tokens: max_output,
            thinking: opts.thinking,
        };
        let backend = match (deps.make_backend)(&model) {
            Ok(b) => b,
            Err(e) => {
                let msg = format!("构造后端失败: {e}");
                outcome.error = Some(msg.clone());
                emit(AgentEvent::Error { message: msg });
                emit(AgentEvent::Done {
                    reply: String::new(),
                });
                return outcome;
            }
        };

        match stream_once(&*backend, req, emit, &deps.tool_ctx.cancel).await {
            Ok(turn) => {
                attempt = 0;
                if let Some(u) = turn.usage {
                    emit(AgentEvent::Usage(u));
                }
                let assistant_msg = assistant_message(&turn, &model);
                deps.record_message(&assistant_msg);
                conv.push(assistant_msg);
                turns += 1;
                outcome.turns = turns;
                outcome.final_model = model.clone();
                outcome.compacted = compacted;

                // continue 点：输出截断 → 扩容续写（截断的工具参数不可执行）
                if turn.max_tokens_hit {
                    if escalated || max_output >= MAX_OUTPUT_TOKENS_CAP {
                        outcome.reply = turn.text.clone();
                        emit(AgentEvent::Done {
                            reply: outcome.reply.clone(),
                        });
                        return outcome;
                    }
                    max_output = max_output.saturating_mul(2).min(MAX_OUTPUT_TOKENS_CAP);
                    escalated = true;
                    emit(AgentEvent::Recovering {
                        action: format!("输出被截断，max_output_tokens 提升到 {max_output} 续写"),
                    });
                    continue;
                }

                // continue 点：流被掐断（没等到 Done）→ 部分回复直接收尾
                if turn.interrupted {
                    outcome.interrupted = true;
                    outcome.reply = turn.text.clone();
                    emit(AgentEvent::Done {
                        reply: outcome.reply.clone(),
                    });
                    return outcome;
                }

                // 自然收尾
                if turn.tool_calls.is_empty() {
                    // continue 点：Stop hook 阻断 → reason 作为新 user 消息注入，
                    // 强迫模型继续（对齐 CC Stop hook 的 continue 语义）
                    if let Some(hooks) = deps
                        .hooks
                        .as_ref()
                        .filter(|h| h.has_hooks_for(HookEvent::Stop))
                    {
                        let o = hooks.run_stop(&deps.tool_ctx.cwd, stop_hook_active).await;
                        emit_hook_notices(emit, &o);
                        if o.blocked {
                            let reason = o.feedback.unwrap_or_default();
                            emit(AgentEvent::Recovering {
                                action: format!("Stop hook 要求继续：{reason}"),
                            });
                            let msg = Message::user_text(reason);
                            deps.record_message(&msg);
                            conv.push(msg);
                            stop_hook_active = true;
                            continue;
                        }
                    }
                    outcome.reply = turn.text.clone();
                    emit(AgentEvent::Done {
                        reply: outcome.reply.clone(),
                    });
                    return outcome;
                }

                // 工具轮：执行 + 结果回填为 user 消息
                let results =
                    execute_tool_calls(deps, &turn.tool_calls, emit, &checkpoint_id).await;
                let blocks = results
                    .into_iter()
                    .map(|(call, res)| {
                        if res.success {
                            ContentBlock::tool_result(&call.id, res.output)
                        } else {
                            ContentBlock::tool_result_error(&call.id, res.output)
                        }
                    })
                    .collect();
                let results_msg = Message::new(Role::User, blocks);
                deps.record_message(&results_msg);
                conv.push(results_msg);
            }
            Err(e) => {
                if e.is_cancelled() {
                    outcome.interrupted = true;
                    emit(AgentEvent::Done {
                        reply: String::new(),
                    });
                    return outcome;
                }
                match classify_recovery(&e, attempt, &fallbacks, compacted) {
                    Recovery::Retry => {
                        attempt += 1;
                        let delay = e.suggested_retry_delay(attempt as u32);
                        emit(AgentEvent::Retrying {
                            attempt,
                            max: MAX_STREAM_RETRIES,
                            reason: e.message.clone(),
                        });
                        tokio::select! {
                            _ = tokio::time::sleep(delay) => {}
                            _ = wait_cancel(deps.tool_ctx.cancel.clone()) => {
                                outcome.interrupted = true;
                                emit(AgentEvent::Done { reply: String::new() });
                                return outcome;
                            }
                        }
                    }
                    Recovery::SwitchModel { model: next } => {
                        fallbacks.retain(|m| m != &next);
                        emit(AgentEvent::Recovering {
                            action: format!(
                                "切换 fallback 模型: {model} → {next}（{}）",
                                e.message
                            ),
                        });
                        model = next;
                        attempt = 0;
                        outcome.final_model = model.clone();
                    }
                    Recovery::CompactRetry => {
                        emit(AgentEvent::Recovering {
                            action: "上下文溢出，压缩后重放".into(),
                        });
                        match compact::compact_conversation(deps, conv, emit).await {
                            Ok(_) => {
                                compacted = true;
                                outcome.compacted = true;
                            }
                            Err(ce) => {
                                let msg = format!("上下文溢出且压缩失败: {ce}");
                                outcome.error = Some(msg.clone());
                                emit(AgentEvent::Error { message: msg });
                                emit(AgentEvent::Done {
                                    reply: String::new(),
                                });
                                return outcome;
                            }
                        }
                    }
                    Recovery::Abort(reason) => {
                        let msg = format!("{reason}：{}", e.message);
                        outcome.error = Some(msg.clone());
                        emit(AgentEvent::Error { message: msg });
                        emit(AgentEvent::Done {
                            reply: String::new(),
                        });
                        return outcome;
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 流消费
// ---------------------------------------------------------------------------

/// 消费一次流：透传 UI 事件 + 聚合 Turn。取消即时生效（Err(Cancelled)）。
async fn stream_once(
    backend: &dyn ModelBackend,
    req: StreamRequest,
    emit: &EventSink,
    cancel: &watch::Receiver<bool>,
) -> Result<Turn, ApiError> {
    let mut stream = backend.stream(req).await?;
    let mut agg = StreamAggregator::new();
    let cancel_fut = wait_cancel(cancel.clone());
    tokio::pin!(cancel_fut);
    loop {
        tokio::select! {
            _ = &mut cancel_fut => return Err(ApiError::cancelled()),
            ev = stream.next() => {
                match ev {
                    Some(Ok(ev)) => {
                        passthrough(emit, &ev);
                        agg.feed(ev);
                    }
                    Some(Err(e)) => return Err(e),
                    None => return Ok(agg.finish()),
                }
            }
        }
    }
}

/// 流事件 → UI 事件的直通映射。
fn passthrough(emit: &EventSink, ev: &StreamEvent) {
    match ev {
        StreamEvent::TextDelta(t) => emit(AgentEvent::TextDelta(t.clone())),
        StreamEvent::ThinkingDelta { text } => emit(AgentEvent::Thinking {
            text: text.clone(),
            scope: None,
        }),
        StreamEvent::ToolUseStart { id, name } => emit(AgentEvent::ToolUseStart {
            id: id.clone(),
            name: name.clone(),
            scope: None,
        }),
        StreamEvent::ToolInputDelta { id, partial_json } => emit(AgentEvent::ToolInputDelta {
            id: id.clone(),
            partial_json: partial_json.clone(),
            scope: None,
        }),
        _ => {}
    }
}

/// 等待取消信号；sender 被弃（通道死亡）→ 永不触发。
async fn wait_cancel(mut rx: watch::Receiver<bool>) {
    loop {
        if rx.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
        if *rx.borrow() {
            return;
        }
    }
}

/// 聚合好的 Turn → 会话消息（Thinking + Text + ToolUse，带 usage/stop/model）。
fn assistant_message(turn: &Turn, model: &str) -> Message {
    let mut blocks = Vec::new();
    if !turn.thinking.is_empty() {
        blocks.push(ContentBlock::Thinking {
            text: turn.thinking.clone(),
            signature: turn.thinking_signature.clone(),
        });
    }
    if !turn.text.is_empty() {
        blocks.push(ContentBlock::text(turn.text.clone()));
    }
    for c in &turn.tool_calls {
        blocks.push(ContentBlock::ToolUse {
            id: c.id.clone(),
            name: c.name.clone(),
            input: c.input.clone(),
        });
    }
    let mut msg = Message::new(Role::Assistant, blocks).with_model(model);
    msg.stop_reason = turn.stop_reason;
    msg.usage = turn.usage;
    msg
}

// ---------------------------------------------------------------------------
// 工具执行（权限门控 + 只读并行）
// ---------------------------------------------------------------------------

/// 执行一批工具调用：连续只读段并行（≤[`PARALLEL_READ_TOOLS`]），其余串行。
/// 返回与入参同序的 (call, result)。
async fn execute_tool_calls(
    deps: &AgentDeps,
    calls: &[PlannedToolCall],
    emit: &EventSink,
    checkpoint_id: &str,
) -> Vec<(PlannedToolCall, ToolResult)> {
    let mut out: Vec<(usize, PlannedToolCall, ToolResult)> = Vec::with_capacity(calls.len());
    let mut i = 0;
    while i < calls.len() {
        if is_read_only_call(deps, &calls[i]) {
            let mut j = i;
            while j < calls.len() && is_read_only_call(deps, &calls[j]) {
                j += 1;
            }
            let batch = &calls[i..j];
            // Box::pin 固化 future 类型：裸借用型闭包 future 在深层 async
            // 调用链（task 工具 → 子代理）下会触发 rustc 的
            // "FnOnce not general enough" 推断失败，装箱后免疫。
            type RunOneFut<'a> = std::pin::Pin<
                Box<
                    dyn std::future::Future<Output = (usize, PlannedToolCall, ToolResult)>
                        + Send
                        + 'a,
                >,
            >;
            let mut futs: Vec<RunOneFut<'_>> = Vec::with_capacity(batch.len());
            for (k, c) in batch.iter().enumerate() {
                futs.push(Box::pin(run_one_tool(deps, i + k, c, emit, checkpoint_id)));
            }
            let results: Vec<_> = futures::stream::iter(futs)
                .buffer_unordered(PARALLEL_READ_TOOLS)
                .collect()
                .await;
            out.extend(results);
            i = j;
        } else {
            let r = run_one_tool(deps, i, &calls[i], emit, checkpoint_id).await;
            out.push(r);
            i += 1;
        }
    }
    out.sort_by_key(|(idx, _, _)| *idx);
    out.into_iter().map(|(_, call, res)| (call, res)).collect()
}

fn is_read_only_call(deps: &AgentDeps, call: &PlannedToolCall) -> bool {
    deps.registry
        .get(&call.name)
        .map(|t| t.def().is_read_only)
        .unwrap_or(false)
}

/// 单个工具调用：未知工具/参数校验/权限门控/执行/钳制/事件。
async fn run_one_tool(
    deps: &AgentDeps,
    idx: usize,
    call: &PlannedToolCall,
    emit: &EventSink,
    checkpoint_id: &str,
) -> (usize, PlannedToolCall, ToolResult) {
    let finish = |idx: usize, res: ToolResult| {
        emit(AgentEvent::ToolResult {
            name: call.name.clone(),
            success: res.success,
            output: preview_text(&res.output),
            scope: None,
        });
        (idx, call.clone(), res)
    };

    let Some(tool) = deps.registry.get(&call.name) else {
        return finish(idx, ToolResult::err(format!("未知工具: {}", call.name)));
    };
    let def: ToolDefinition = tool.def();
    if let Err(e) = crate::tools::validate_input(&def, &call.input) {
        return finish(idx, ToolResult::err(format!("参数校验失败: {e}")));
    }

    // continue 点：PreToolUse hook（权限判定**之前**）。deny → 直接拒绝；
    // allow → 跳过权限询问；ask → 强制走确认；updatedInput → 改写执行参数
    // （会话消息里保留模型原始参数，执行与权限检查用改写后的）。
    let mut effective = call.input.clone();
    let mut hook_skip_perm = false;
    let mut hook_force_ask = false;
    if let Some(hooks) = deps
        .hooks
        .as_ref()
        .filter(|h| h.has_hooks_for(HookEvent::PreToolUse))
    {
        let o = hooks
            .run_pre_tool_use(&deps.tool_ctx.cwd, &call.name, &call.input)
            .await;
        emit_hook_notices(emit, &o);
        if o.blocked {
            let reason = o.feedback.unwrap_or_default();
            return finish(idx, ToolResult::err(format!("hook 阻断：{reason}")));
        }
        if let Some(updated) = o.updated_input {
            if let Err(e) = crate::tools::validate_input(&def, &updated) {
                return finish(idx, ToolResult::err(format!("hook 改写参数校验失败: {e}")));
            }
            effective = updated;
        }
        hook_skip_perm = o.allow_skip_permission;
        hook_force_ask = o.force_ask;
    }

    // 权限门控（deny 优先；Confirm 走确认通道）
    let cmd = effective
        .get("command")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let target = effective
        .get("path")
        .or_else(|| effective.get("file_path"))
        .or_else(|| effective.get("target"))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let check = PermCheck {
        tool: &call.name,
        is_read_only: def.is_read_only,
        command: if call.name == "bash" && !cmd.is_empty() {
            Some(cmd.as_str())
        } else {
            None
        },
        target: if !def.is_read_only && !target.is_empty() {
            Some(target.as_str())
        } else {
            None
        },
    };
    let verdict = if hook_skip_perm {
        Verdict::Allow // hook 已批准：跳过权限询问（CC permissionDecision=allow 语义）
    } else if hook_force_ask {
        Verdict::Confirm // hook 要求强制确认（permissionDecision=ask 语义）
    } else {
        deps.perms.lock().await.evaluate(&check)
    };

    let executed = match verdict {
        Verdict::Allow => {
            Some(run_tool(tool.as_ref(), effective.clone(), deps, checkpoint_id).await)
        }
        Verdict::Deny { reason } => Some(ToolResult::err(format!("权限拒绝：{reason}"))),
        Verdict::Confirm => request_confirm(deps, call, emit, checkpoint_id).await,
    };

    let res = match executed {
        Some(r) => r,
        // headless 无确认通道：跳过执行（调用方决定是否报错给用户）
        None => ToolResult::err("需要用户确认，但当前无确认通道——已跳过执行"),
    };
    let res = ToolResult {
        output: {
            let guard = deps.tool_ctx.shared.lock().expect("工具状态锁");
            crate::tools::clamp_output(&guard, res.output)
        },
        ..res
    };

    // continue 点：PostToolUse hook——block 把工具结果改写为错误回喂
    if let Some(hooks) = deps
        .hooks
        .as_ref()
        .filter(|h| h.has_hooks_for(HookEvent::PostToolUse))
    {
        let o = hooks
            .run_post_tool_use(&deps.tool_ctx.cwd, &call.name, &effective, &res.output)
            .await;
        emit_hook_notices(emit, &o);
        if o.blocked {
            let reason = o.feedback.unwrap_or_default();
            return finish(idx, ToolResult::err(format!("hook 阻断：{reason}")));
        }
    }
    finish(idx, res)
}

async fn run_tool(
    tool: &dyn crate::tools::Tool,
    input: serde_json::Value,
    deps: &AgentDeps,
    checkpoint_id: &str,
) -> ToolResult {
    capture_file_snapshot(deps, checkpoint_id, tool, &input);
    match tool.execute(input, &deps.tool_ctx).await {
        Ok(r) => r,
        Err(e) => ToolResult::err(format!("工具执行失败: {e}")),
    }
}

/// 写前快照：非只读工具且带 path 型参数时，把目标文件旧内容存进会话
/// （rewind 恢复数据源）。无会话/只读工具/无路径参数（如 bash）→ 跳过。
fn capture_file_snapshot(
    deps: &AgentDeps,
    checkpoint_id: &str,
    tool: &dyn crate::tools::Tool,
    input: &serde_json::Value,
) {
    if checkpoint_id.is_empty() {
        return;
    }
    if tool.def().is_read_only {
        return;
    }
    let Some(session) = deps.session.as_ref() else {
        return;
    };
    let Some(target) = input
        .get("path")
        .or_else(|| input.get("file_path"))
        .or_else(|| input.get("target"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    else {
        return;
    };
    let p = std::path::PathBuf::from(target);
    let p = if p.is_absolute() {
        p
    } else {
        deps.tool_ctx.cwd.join(p)
    };
    let prior_content = std::fs::read_to_string(&p).ok();
    session.append_snapshot(&crate::sessions::FileSnapshot {
        checkpoint_id: checkpoint_id.to_string(),
        path: p,
        prior_content,
    });
}

/// hook 非阻断警告 → UI（阻断的 feedback 已随错误结果进模型上下文）。
fn emit_hook_notices(emit: &EventSink, o: &HookOutcome) {
    for w in &o.warnings {
        emit(AgentEvent::HookNotice { message: w.clone() });
    }
}

/// Confirm 裁决：None = 用户取消/通道异常（不执行）；Some = 确认后的执行结果或拒绝。
async fn request_confirm(
    deps: &AgentDeps,
    call: &PlannedToolCall,
    emit: &EventSink,
    checkpoint_id: &str,
) -> Option<ToolResult> {
    let tx = deps.confirm.as_ref()?;
    let args_summary = summarize_args(&call.input);
    emit(AgentEvent::Confirm {
        tool: call.name.clone(),
        args_summary: args_summary.clone(),
    });
    let (rtx, rrx) = oneshot::channel();
    if tx
        .send(ConfirmRequest {
            tool: call.name.clone(),
            args_summary,
            respond: rtx,
        })
        .await
        .is_err()
    {
        return Some(ToolResult::err("确认通道已关闭"));
    }
    tokio::select! {
        _ = wait_cancel(deps.tool_ctx.cancel.clone()) => None,
        resp = rrx => match resp {
            Ok(decision) => {
                deps.perms.lock().await.record(&call.name, &decision);
                match decision {
                    Decision::Deny => Some(ToolResult::err("用户拒绝执行")),
                    Decision::Allow | Decision::AllowAlways => {
                        let tool = deps.registry.get(&call.name)?;
                        Some(run_tool(tool.as_ref(), call.input.clone(), deps, checkpoint_id).await)
                    }
                }
            }
            Err(_) => Some(ToolResult::err("确认通道中断")),
        }
    }
}

fn summarize_args(input: &serde_json::Value) -> String {
    let s = serde_json::to_string(input).unwrap_or_default();
    preview_text(&s)
}

fn preview_text(s: &str) -> String {
    if s.chars().count() <= EVENT_OUTPUT_PREVIEW {
        s.to_string()
    } else {
        let head: String = s.chars().take(EVENT_OUTPUT_PREVIEW).collect();
        format!("{head}…[预览截断]")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::ApiErrorKind;
    use crate::message::StopReason;
    use serde_json::json;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    // ---------------- 脚本化后端 ----------------

    type ScriptQueue = Arc<std::sync::Mutex<VecDeque<Result<Vec<StreamEvent>, ApiError>>>>;

    struct ScriptBackend {
        script: ScriptQueue,
        requests: Arc<std::sync::Mutex<Vec<StreamRequest>>>,
        streams_started: Arc<AtomicUsize>,
    }

    impl ScriptBackend {
        fn new(script: ScriptQueue, requests: Arc<std::sync::Mutex<Vec<StreamRequest>>>) -> Self {
            ScriptBackend {
                script,
                requests,
                streams_started: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    #[async_trait::async_trait]
    impl ModelBackend for ScriptBackend {
        fn backend_name(&self) -> &'static str {
            "script"
        }
        async fn stream(
            &self,
            req: StreamRequest,
        ) -> Result<futures::stream::BoxStream<'static, Result<StreamEvent, ApiError>>, ApiError>
        {
            self.streams_started.fetch_add(1, Ordering::SeqCst);
            self.requests.lock().unwrap().push(req);
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

    struct TestHarness {
        script: ScriptQueue,
        requests: Arc<std::sync::Mutex<Vec<StreamRequest>>>,
        events: Arc<std::sync::Mutex<Vec<AgentEvent>>>,
        deps: AgentDeps,
    }

    impl TestHarness {
        fn new(model: &str, fallbacks: Vec<String>) -> Self {
            Self::with_registry(model, fallbacks, ToolRegistry::new_empty())
        }

        fn with_registry(model: &str, fallbacks: Vec<String>, registry: ToolRegistry) -> Self {
            let script = Arc::new(std::sync::Mutex::new(VecDeque::new()));
            let requests = Arc::new(std::sync::Mutex::new(vec![]));
            let events = Arc::new(std::sync::Mutex::new(vec![]));
            let deps = AgentDeps::with_registry(
                model,
                fallbacks,
                {
                    let script = script.clone();
                    let requests = requests.clone();
                    Arc::new(move |_: &str| {
                        Ok(
                            Arc::new(ScriptBackend::new(script.clone(), requests.clone()))
                                as Arc<dyn ModelBackend>,
                        )
                    })
                },
                registry,
            );
            TestHarness {
                script,
                requests,
                events,
                deps,
            }
        }

        fn push(&self, item: Result<Vec<StreamEvent>, ApiError>) {
            self.script.lock().unwrap().push_back(item);
        }

        fn emitted(&self) -> Vec<AgentEvent> {
            self.events.lock().unwrap().clone()
        }

        fn sink(&self) -> EventSink {
            let events = self.events.clone();
            Arc::new(move |e| events.lock().unwrap().push(e))
        }

        fn reqs(&self) -> Vec<StreamRequest> {
            self.requests.lock().unwrap().clone()
        }
    }

    fn text_turn(text: &str) -> Result<Vec<StreamEvent>, ApiError> {
        Ok(vec![
            StreamEvent::TextDelta(text.to_string()),
            StreamEvent::Done {
                stop_reason: StopReason::EndTurn,
            },
        ])
    }

    fn tool_turn(
        id: &str,
        name: &str,
        args: serde_json::Value,
    ) -> Result<Vec<StreamEvent>, ApiError> {
        Ok(vec![
            StreamEvent::ToolUseStart {
                id: id.into(),
                name: name.into(),
            },
            StreamEvent::ToolInputDelta {
                id: id.into(),
                partial_json: serde_json::to_string(&args).unwrap(),
            },
            StreamEvent::Done {
                stop_reason: StopReason::ToolUse,
            },
        ])
    }

    struct EchoTool {
        counter: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl crate::tools::Tool for EchoTool {
        fn def(&self) -> ToolDefinition {
            ToolDefinition {
                name: "echo".into(),
                description: "回声测试工具".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {"text": {"type": "string"}},
                    "required": ["text"]
                }),
                is_read_only: false,
            }
        }
        async fn execute(
            &self,
            input: serde_json::Value,
            _ctx: &ToolContext,
        ) -> anyhow::Result<ToolResult> {
            self.counter.fetch_add(1, Ordering::SeqCst);
            Ok(ToolResult::ok(format!(
                "echo: {}",
                input["text"].as_str().unwrap_or("")
            )))
        }
    }

    fn echo_registry() -> (ToolRegistry, Arc<AtomicUsize>) {
        let counter = Arc::new(AtomicUsize::new(0));
        let reg = ToolRegistry::new_empty();
        reg.register(Arc::new(EchoTool {
            counter: counter.clone(),
        }));
        (reg, counter)
    }

    /// 写文件测试工具：非只读 + path 参数（写前快照捕获路径）。
    struct TestWriteTool;

    #[async_trait::async_trait]
    impl crate::tools::Tool for TestWriteTool {
        fn def(&self) -> ToolDefinition {
            ToolDefinition {
                name: "test_write".into(),
                description: "测试写文件".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "content": {"type": "string"}
                    },
                    "required": ["path", "content"]
                }),
                is_read_only: false,
            }
        }
        async fn execute(
            &self,
            input: serde_json::Value,
            _ctx: &ToolContext,
        ) -> anyhow::Result<ToolResult> {
            let p = input["path"].as_str().unwrap_or_default();
            std::fs::write(p, input["content"].as_str().unwrap_or_default())?;
            Ok(ToolResult::ok("written"))
        }
    }

    fn write_registry() -> ToolRegistry {
        let reg = ToolRegistry::new_empty();
        reg.register(Arc::new(TestWriteTool));
        reg
    }

    /// 唯一临时目录（并发测试互不干扰）。
    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "mcode-runner-sess-{tag}-{}-{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    // ---------------- 会话落盘 ----------------

    #[tokio::test]
    async fn session_records_messages_and_write_snapshots() {
        let dir = tmp_dir("snapshot");
        let target = dir.join("a.txt");
        std::fs::write(&target, "v0").unwrap();

        let base = tmp_dir("snapshot-base");
        let store = crate::sessions::SessionStore::with_base(base.clone());
        let (handle, meta) = store.start(&dir, None).unwrap();

        let mut h = TestHarness::with_registry("m", vec![], write_registry());
        h.deps.session = Some(Arc::new(handle));

        h.push(tool_turn(
            "t1",
            "test_write",
            json!({"path": target.display().to_string(), "content": "v1"}),
        ));
        h.push(text_turn("改好了"));
        let mut conv = Conversation::new();
        // checkpoint = 本回合的 user 消息（调用方 push 时落盘）
        let user = Message::user_text("帮我改文件");
        conv.push(user.clone());
        h.deps.record_message(&user);

        let opts = RunOpts::new(vec![]).with_tools(
            h.deps
                .registry
                .definitions()
                .iter()
                .map(|d| d.to_spec())
                .collect(),
        );
        let outcome = run_agent_turn(&h.deps, &mut conv, &opts, &h.sink()).await;
        assert_eq!(outcome.error, None, "{}", outcome.reply);

        // 工具真实生效
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "v1");

        // JSONL：user(checkpoint) → assistant(tooluse) → 快照 → user(result) → assistant
        let lines =
            crate::sessions::read_lines(&store.dir().join(format!("{}.jsonl", meta.session_id)))
                .unwrap();
        let msgs: Vec<_> = lines
            .iter()
            .filter_map(|l| match l {
                crate::sessions::Line::Message(m) => Some(m.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(msgs.len(), 4, "user+assistant+result+assistant: {msgs:?}");
        assert_eq!(msgs[0].text(), "帮我改文件");
        assert!(!msgs[1].tool_uses().is_empty());
        assert!(!msgs[2].tool_results().is_empty());
        assert_eq!(msgs[3].text(), "改好了");

        let snaps: Vec<_> = lines
            .iter()
            .filter_map(|l| match l {
                crate::sessions::Line::Snapshot(s) => Some(s.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(snaps.len(), 1, "写前快照恰好一条");
        assert_eq!(snaps[0].prior_content.as_deref(), Some("v0"));
        assert_eq!(snaps[0].path, target);
        assert_eq!(
            snaps[0].checkpoint_id, msgs[0].id,
            "挂在本回合 checkpoint 上"
        );

        // rewind 端到端：checkpoint → 快照恢复 → 文件回到 v0
        let rewind_snaps = crate::sessions::snapshots_after(&lines, &msgs[0].id);
        assert_eq!(rewind_snaps.len(), 1);
        assert_eq!(crate::sessions::apply_snapshots(&rewind_snaps), 1);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "v0");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn session_without_file_tools_has_no_snapshots() {
        let base = tmp_dir("nosnap-base");
        let store = crate::sessions::SessionStore::with_base(base.clone());
        let (handle, meta) = store.start(std::path::Path::new("/p"), None).unwrap();

        let mut h = TestHarness::new("m", vec![]);
        h.deps.session = Some(Arc::new(handle));
        h.push(text_turn("纯文本回复"));
        let mut conv = Conversation::new();
        let user = Message::user_text("你好");
        conv.push(user.clone());
        h.deps.record_message(&user);
        let outcome = run_agent_turn(&h.deps, &mut conv, &RunOpts::new(vec![]), &h.sink()).await;
        assert_eq!(outcome.error, None);

        let raw = std::fs::read_to_string(store.dir().join(format!("{}.jsonl", meta.session_id)))
            .unwrap();
        assert!(!raw.contains("\"snapshot\""), "纯文本回合不产生快照: {raw}");
        assert!(raw.contains("纯文本回复"));
        let _ = std::fs::remove_dir_all(&base);
    }

    // ---------------- 用例 ----------------

    #[tokio::test]
    async fn text_only_turn_emits_done_with_reply() {
        let h = TestHarness::new("main-model", vec![]);
        h.push(text_turn("答案在此"));
        let mut conv = Conversation::new();
        conv.push(Message::user_text("问题"));
        let outcome = run_agent_turn(&h.deps, &mut conv, &RunOpts::new(vec![]), &h.sink()).await;

        assert_eq!(outcome.reply, "答案在此");
        assert_eq!(outcome.turns, 1);
        assert!(!outcome.interrupted);
        assert_eq!(outcome.error, None);
        assert_eq!(outcome.final_model, "main-model");

        let events = h.emitted();
        assert!(
            matches!(events.last(), Some(AgentEvent::Done { reply }) if reply == "答案在此"),
            "最后事件必须是 Done：{:?}",
            events.last()
        );
        // 助手消息入库
        assert_eq!(conv.active_messages().len(), 2);
    }

    #[tokio::test]
    async fn tool_roundtrip_feeds_result_back() {
        let (reg, counter) = echo_registry();
        let h = TestHarness::with_registry("m", vec![], reg);
        h.push(tool_turn("t1", "echo", json!({"text": "hi"})));
        h.push(text_turn("done"));
        let mut conv = Conversation::new();
        conv.push(Message::user_text("调用一下"));
        let opts = RunOpts::new(vec![]).with_tools(
            h.deps
                .registry
                .definitions()
                .iter()
                .map(|d| d.to_spec())
                .collect(),
        );
        let outcome = run_agent_turn(&h.deps, &mut conv, &opts, &h.sink()).await;

        assert_eq!(outcome.reply, "done");
        assert_eq!(counter.load(Ordering::SeqCst), 1);
        // 两次 API 往返；第二次请求带 tool_result
        let reqs = h.reqs();
        assert_eq!(reqs.len(), 2);
        let last = reqs.last().unwrap();
        let has_result = last.messages.iter().any(|m| {
            matches!(
                m,
                crate::message::ApiMessage::User { blocks }
                    if blocks.iter().any(|b| matches!(b, ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "t1"))
            )
        });
        assert!(has_result, "第二轮请求必须包含 t1 的 tool_result");
        // 会话：user → assistant(tooluse) → user(toolresult) → assistant(text)
        assert_eq!(conv.active_messages().len(), 4);
        // 事件含 ToolUseStart 与 ToolResult
        let events = h.emitted();
        assert!(events
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolUseStart { name, .. } if name == "echo")));
        assert!(events
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolResult { success: true, .. })));
    }

    #[tokio::test]
    async fn transient_error_retries_then_succeeds() {
        let h = TestHarness::new("m", vec![]);
        h.push(Err(ApiError::new(
            ApiErrorKind::Server { status: 500 },
            "boom",
        )));
        h.push(text_turn("恢复了"));
        let mut conv = Conversation::new();
        conv.push(Message::user_text("hi"));
        let outcome = run_agent_turn(&h.deps, &mut conv, &RunOpts::new(vec![]), &h.sink()).await;

        assert_eq!(outcome.reply, "恢复了");
        assert!(h.emitted().iter().any(|e| matches!(
            e,
            AgentEvent::Retrying {
                attempt: 1,
                max: MAX_STREAM_RETRIES,
                ..
            }
        )));
    }

    #[tokio::test]
    async fn overflow_triggers_reactive_compact_then_replays() {
        let h = TestHarness::new("m", vec![]);
        h.push(Err(ApiError::new(
            ApiErrorKind::ContextOverflow,
            "too long",
        )));
        // 第二次 stream 调用 = 摘要请求；第三次 = 重放主请求
        h.push(Ok(vec![
            StreamEvent::TextDelta("- 摘要内容".into()),
            StreamEvent::Done {
                stop_reason: StopReason::EndTurn,
            },
        ]));
        h.push(text_turn("压缩后完成"));
        let mut conv = Conversation::new();
        conv.push(Message::user_text("root"));
        for i in 0..20 {
            conv.push(Message::user_text(format!("历史 {i}")));
        }
        let outcome = run_agent_turn(&h.deps, &mut conv, &RunOpts::new(vec![]), &h.sink()).await;

        assert_eq!(outcome.reply, "压缩后完成");
        assert!(outcome.compacted);
        let path = conv.active_messages();
        assert!(path[0].synthetic, "路径头必须是压缩摘要");
        // 摘要 + 12 条尾部 + 压缩后新产出的助手回复
        assert!(path.len() <= compact::KEEP_RECENT + 2);
        assert!(path.len() > compact::KEEP_RECENT + 1);
        assert!(h
            .emitted()
            .iter()
            .any(|e| matches!(e, AgentEvent::Compacted { .. })));
    }

    #[tokio::test]
    async fn persistent_errors_fall_back_then_abort() {
        let h = TestHarness::new("main", vec!["fb-model".into()]);
        // 主模型 5 次重试耗尽 → 切 fb；fb 再耗尽 → Abort（无更多 fallback）
        for _ in 0..(MAX_STREAM_RETRIES + 1) * 2 {
            h.push(Err(ApiError::new(ApiErrorKind::Overloaded, "529")));
        }
        let mut conv = Conversation::new();
        conv.push(Message::user_text("hi"));
        let outcome = run_agent_turn(&h.deps, &mut conv, &RunOpts::new(vec![]), &h.sink()).await;

        assert!(outcome.error.is_some(), "必须以终态错误收场");
        assert_eq!(outcome.final_model, "fb-model", "至少切过一次 fallback");
        let events = h.emitted();
        assert!(events.iter().any(
            |e| matches!(e, AgentEvent::Recovering { action } if action.contains("fb-model"))
        ));
        assert!(matches!(events.last(), Some(AgentEvent::Done { .. })));
    }

    #[tokio::test]
    async fn cancel_mid_stream_interrupts_with_done() {
        let h = TestHarness::new("m", vec![]);
        // 长流：不发 Done，靠 cancel 掐断
        h.push(Ok(vec![StreamEvent::TextDelta("部分".into())]));
        let mut conv = Conversation::new();
        conv.push(Message::user_text("hi"));
        // stream_once 一进循环就会看到取消位
        h.deps.cancel_tx.send(true).unwrap();
        let outcome = run_agent_turn(&h.deps, &mut conv, &RunOpts::new(vec![]), &h.sink()).await;

        assert!(outcome.interrupted);
        assert!(matches!(h.emitted().last(), Some(AgentEvent::Done { .. })));
    }

    #[tokio::test]
    async fn manual_mode_without_confirm_channel_skips_tool() {
        let (reg, counter) = echo_registry();
        let h = TestHarness::with_registry("m", vec![], reg);
        {
            let mut perms = h.deps.perms.lock().await;
            perms.set_mode(crate::permission::Mode::Manual);
        }
        h.push(tool_turn("t1", "echo", json!({"text": "x"})));
        h.push(text_turn("跳过后的收尾"));
        let mut conv = Conversation::new();
        conv.push(Message::user_text("hi"));
        let outcome = run_agent_turn(&h.deps, &mut conv, &RunOpts::new(vec![]), &h.sink()).await;

        assert_eq!(outcome.reply, "跳过后的收尾");
        assert_eq!(counter.load(Ordering::SeqCst), 0, "无确认通道不得执行");
        assert!(h
            .emitted()
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolResult { success: false, .. })));
    }

    #[tokio::test]
    async fn confirm_channel_allow_executes_tool() {
        let (reg, counter) = echo_registry();
        let mut h = TestHarness::with_registry("m", vec![], reg);
        {
            let mut perms = h.deps.perms.lock().await;
            perms.set_mode(crate::permission::Mode::Manual);
        }
        let (ctx_tx, mut ctx_rx) = mpsc::channel::<ConfirmRequest>(4);
        // 应答协程：收到确认请求 → AllowAlways
        tokio::spawn(async move {
            while let Some(req) = ctx_rx.recv().await {
                let _ = req.respond.send(Decision::AllowAlways);
            }
        });
        h.deps.confirm = Some(ctx_tx);

        h.push(tool_turn("t1", "echo", json!({"text": "y"})));
        h.push(text_turn("ok"));
        let mut conv = Conversation::new();
        conv.push(Message::user_text("hi"));
        let outcome = run_agent_turn(&h.deps, &mut conv, &RunOpts::new(vec![]), &h.sink()).await;

        assert_eq!(outcome.reply, "ok");
        assert!(counter.load(Ordering::SeqCst) >= 1, "确认后必须执行");
        // always-allow 已沉淀：第二次同工具调用不再确认（模式仍 manual）
        let perms = h.deps.perms.lock().await;
        let verdict = perms.evaluate(&PermCheck {
            tool: "echo",
            is_read_only: false,
            command: None,
            target: None,
        });
        assert_eq!(verdict, Verdict::Allow);
        // Confirm 事件发过一次
        assert!(h
            .emitted()
            .iter()
            .any(|e| matches!(e, AgentEvent::Confirm { tool, .. } if tool == "echo")));
    }

    #[tokio::test]
    async fn max_tokens_escalates_output_cap_once() {
        let h = TestHarness::new("m", vec![]);
        h.push(Ok(vec![
            StreamEvent::TextDelta("截断了".into()),
            StreamEvent::Done {
                stop_reason: StopReason::MaxTokens,
            },
        ]));
        h.push(text_turn("续写完整"));
        let mut conv = Conversation::new();
        conv.push(Message::user_text("hi"));
        let opts = RunOpts::new(vec![]).with_max_output_tokens(4_000);
        let outcome = run_agent_turn(&h.deps, &mut conv, &opts, &h.sink()).await;

        assert_eq!(outcome.reply, "续写完整");
        assert!(h
            .emitted()
            .iter()
            .any(|e| matches!(e, AgentEvent::Recovering { action } if action.contains("8192") || action.contains("8000"))));
        let reqs = h.reqs();
        assert_eq!(reqs.len(), 2);
        assert_eq!(reqs[1].max_output_tokens, 8_000, "扩容后翻倍");
    }

    #[tokio::test]
    async fn max_turns_budget_stops_loop() {
        let h = TestHarness::new("m", vec![]);
        h.push(tool_turn("t1", "loop-tool", json!({})));
        h.push(tool_turn("t2", "loop-tool", json!({})));
        struct Noop;
        #[async_trait::async_trait]
        impl crate::tools::Tool for Noop {
            fn def(&self) -> ToolDefinition {
                ToolDefinition {
                    name: "loop-tool".into(),
                    description: "noop".into(),
                    parameters: json!({"type":"object","properties":{}}),
                    is_read_only: false,
                }
            }
            async fn execute(
                &self,
                _: serde_json::Value,
                _: &ToolContext,
            ) -> anyhow::Result<ToolResult> {
                Ok(ToolResult::ok("ok"))
            }
        }
        h.deps.registry.register(Arc::new(Noop));
        let mut conv = Conversation::new();
        conv.push(Message::user_text("hi"));
        let opts = RunOpts::new(vec![]).with_max_turns(2);
        let outcome = run_agent_turn(&h.deps, &mut conv, &opts, &h.sink()).await;

        assert!(outcome.error.unwrap_or_default().contains("最大轮数"));
        assert_eq!(outcome.turns, 2);
        assert!(matches!(h.emitted().last(), Some(AgentEvent::Done { .. })));
    }

    #[tokio::test]
    async fn invalid_request_aborts_without_retry() {
        let h = TestHarness::new("m", vec![]);
        h.push(Err(ApiError::new(
            ApiErrorKind::InvalidRequest {
                detail: "bad schema".into(),
            },
            "400",
        )));
        let mut conv = Conversation::new();
        conv.push(Message::user_text("hi"));
        let outcome = run_agent_turn(&h.deps, &mut conv, &RunOpts::new(vec![]), &h.sink()).await;

        assert!(outcome.error.unwrap_or_default().contains("请求不合法"));
        // 只请求过一次：不重试
        assert_eq!(h.reqs().len(), 1);
        assert!(matches!(h.emitted().last(), Some(AgentEvent::Done { .. })));
    }

    #[tokio::test]
    async fn parallel_read_only_batch_preserves_order() {
        // 三个只读工具并发执行，结果顺序与调用顺序一致（tool_use_id 对齐）
        struct ROTool {
            name: String,
            delay_ms: u64,
        }
        #[async_trait::async_trait]
        impl crate::tools::Tool for ROTool {
            fn def(&self) -> ToolDefinition {
                ToolDefinition {
                    name: self.name.clone(),
                    description: "ro".into(),
                    parameters: json!({"type":"object","properties":{}}),
                    is_read_only: true,
                }
            }
            async fn execute(
                &self,
                _: serde_json::Value,
                _: &ToolContext,
            ) -> anyhow::Result<ToolResult> {
                tokio::time::sleep(Duration::from_millis(self.delay_ms)).await;
                Ok(ToolResult::ok(self.name.clone()))
            }
        }
        let reg = ToolRegistry::new_empty();
        reg.register(Arc::new(ROTool {
            name: "slow".into(),
            delay_ms: 60,
        }));
        reg.register(Arc::new(ROTool {
            name: "fast".into(),
            delay_ms: 5,
        }));
        let h = TestHarness::with_registry("m", vec![], reg);
        // 两个只读工具同轮：slow 在前、fast 在后（执行乱序完成，结果必须按调用序回填）
        h.push(Ok(vec![
            StreamEvent::ToolUseStart {
                id: "a".into(),
                name: "slow".into(),
            },
            StreamEvent::ToolUseStart {
                id: "b".into(),
                name: "fast".into(),
            },
            StreamEvent::Done {
                stop_reason: StopReason::ToolUse,
            },
        ]));
        h.push(text_turn("all done"));
        let mut conv = Conversation::new();
        conv.push(Message::user_text("hi"));
        let outcome = run_agent_turn(&h.deps, &mut conv, &RunOpts::new(vec![]), &h.sink()).await;
        assert_eq!(outcome.reply, "all done");
        // 工具结果消息里的顺序必须 a(slow) 在前
        let msgs = conv.active_messages();
        let results = msgs
            .iter()
            .find(|m| m.role == Role::User && m.tool_results().len() == 2)
            .expect("工具结果消息存在");
        let ids: Vec<&str> = results
            .tool_results()
            .into_iter()
            .filter_map(|b| match b {
                ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(ids, vec!["a", "b"]);
    }

    // ---------------- hooks 集成（PreToolUse / PostToolUse / Stop）----------------

    use crate::config::HookConfig;

    fn hook_cfg(event: HookEvent, command: &str) -> HookConfig {
        HookConfig {
            event,
            matcher: "*".into(),
            command: command.into(),
            timeout_secs: None,
        }
    }

    fn deny_cmd(reason: &str) -> String {
        if cfg!(windows) {
            format!("echo {reason} 1>&2 & exit /b 2")
        } else {
            format!("echo {reason} >&2; exit 2")
        }
    }

    fn allow_json_cmd() -> String {
        if cfg!(windows) {
            "echo {\"hookSpecificOutput\":{\"permissionDecision\":\"allow\"}}".into()
        } else {
            r#"printf '%s' '{"hookSpecificOutput":{"permissionDecision":"allow"}}'"#.into()
        }
    }

    fn allow_with_updated_input_cmd() -> String {
        if cfg!(windows) {
            "echo {\"hookSpecificOutput\":{\"permissionDecision\":\"allow\",\"updatedInput\":{\"text\":\"rewritten\"}}}".into()
        } else {
            r#"printf '%s' '{"hookSpecificOutput":{"permissionDecision":"allow","updatedInput":{"text":"rewritten"}}}'"#.into()
        }
    }

    /// 找到会话里唯一一条工具结果消息的首个 ToolResult 块。
    fn first_tool_result(msgs: &[Message]) -> (&str, bool, &str) {
        let m = msgs
            .iter()
            .find(|m| m.tool_results().len() == 1)
            .expect("工具结果消息存在");
        match &m.content[0] {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
                ..
            } => (tool_use_id, *is_error, content),
            _ => panic!("首块必须是 ToolResult"),
        }
    }

    #[tokio::test]
    async fn pre_tool_use_deny_blocks_tool_execution() {
        let (reg, counter) = echo_registry();
        let mut h = TestHarness::with_registry("m", vec![], reg);
        // Windows cmd 在非 UTF-8 代码页（runner 是 CP437）下 stdout 会把非 ASCII
        // 打成 '?'——断言载荷换 ASCII；阻断语义与载荷语言无关，中文往返
        // 完整性在 unix 侧照样覆盖。
        let reason = if cfg!(windows) {
            "deny echo"
        } else {
            "禁用 echo"
        };
        h.deps.hooks = Some(Arc::new(HookRunner::new(
            vec![hook_cfg(HookEvent::PreToolUse, &deny_cmd(reason))],
            "sess",
        )));
        h.push(tool_turn("t1", "echo", json!({"text": "hi"})));
        h.push(text_turn("done"));
        let mut conv = Conversation::new();
        conv.push(Message::user_text("hi"));
        let outcome = run_agent_turn(&h.deps, &mut conv, &RunOpts::new(vec![]), &h.sink()).await;

        assert_eq!(outcome.reply, "done");
        assert_eq!(
            counter.load(Ordering::SeqCst),
            0,
            "hook deny 后工具不得执行"
        );
        let msgs = conv.active_messages();
        let (_, is_error, content) = first_tool_result(&msgs);
        assert!(is_error, "hook 阻断以错误结果回喂");
        assert!(content.contains(reason), "reason 回喂模型: {content}");
    }

    #[tokio::test]
    async fn pre_tool_use_allow_skips_permission_confirm() {
        // EchoTool 非只读 → Passon 模式本应走 Confirm；无确认通道时会被跳过。
        // hook permissionDecision=allow → 跳过权限询问直接执行（CC 语义）。
        let (reg, counter) = echo_registry();
        let mut h = TestHarness::with_registry("m", vec![], reg);
        assert!(h.deps.confirm.is_none());
        h.deps.hooks = Some(Arc::new(HookRunner::new(
            vec![hook_cfg(HookEvent::PreToolUse, &allow_json_cmd())],
            "sess",
        )));
        h.push(tool_turn("t1", "echo", json!({"text": "hi"})));
        h.push(text_turn("done"));
        let mut conv = Conversation::new();
        conv.push(Message::user_text("hi"));
        let outcome = run_agent_turn(&h.deps, &mut conv, &RunOpts::new(vec![]), &h.sink()).await;

        assert_eq!(outcome.reply, "done");
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "hook allow 应跳过权限询问直接执行"
        );
        let msgs = conv.active_messages();
        let (_, is_error, _) = first_tool_result(&msgs);
        assert!(!is_error);
    }

    #[tokio::test]
    async fn pre_tool_use_updated_input_rewrites_args() {
        let (reg, counter) = echo_registry();
        let mut h = TestHarness::with_registry("m", vec![], reg);
        h.deps.hooks = Some(Arc::new(HookRunner::new(
            vec![hook_cfg(
                HookEvent::PreToolUse,
                &allow_with_updated_input_cmd(),
            )],
            "sess",
        )));
        h.push(tool_turn("t1", "echo", json!({"text": "原始参数"})));
        h.push(text_turn("done"));
        let mut conv = Conversation::new();
        conv.push(Message::user_text("hi"));
        run_agent_turn(&h.deps, &mut conv, &RunOpts::new(vec![]), &h.sink()).await;

        assert_eq!(counter.load(Ordering::SeqCst), 1);
        let msgs = conv.active_messages();
        let (_, _, content) = first_tool_result(&msgs);
        assert!(
            content.contains("rewritten"),
            "执行必须用改写后的参数: {content}"
        );
        assert!(!content.contains("原始参数"), "原参数不应被执行: {content}");
    }

    #[tokio::test]
    async fn post_tool_use_block_converts_result_to_error() {
        let (reg, counter) = echo_registry();
        let mut h = TestHarness::with_registry("m", vec![], reg);
        let reason = if cfg!(windows) {
            "blocked"
        } else {
            "结果不过审"
        };
        h.deps.hooks = Some(Arc::new(HookRunner::new(
            vec![hook_cfg(HookEvent::PostToolUse, &deny_cmd(reason))],
            "sess",
        )));
        h.push(tool_turn("t1", "echo", json!({"text": "hi"})));
        h.push(text_turn("done"));
        let mut conv = Conversation::new();
        conv.push(Message::user_text("hi"));
        run_agent_turn(&h.deps, &mut conv, &RunOpts::new(vec![]), &h.sink()).await;

        // PostToolUse 在执行之后：工具已跑（计数 1），但结果被改写为错误回喂
        assert_eq!(counter.load(Ordering::SeqCst), 1);
        let msgs = conv.active_messages();
        let (_, is_error, content) = first_tool_result(&msgs);
        assert!(is_error);
        assert!(content.contains(reason), "{content}");
    }

    #[tokio::test]
    async fn stop_hook_block_forces_continuation() {
        // 首次 Stop 阻断（写标记文件），第二次放行——验证「注入 reason 强迫继续」
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let marker =
            std::env::temp_dir().join(format!("mcode-stop-hook-{}-{n}", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        let cmd = if cfg!(windows) {
            format!(
                "if exist \"{}\" (exit /b 0) else (type nul > \"{}\" & echo continue-please 1>&2 & exit /b 2)",
                marker.display(),
                marker.display()
            )
        } else {
            format!(
                "if [ -e {m} ]; then exit 0; else touch {m}; echo continue-please >&2; exit 2; fi",
                m = marker.display()
            )
        };
        let mut h = TestHarness::new("m", vec![]);
        h.deps.hooks = Some(Arc::new(HookRunner::new(
            vec![hook_cfg(HookEvent::Stop, &cmd)],
            "sess",
        )));
        h.push(text_turn("第一版"));
        h.push(text_turn("最终版"));
        let mut conv = Conversation::new();
        conv.push(Message::user_text("做任务"));
        let outcome = run_agent_turn(&h.deps, &mut conv, &RunOpts::new(vec![]), &h.sink()).await;

        assert_eq!(outcome.reply, "最终版", "阻断后模型被强迫继续出第二版");
        assert_eq!(outcome.turns, 2);
        // 注入的强迫继续消息入库（synthetic user 消息）
        assert!(
            conv.active_messages()
                .iter()
                .any(|m| m.role == Role::User && m.text().contains("continue-please")),
            "Stop hook 的 reason 应作为 user 消息注入"
        );
        // UI 可见恢复动作
        assert!(h.emitted().iter().any(
            |e| matches!(e, AgentEvent::Recovering { action } if action.contains("Stop hook"))
        ));
        let _ = std::fs::remove_file(&marker);
    }
}
