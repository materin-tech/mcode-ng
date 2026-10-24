//! TUI 主循环：crossterm 事件 → 状态机 → ratatui 渲染；回合在 tokio
//! 任务里跑，事件经无界通道回 UI 线程。
//!
//! 交互契约（对齐 CC 的核心键位）：
//! - Enter 发送；busy 时输入自动排队，回合结束按 FIFO 续发；
//! - esc 打断当前回合；**双击 esc** 打开回退列表（会话 + 文件快照）；
//! - 工具需确认时弹窗：y/Enter 允许一次、a 本会话总是、n/Esc 拒绝；
//! - `!cmd` 绕过模型直接跑 shell（输出进 transcript，详情可展开）；
//! - `/context` 显示当前活跃路径的 token 估算；
//! - shift+tab 循环权限模式（与 PermissionManager 同步）；
//! - ctrl+o 简洁/详情视图；ctrl+c 退出（终端状态经 Drop 兜底恢复）。

pub mod complete;
pub mod render;
pub mod state;

use crate::print_mode::run_prepared;
use crate::session_setup::{build_runtime, SessionRuntime};
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use mcode_core::agent::AgentEvent;
use mcode_core::message::{Conversation, Message};
use mcode_core::permission::Decision;
use mcode_core::sessions::SessionStore;
use mcode_core::subagents::AgentTaskStatus;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use state::{Action, AppState, Checkpoint, ConfirmPrompt, LineKind};
use std::collections::BTreeMap;
use std::io::stdout;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Mutex};

/// 事件轮询间隔（UI 刷新粒度）。
const POLL_MS: u64 = 50;
/// 双击 esc 判定窗口。
const DOUBLE_ESC_MS: u64 = 600;

/// 终端状态兜底：panic/early-return 都能还原 cooked mode。
struct TerminalGuard;
impl TerminalGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        crossterm::execute!(stdout(), EnterAlternateScreen)?;
        Ok(TerminalGuard)
    }
}
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = crossterm::execute!(stdout(), LeaveAlternateScreen);
    }
}

/// 发送前展开：`/技能名 参数` → 技能手册全文 + 参数；非斜杠原样通过。
/// 未知斜杠命令 → Err（UI 提示，不发给模型）。
fn expand_prompt(rt: &SessionRuntime, prompt: &str) -> std::result::Result<String, String> {
    let Some(rest) = prompt.strip_prefix('/') else {
        return Ok(prompt.to_string());
    };
    let (name, args) = match rest.split_once(' ') {
        Some((n, a)) => (n, a.trim()),
        None => (rest, ""),
    };
    match rt.skills.get(name) {
        Some(skill) => {
            let body = std::fs::read_to_string(&skill.path).unwrap_or_default();
            let args_note = if args.is_empty() {
                String::new()
            } else {
                format!("\n用户参数：{args}")
            };
            Ok(format!(
                "已装载技能 {}。按以下手册执行：\n\n---\n{body}---{args_note}",
                skill.name
            ))
        }
        None => Err(format!(
            "未知命令 /{name}。/help 看内置命令；可用技能：{}",
            rt.skills.names().join(", ")
        )),
    }
}

/// `!cmd`：绕过模型直接跑 shell（输出走事件通道进 transcript）。
fn spawn_shell(etx: mpsc::UnboundedSender<AgentEvent>, cwd: Arc<std::path::PathBuf>, cmd: String) {
    let _ = etx.send(AgentEvent::ToolUseStart {
        id: format!("shell-{}", mcode_core::util::now_ms()),
        name: "bash".into(),
        scope: Some("shell".into()),
    });
    tokio::spawn(async move {
        let mut command = if cfg!(windows) {
            let mut c = tokio::process::Command::new("cmd");
            c.arg("/C").arg(&cmd);
            c
        } else {
            let mut c = tokio::process::Command::new("sh");
            c.arg("-c").arg(&cmd);
            c
        };
        command.current_dir(cwd.as_path());
        let _ = mcode_core::util::hide_console_window(&mut command);
        let (success, output) = match command.output().await {
            Ok(o) => {
                let mut text = String::from_utf8_lossy(&o.stdout).to_string();
                let err = String::from_utf8_lossy(&o.stderr);
                if !err.trim().is_empty() {
                    if !text.is_empty() && !text.ends_with('\n') {
                        text.push('\n');
                    }
                    text.push_str(&err);
                }
                (o.status.success(), text)
            }
            Err(e) => (false, format!("启动失败：{e}")),
        };
        let _ = etx.send(AgentEvent::ToolResult {
            name: "bash".into(),
            success,
            output,
            scope: Some("shell".into()),
        });
    });
}

/// rewind 落定：文件快照恢复 + 会话叶回退 + transcript 截断。
async fn apply_rewind(
    rt: &SessionRuntime,
    conv: &Mutex<Conversation>,
    state: &mut AppState,
    cp: &Checkpoint,
) {
    let session = match rt.deps.session.as_ref() {
        Some(s) => s.clone(),
        None => {
            state.push_line(LineKind::System, "无会话句柄，无法回退");
            return;
        }
    };
    let lines = match mcode_core::sessions::read_lines(session.path()) {
        Ok(l) => l,
        Err(e) => {
            state.push_line(LineKind::System, format!("读取会话失败：{e}"));
            return;
        }
    };
    let snaps = mcode_core::sessions::snapshots_after(&lines, &cp.id);
    let restored = mcode_core::sessions::apply_snapshots(&snaps);
    conv.lock().await.rewind_before(&cp.id);
    // 落盘叶指针：resume 时停在回退点（None = 回退到空会话）
    session.append_leaf(cp.parent.as_deref());

    state.close_rewind();
    state.truncate_from(cp.line_index);
    state.push_line(
        LineKind::System,
        format!(
            "已回退到「{}」：恢复 {} 个文件快照，其后回合已丢弃",
            cp.prompt, restored
        ),
    );
}

/// 会话恢复方式（--continue / --resume）。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub enum Resume {
    #[default]
    Fresh,
    /// 继续本项目最近会话（`--continue`）。
    Continue,
    /// 列出本项目会话（`--resume` 无 id）。
    Pick,
    /// 加载指定会话（`--resume <id>`）。
    Id(String),
}

/// 从已加载会话重建 transcript（活跃路径文本消息；工具历史折叠为一行）。
fn replay_lines(conv: &Conversation) -> Vec<state::Line> {
    use mcode_core::message::Role;
    conv.active_messages()
        .iter()
        .filter_map(|m| {
            let text = m.text();
            if text.trim().is_empty() {
                return None;
            }
            let kind = match m.role {
                Role::User => LineKind::User,
                Role::Assistant => LineKind::Assistant,
            };
            Some(state::Line {
                kind,
                text,
                detail: None,
            })
        })
        .collect()
}

/// 「本会话总是允许」→ desktop.json 追加 `tool: *` allow 规则（已有同键跳过）。
/// 回答 AllowAlways 时 runner 只更新会话内集合，跨会话持久化是 UI 层职责。
fn persist_allow_always(tool: &str) -> Result<()> {
    let Some(path) = mcode_core::util::desktop_config_path() else {
        anyhow::bail!("无法定位 desktop.json（无 HOME？）");
    };
    persist_allow_always_to(&path, tool)
}

/// [`persist_allow_always`] 的路径注入内核（测试用；desktop.json 不存在则新建）。
fn persist_allow_always_to(path: &std::path::Path, tool: &str) -> Result<()> {
    let mut desktop = mcode_core::config::DesktopConfig::load_from(path);
    let exists = desktop
        .permission_rules
        .iter()
        .any(|r| r.tool.eq_ignore_ascii_case(tool) && (r.pattern == "*" || r.pattern.is_empty()));
    if exists {
        return Ok(());
    }
    desktop
        .permission_rules
        .push(mcode_core::config::PermissionRule {
            tool: tool.to_string(),
            pattern: "*".into(),
            allow: true,
            behavior: Some(mcode_core::permission::Behavior::Allow),
        });
    desktop.save_to(path)?;
    Ok(())
}

/// 后台任务表轮询（~1s 粒度）：状态迁移到终态 → transcript 通知。
/// `seen` 是 UI 侧上次状态快照；首见条目只记基线（追历史时不刷屏），
/// 但首见即终态照样通知——用户不该错过已完成的结果。
fn poll_task_table(
    table: &BTreeMap<String, mcode_core::subagents::BackgroundAgent>,
    seen: &mut BTreeMap<String, AgentTaskStatus>,
    state: &mut AppState,
) {
    for (id, bg) in table.iter() {
        let changed = match seen.get(id) {
            None => {
                seen.insert(id.clone(), bg.status.clone());
                bg.status != AgentTaskStatus::Running
            }
            Some(prev) if *prev != bg.status => {
                seen.insert(id.clone(), bg.status.clone());
                bg.status != AgentTaskStatus::Running
            }
            Some(_) => false,
        };
        if changed {
            let what = match bg.status {
                AgentTaskStatus::Completed => "完成",
                AgentTaskStatus::Failed => "失败",
                AgentTaskStatus::Killed => "已停止",
                AgentTaskStatus::Running => continue,
            };
            state.push_line(
                LineKind::System,
                format!(
                    "后台任务 {id}（{}）{}：{}（task_output 查看结果）",
                    bg.agent_type, what, bg.description
                ),
            );
        }
    }
}

/// TUI 入口。
pub async fn run(model: Option<&str>, fallback: Option<Vec<String>>, resume: Resume) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let config = mcode_core::config::Config::load()?;
    let desktop = mcode_core::config::DesktopConfig::load()?;
    let rt = Arc::new(build_runtime(&cwd, &config, &desktop, model, fallback, None).await?);

    let (etx, mut erx) = mpsc::unbounded_channel::<AgentEvent>();

    // 恢复会话：Fresh 起新会话；Continue/Id 加载并重建 transcript；Pick 列清单。
    let (conv, resume_note): (Conversation, String) = match &resume {
        Resume::Fresh => (Conversation::new(), String::new()),
        Resume::Continue => {
            let store = SessionStore::for_project(&cwd)?;
            match store.list().first().map(|s| s.session_id.clone()) {
                Some(id) => {
                    let (c, _) = store.load(&id)?;
                    let n = c.messages.len();
                    (
                        c,
                        format!("已继续最近会话 {id}（{n} 条消息，活跃路径见下）"),
                    )
                }
                None => (Conversation::new(), "本项目无历史会话，开新会话".into()),
            }
        }
        Resume::Id(ref id) => {
            let store = SessionStore::for_project(&cwd)?;
            let (c, _) = store
                .load(id)
                .map_err(|e| anyhow::anyhow!("恢复会话 {id} 失败：{e:#}"))?;
            let n = c.messages.len();
            (c, format!("已恢复会话 {id}（{n} 条消息，活跃路径见下）"))
        }
        Resume::Pick => {
            let store = SessionStore::for_project(&cwd)?;
            let list = store.list();
            let mut replay = vec![state::Line {
                kind: LineKind::System,
                text: "本项目会话（用 `mcode --resume <id>` 打开）:".into(),
                detail: None,
            }];
            for s in &list {
                let id8: String = s.session_id.chars().take(12).collect();
                let title = s.title.as_deref().unwrap_or("（无标题）");
                replay.push(state::Line {
                    kind: LineKind::System,
                    text: format!(
                        "  {id8}  「{}」  {} 条消息",
                        title.chars().take(40).collect::<String>(),
                        s.message_count
                    ),
                    detail: None,
                });
            }
            if list.is_empty() {
                replay.push(state::Line {
                    kind: LineKind::System,
                    text: "（本项目暂无历史会话）".into(),
                    detail: None,
                });
            }
            // 不进对话循环：列出即出（CC 的 --resume 无 id 是选择器，先给清单）
            let _guard = TerminalGuard::enter()?;
            let mut terminal = Terminal::new(CrosstermBackend::new(stdout()))?;
            let mut st = AppState {
                mode: config.default_mode,
                ..Default::default()
            };
            st.lines = replay;
            terminal.draw(|f| render::draw(f, &st))?;
            std::thread::sleep(Duration::from_millis(2500));
            return Ok(());
        }
    };
    let replay = replay_lines(&conv);
    let conv = Arc::new(Mutex::new(conv));

    let _guard = TerminalGuard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout()))?;

    let mut state = AppState {
        mode: config.default_mode,
        model: rt.deps.model.clone(),
        ..Default::default()
    };
    state.set_workspace_files(complete::collect_workspace_files(&cwd));
    let mut cmds: Vec<String> = state::BUILTIN_SLASH_COMMANDS
        .iter()
        .map(|s| s.to_string())
        .collect();
    cmds.extend(rt.skills.names());
    state.set_slash_commands(cmds);
    rt.deps.perms.lock().await.set_mode(state.mode);
    for l in replay {
        state.lines.push(l);
    }

    let plugin_note = {
        let names = rt.plugins.names();
        if names.is_empty() {
            String::new()
        } else {
            format!(" ｜ 插件: {}", names.join(", "))
        }
    };
    state.push_line(
        LineKind::System,
        format!(
            "MCode {} · 模型 {} · {}{plugin_note} ｜ /help 命令，双 esc 回退，shift+tab 切权限",
            env!("CARGO_PKG_VERSION"),
            rt.deps.model,
            cwd.display()
        ),
    );
    if !resume_note.is_empty() {
        state.push_line(LineKind::System, resume_note);
    }

    // 待发起的回合（队列出队后在这里落地）+ 待应答的确认请求
    let mut pending: Option<String> = None;
    let mut confirm_responder: Option<tokio::sync::oneshot::Sender<Decision>> = None;
    let mut last_esc: Option<Instant> = None;
    // 后台任务轮询：上次状态快照 + 分频计数（POLL_MS 粒度凑出 ~1s）
    let mut task_seen: BTreeMap<String, AgentTaskStatus> = BTreeMap::new();
    let mut poll_tick: u32 = 0;

    loop {
        // 1) 回合事件 → 状态（Done 由回合契约保证必达，收尾 + 续发队列）
        while let Ok(e) = erx.try_recv() {
            let is_done = matches!(e, AgentEvent::Done { .. });
            state.apply_event(&e);
            if is_done {
                state.finish_turn();
                pending = pending.or_else(|| state.dequeue_next());
            }
        }

        // 1.5) 权限确认请求 → 弹窗（运行中随时可能到达）
        {
            let mut rx = rt.confirm_rx.lock().await;
            while let Ok(req) = rx.try_recv() {
                state.confirm = Some(ConfirmPrompt {
                    tool: req.tool.clone(),
                    args_summary: req.args_summary.clone(),
                });
                confirm_responder = Some(req.respond);
            }
        }

        // 1.6) 后台任务状态轮询（终态迁移 → transcript 通知）
        poll_tick = poll_tick.wrapping_add(1);
        if poll_tick as u64 >= 1000 / POLL_MS {
            poll_tick = 0;
            // std Mutex：中毒 = 有任务线程 panic，跳过本轮轮询即可
            if let Ok(table) = rt.task_table.lock() {
                poll_task_table(&table, &mut task_seen, &mut state);
            }
        }

        // 2) 发起回合（不 busy 时；expand 失败只报错不进模型）
        if let Some(prompt) = pending.take() {
            if state.busy {
                state.queue.push_back(prompt);
            } else {
                let expanded_prompt = match expand_prompt(&rt, &prompt) {
                    Ok(p) => p,
                    Err(msg) => {
                        state.push_line(LineKind::System, msg);
                        continue;
                    }
                };
                let emit: mcode_core::agent::EventSink = {
                    let etx = etx.clone();
                    Arc::new(move |e| {
                        let _ = etx.send(e);
                    })
                };
                state.busy = true;
                state.status = "思考中…".into();
                let line_index = state.lines.len();
                state.push_line(LineKind::User, prompt.clone());
                // checkpoint：TUI 自己 push 拿消息 id（此刻无回合持锁）
                let (cp_id, parent) = {
                    let mut guard = conv.lock().await;
                    let parent = guard.leaf_id.clone();
                    let cp_id = guard.push(Message::user_text(&expanded_prompt));
                    rt.deps
                        .record_message(guard.messages.last().expect("刚 push 必有"));
                    (cp_id, parent)
                };
                state.record_checkpoint(Checkpoint {
                    id: cp_id,
                    parent,
                    prompt: prompt.clone(),
                    line_index,
                });
                let rt2 = rt.clone();
                let conv2 = conv.clone();
                let model_override = Some(state.model.clone());
                tokio::spawn(async move {
                    rt2.deps.cancel_tx.send_replace(false);
                    let mut guard = conv2.lock().await;
                    run_prepared(&rt2, &mut guard, model_override.as_deref(), emit).await;
                });
            }
        }

        // 3) 键盘/鼠标（弹窗模态最高优先）
        if event::poll(Duration::from_millis(POLL_MS))? {
            match event::read()? {
                Event::Key(k) if k.kind == crossterm::event::KeyEventKind::Press => {
                    if state.confirm.is_some() {
                        // 权限确认模态：全部按键拦截
                        match k.code {
                            KeyCode::Char('c')
                                if k.modifiers
                                    .contains(crossterm::event::KeyModifiers::CONTROL) =>
                            {
                                state.should_quit = true;
                            }
                            KeyCode::Char('y') | KeyCode::Enter => {
                                if let Some(tx) = confirm_responder.take() {
                                    let _ = tx.send(Decision::Allow);
                                    state.push_line(
                                        LineKind::System,
                                        format!(
                                            "已允许 {}",
                                            state
                                                .confirm
                                                .as_ref()
                                                .map(|c| c.tool.as_str())
                                                .unwrap_or("")
                                        ),
                                    );
                                }
                                state.confirm = None;
                            }
                            KeyCode::Char('a') => {
                                let tool = state
                                    .confirm
                                    .as_ref()
                                    .map(|c| c.tool.clone())
                                    .unwrap_or_default();
                                if let Some(tx) = confirm_responder.take() {
                                    let _ = tx.send(Decision::AllowAlways);
                                    state.push_line(
                                        LineKind::System,
                                        format!("本会话总是允许 {tool}（已持久化）"),
                                    );
                                }
                                state.confirm = None;
                                if let Err(e) = persist_allow_always(&tool) {
                                    state.push_line(
                                        LineKind::System,
                                        format!("规则落盘失败：{e:#}"),
                                    );
                                }
                            }
                            KeyCode::Char('n') | KeyCode::Esc => {
                                if let Some(tx) = confirm_responder.take() {
                                    let _ = tx.send(Decision::Deny);
                                    state.push_line(
                                        LineKind::System,
                                        format!(
                                            "已拒绝 {}",
                                            state
                                                .confirm
                                                .as_ref()
                                                .map(|c| c.tool.as_str())
                                                .unwrap_or("")
                                        ),
                                    );
                                }
                                state.confirm = None;
                            }
                            _ => {}
                        }
                    } else if state.rewind.is_some() {
                        // 回退列表模态
                        match k.code {
                            KeyCode::Char('c')
                                if k.modifiers
                                    .contains(crossterm::event::KeyModifiers::CONTROL) =>
                            {
                                state.should_quit = true;
                            }
                            KeyCode::Up => state.rewind_move(-1),
                            KeyCode::Down => state.rewind_move(1),
                            KeyCode::Enter => {
                                if let Some(cp) = state.rewind_selected() {
                                    apply_rewind(&rt, &conv, &mut state, &cp).await;
                                } else {
                                    state.close_rewind();
                                }
                            }
                            KeyCode::Esc => state.close_rewind(),
                            _ => {}
                        }
                    } else if state.completion.is_some() {
                        // @ 文件补全模态：↑↓ 选候选，Tab/Enter 接受，Esc 关闭；
                        // 其余按键照常编辑并刷新候选
                        match k.code {
                            KeyCode::Char('c')
                                if k.modifiers
                                    .contains(crossterm::event::KeyModifiers::CONTROL) =>
                            {
                                state.should_quit = true;
                            }
                            KeyCode::Up => state.completion_move(-1),
                            KeyCode::Down => state.completion_move(1),
                            KeyCode::Tab | KeyCode::Enter => {
                                state.accept_completion();
                            }
                            KeyCode::Esc => state.close_completion(),
                            _ => {
                                if render::on_key(&mut state, k) {
                                    state.refresh_completion();
                                }
                            }
                        }
                    } else {
                        match k.code {
                            KeyCode::Esc => {
                                let now = Instant::now();
                                let is_double = last_esc
                                    .map(|t| {
                                        now.duration_since(t)
                                            <= Duration::from_millis(DOUBLE_ESC_MS)
                                    })
                                    .unwrap_or(false);
                                last_esc = Some(now);
                                if is_double {
                                    if state.open_rewind() {
                                        state.push_line(
                                            LineKind::System,
                                            "回退列表：↑↓ 选择，Enter 回退到该回合之前",
                                        );
                                    }
                                } else if state.busy {
                                    let _ = rt.deps.cancel_tx.send(true);
                                    state.push_line(LineKind::System, "正在打断…");
                                } else {
                                    state.input.clear();
                                }
                            }
                            KeyCode::BackTab => {
                                let m = state.cycle_mode();
                                rt.deps.perms.lock().await.set_mode(m);
                            }
                            KeyCode::Enter => match state.submit() {
                                Some(Action::Send(prompt)) => {
                                    if state.busy {
                                        state.queue.push_back(prompt);
                                    } else {
                                        pending = Some(prompt);
                                    }
                                }
                                Some(Action::Bash(cmd)) => {
                                    spawn_shell(
                                        etx.clone(),
                                        Arc::new(rt.deps.tool_ctx.cwd.clone()),
                                        cmd,
                                    );
                                }
                                Some(Action::Local("quit")) => break,
                                Some(Action::Local("context")) => {
                                    let guard = conv.lock().await;
                                    let used = mcode_core::token::raw_estimate_messages(
                                        &guard.active_messages(),
                                    );
                                    let window =
                                        mcode_core::agent::context_window_for_model(&rt.deps.model);
                                    let pct = used * 100 / window.max(1);
                                    state.push_line(
                                        LineKind::System,
                                        format!("上下文估算 ~{used} / {window} tokens（{pct}%）"),
                                    );
                                }
                                Some(Action::Local(_)) => {}
                                None => {}
                            },
                            _ => {
                                if render::on_key(&mut state, k) {
                                    match k.code {
                                        // 刚敲进 '@' → 文件补全；行首 '/' → 命令补全
                                        KeyCode::Char('@') => state.open_completion(),
                                        KeyCode::Char('/') if state.input == "/" => {
                                            state.open_slash_completion();
                                        }
                                        _ => {}
                                    }
                                }
                            }
                        }
                    }
                }
                Event::Mouse(m) => render::on_mouse(&mut state, m.kind),
                _ => {}
            }
        }

        // 4) 绘制 + 退出判定
        terminal.draw(|f| render::draw(f, &state))?;
        if state.should_quit {
            break;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcode_core::message::Message;

    #[test]
    fn replay_extracts_active_path_text_only() {
        let mut conv = Conversation::new();
        conv.push(Message::user_text("第一问"));
        conv.push(Message::assistant_text("第一答"));
        let id2 = conv.push(Message::user_text("第二问"));

        let lines = replay_lines(&conv);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].kind, LineKind::User);
        assert_eq!(lines[0].text, "第一问");
        assert_eq!(lines[1].kind, LineKind::Assistant);

        // rewind 后只重放活跃路径
        conv.rewind_before(&id2);
        let lines = replay_lines(&conv);
        assert_eq!(lines.len(), 2, "被回退的消息不重放");
    }

    #[test]
    fn allow_always_rule_dedups_and_persists() {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp =
            std::env::temp_dir().join(format!("mcode-allow-always-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let path = tmp.join("desktop.json");

        persist_allow_always_to(&path, "bash").unwrap();
        persist_allow_always_to(&path, "bash").unwrap(); // 重复回答去重
        let d = mcode_core::config::DesktopConfig::try_load_from(&path).unwrap();
        assert_eq!(d.permission_rules.len(), 1, "重复规则不叠加");
        assert_eq!(d.permission_rules[0].tool, "bash");
        assert_eq!(
            d.permission_rules[0].behavior_or_legacy(),
            mcode_core::permission::Behavior::Allow
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
