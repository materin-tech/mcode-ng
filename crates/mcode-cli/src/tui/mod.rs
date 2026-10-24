//! TUI 主循环：CC 式两区渲染（冻结 scrollback + 底部动态区）。
//!
//! 已完成的块打印进终端 scrollback（append-only，原生滚动/复制），
//! 底部动态区每帧重绘（流式块 + spinner + 弹窗 + 输入框 + footer）。
//!
//! 交互契约（对齐 Claude Code）：
//! - Enter 发送；行尾 `\` + Enter / ctrl+j / alt+enter 软换行；
//! - busy 时输入自动排队（`⏸` 提示），回合结束 FIFO 续发；
//! - esc 打断当前回合（Interrupted 标记）；双击 esc 打开回退列表；
//! - 工具确认弹窗：↑↓/1-3 选择，y/a/n/Esc 快捷，a 持久化到 desktop.json；
//! - `!cmd` 绕过模型直接跑 shell；`/` 与 `@` 补全；ctrl+o 详情视图；
//! - shift+tab 循环权限模式（footer 指示）；ctrl+c 清输入 / 二次退出；
//! - 回合进行中底部 spinner（`✻ Verb… (esc to interrupt · ↓ tokens · 计时)`）。

pub mod blocks;
pub mod commands;
pub mod complete;
pub mod confirm;
pub mod input;
pub mod render;
pub mod spinner;
pub mod style;
pub mod term;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use mcode_core::agent::AgentEvent;
use mcode_core::message::{Conversation, Message};
use mcode_core::permission::Decision;
use mcode_core::sessions::SessionStore;
use mcode_core::subagents::AgentTaskStatus;
use tokio::sync::{mpsc, Mutex};

use crate::print_mode::run_prepared;
use crate::session_setup::{build_runtime, SessionRuntime};

use self::blocks::{Block, NoticeKind, TuiState};
use self::complete::CompletionState;
use self::confirm::ConfirmDialog;
use self::input::{InputBuffer, KeyKind};
use self::render::FrameCtx;
use self::style::{truncate_chars, Color, Span, StyledLine};
use self::term::Term;

/// 事件轮询间隔（UI 刷新粒度 = spinner 帧间隔）。
const POLL_MS: u64 = 50;
/// 双击 esc 判定窗口。
const DOUBLE_ESC_MS: u64 = 600;
/// ctrl+c 二次退出判定窗口。
const CTRL_C_QUIT_MS: u64 = 2000;

/// 随机告别语（退出时打印一条）。
const GOODBYES: [&str; 5] = [
    "Bye!",
    "回见。",
    "下次见 👋",
    "去写点好代码。",
    "See you next run.",
];

// ---------------------------------------------------------------------------
// 会话级控制态（blocks 之外的一切：模式/effort/goal/checkpoint/退出标志）
// ---------------------------------------------------------------------------

/// 一个回合检查点（rewind 目标）：回合入口的 user 消息 + 其 parent。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkpoint {
    pub id: String,
    /// 该 user 消息的 parent（rewind_before 目标）；None = 会话首条。
    pub parent: Option<String>,
    pub prompt: String,
    /// 该回合入口块在 blocks 里的下标（rewind 落定后截断）。
    pub block_index: usize,
}

/// goal 外层循环状态。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GoalState {
    pub objective: String,
    /// 已完成回合数（含判官否决的续跑），达 [`mcode_core::agent::goal::MAX_GOAL_ROUNDS`] 上限即停。
    pub round: usize,
}

/// 会话级控制态（不属于渲染块模型的旁路状态）。
#[derive(Debug, Default)]
struct SessionCtl {
    /// 权限模式（shift+tab 循环；与 PermissionManager 同步）。
    mode: mcode_core::permission::Mode,
    /// /effort 选择（None = API 侧默认；下一回合生效）。
    effort: Option<mcode_core::api::ThinkingEffort>,
    goal: Option<GoalState>,
    checkpoints: Vec<Checkpoint>,
    should_quit: bool,
    /// /resume <id> 请求（退出后重建运行时载入该会话）。
    resume_request: Option<String>,
    /// 距 auto-compact 的百分比缓存（会话开始/回合结束/切模型时刷新）。
    context_pct: Option<u64>,
    /// 本会话累计 token（Usage 事件；/cost /token 展示）。
    tokens_in: u64,
    tokens_out: u64,
}

/// 提交输入后的分派动作。
#[derive(Debug, Clone, PartialEq, Eq)]
enum Action {
    /// 发起回合（display 与 payload 同文）。
    Send(String),
    /// 发起回合，但 transcript 展示 `display`（如 `/init`），实际发送 `payload`。
    Prompt { display: String, payload: String },
    /// `!cmd`：绕过模型直接跑 shell。
    Bash(String),
    /// slash 命令已本地处理。
    Local(&'static str),
    /// 需要运行时数据/IO 的命令（run 循环落地）。
    Runtime(RuntimeCmd),
}

/// 需要会话/存储/后端配合的命令。
#[derive(Debug, Clone, PartialEq, Eq)]
enum RuntimeCmd {
    /// /compact [instructions]
    Compact(Option<String>),
    /// /export [file]
    Export(Option<String>),
    /// /rename <title>
    Rename(String),
    /// /resume [id]；None = 列出会话
    Resume(Option<String>),
    /// /provider [name]；None = 展示清单
    Provider(Option<String>),
}

// ---------------------------------------------------------------------------
// 事件/终端 helpers
// ---------------------------------------------------------------------------

/// crossterm 按键 → 终端无关的 [`KeyKind`]。
fn translate_key(k: KeyEvent) -> KeyKind {
    let m = k.modifiers;
    match k.code {
        KeyCode::Char('c') if m.contains(KeyModifiers::CONTROL) => KeyKind::CtrlC,
        KeyCode::Char('d') if m.contains(KeyModifiers::CONTROL) => KeyKind::CtrlD,
        KeyCode::Char('j') if m.contains(KeyModifiers::CONTROL) => KeyKind::CtrlJ,
        KeyCode::Char('o') if m.contains(KeyModifiers::CONTROL) => KeyKind::CtrlO,
        KeyCode::Char('r') if m.contains(KeyModifiers::CONTROL) => KeyKind::CtrlR,
        KeyCode::Char('a') if m.contains(KeyModifiers::CONTROL) => KeyKind::CtrlA,
        KeyCode::Char('e') if m.contains(KeyModifiers::CONTROL) => KeyKind::CtrlE,
        KeyCode::Char('k') if m.contains(KeyModifiers::CONTROL) => KeyKind::CtrlK,
        KeyCode::Char('u') if m.contains(KeyModifiers::CONTROL) => KeyKind::CtrlU,
        KeyCode::Char('w') if m.contains(KeyModifiers::CONTROL) => KeyKind::CtrlW,
        KeyCode::Enter if m.contains(KeyModifiers::ALT) || m.contains(KeyModifiers::SHIFT) => {
            KeyKind::AltEnter
        }
        KeyCode::Enter => KeyKind::Enter,
        KeyCode::Esc => KeyKind::Esc,
        KeyCode::BackTab => KeyKind::ShiftTab,
        KeyCode::Tab if m.contains(KeyModifiers::SHIFT) => KeyKind::ShiftTab,
        KeyCode::Tab => KeyKind::Tab,
        KeyCode::Up => KeyKind::Up,
        KeyCode::Down => KeyKind::Down,
        KeyCode::Left => KeyKind::Left,
        KeyCode::Right => KeyKind::Right,
        KeyCode::Home => KeyKind::Home,
        KeyCode::End => KeyKind::End,
        KeyCode::Backspace => KeyKind::Backspace,
        KeyCode::Delete => KeyKind::Delete,
        KeyCode::PageUp => KeyKind::PageUp,
        KeyCode::PageDown => KeyKind::PageDown,
        KeyCode::Char(c) => KeyKind::Char(c),
        _ => KeyKind::Other,
    }
}

/// 输入编辑键落地（emacs 键位含在 [`KeyKind`] 里）；返回是否改动了缓冲。
fn edit_input(input: &mut InputBuffer, key: KeyKind) -> bool {
    match key {
        KeyKind::Char(c) => input.insert_char(c),
        KeyKind::Backspace => input.backspace(),
        KeyKind::Delete => input.delete(),
        KeyKind::Left => input.left(),
        KeyKind::Right => input.right(),
        KeyKind::Home | KeyKind::CtrlA => input.home(),
        KeyKind::End | KeyKind::CtrlE => input.end(),
        KeyKind::CtrlK => {
            // 删到行尾
            input.buf.truncate(input.cursor);
        }
        KeyKind::CtrlU => {
            // 删到行首
            let start = input.buf[..input.cursor]
                .rfind('\n')
                .map(|i| i + 1)
                .unwrap_or(0);
            input.buf.replace_range(start..input.cursor, "");
            input.cursor = start;
        }
        KeyKind::CtrlW => {
            // 删光标前一个词（先吃尾随空白，再吃非空白段）
            let before = input.buf[..input.cursor].to_string();
            let cut = before
                .trim_end_matches(char::is_whitespace)
                .trim_end_matches(|c: char| !c.is_whitespace())
                .len();
            input.buf.replace_range(cut..input.cursor, "");
            input.cursor = cut;
        }
        _ => return false,
    }
    true
}

/// ctrl+c：有输入先清输入，否则 2s 内二次按下退出。
fn handle_ctrl_c(input: &mut InputBuffer, ctl: &mut SessionCtl, last: &mut Option<Instant>) {
    let now = Instant::now();
    if !input.is_empty() {
        input.clear();
        *last = None;
    } else if last
        .map(|t| now.duration_since(t) <= Duration::from_millis(CTRL_C_QUIT_MS))
        .unwrap_or(false)
    {
        ctl.should_quit = true;
    } else {
        *last = Some(now);
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
    let id = mcode_core::util::now_ms().to_string();
    let _ = etx.send(AgentEvent::ToolUseStart {
        id: id.clone(),
        name: "bash".into(),
        scope: Some("shell".into()),
    });
    // 参数直填（无流式过程）：Bash(cmd) 头行立刻可见
    let _ = etx.send(AgentEvent::ToolInputDelta {
        id,
        partial_json: serde_json::json!({ "command": cmd }).to_string(),
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
            display: None,
            scope: Some("shell".into()),
        });
    });
}

/// rewind 落定：文件快照恢复 + 会话叶回退 + 块截断。
/// 冻结区已打印的行留在 scrollback（append-only 模型不可回擦）。
async fn apply_rewind(
    rt: &Arc<SessionRuntime>,
    conv: &Arc<Mutex<Conversation>>,
    state: &mut TuiState,
    ctl: &mut SessionCtl,
    frozen_upto: &mut usize,
    cp: &Checkpoint,
) {
    let session = match rt.deps.session.as_ref() {
        Some(s) => s.clone(),
        None => {
            state.push_notice("无会话句柄，无法回退", NoticeKind::Error);
            return;
        }
    };
    let lines = match mcode_core::sessions::read_lines(session.path()) {
        Ok(l) => l,
        Err(e) => {
            state.push_notice(format!("读取会话失败：{e}"), NoticeKind::Error);
            return;
        }
    };
    let snaps = mcode_core::sessions::snapshots_after(&lines, &cp.id);
    let restored = mcode_core::sessions::apply_snapshots(&snaps);
    conv.lock().await.rewind_before(&cp.id);
    // 落盘叶指针：resume 时停在回退点（None = 回退到空会话）
    session.append_leaf(cp.parent.as_deref());

    state.blocks.truncate(cp.block_index);
    ctl.checkpoints.retain(|c| c.block_index < cp.block_index);
    *frozen_upto = (*frozen_upto).min(cp.block_index);
    state.push_notice(
        format!(
            "已回退到「{}」：恢复 {} 个文件快照，其后回合已丢弃",
            cp.prompt, restored
        ),
        NoticeKind::Success,
    );
}

/// 从已加载会话重建块序列（活跃路径文本消息；空文本跳过）。
fn replay_blocks(conv: &Conversation) -> Vec<Block> {
    use mcode_core::message::Role;
    conv.active_messages()
        .iter()
        .filter_map(|m| {
            let text = m.text();
            if text.trim().is_empty() {
                return None;
            }
            Some(match m.role {
                Role::User => Block::User { text },
                Role::Assistant => Block::Assistant {
                    text,
                    streaming: false,
                },
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

/// 后台任务表轮询（~1s 粒度）：状态迁移到终态 → 通知。
/// `seen` 是 UI 侧上次状态快照；首见条目只记基线（追历史时不刷屏），
/// 但首见即终态照样通知——用户不该错过已完成的结果。
fn poll_task_table(
    table: &BTreeMap<String, mcode_core::subagents::BackgroundAgent>,
    seen: &mut BTreeMap<String, AgentTaskStatus>,
    state: &mut TuiState,
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
            state.push_notice(
                format!(
                    "后台任务 {id}（{}）{}：{}（task_output 查看结果）",
                    bg.agent_type, what, bg.description
                ),
                NoticeKind::Info,
            );
        }
    }
}

/// 上下文占比（footer `{p}% until auto-compact`）。
fn context_pct_of(conv: &Conversation, model: &str) -> Option<u64> {
    let used = mcode_core::token::raw_estimate_messages(&conv.active_messages());
    let window = mcode_core::agent::context_window_for_model(model);
    Some((used * 100 / window.max(1)) as u64)
}

/// 斜杠命令补全表：(命令名, 菜单右侧说明)。内置目录 + 技能名。
fn slash_command_table(rt: &SessionRuntime) -> Vec<(String, String)> {
    let mut table: Vec<(String, String)> = mcode_core::commands::COMMANDS
        .iter()
        .map(|c| (c.name.to_string(), format!("{} — {}", c.usage, c.desc)))
        .collect();
    for sk in rt.skills.all() {
        let head: String = sk.description.trim().chars().take(60).collect();
        table.push((sk.name.clone(), format!("技能：{head}")));
    }
    table
}

// ---------------------------------------------------------------------------
// slash 分派
// ---------------------------------------------------------------------------

/// 提交文本 → 动作：`!` 走 Bash，`/` 走 slash 分派，其余 Send。
fn classify_input(text: &str, state: &mut TuiState, ctl: &mut SessionCtl) -> Option<Action> {
    if text.trim().is_empty() {
        return None;
    }
    if let Some(cmd) = text.strip_prefix('!') {
        return Some(Action::Bash(cmd.trim().to_string()));
    }
    if let Some(rest) = text.strip_prefix('/') {
        let (name, args) = match rest.split_once(' ') {
            Some((n, a)) => (n, a.trim()),
            None => (rest, ""),
        };
        return Some(dispatch_slash(state, ctl, name, args));
    }
    Some(Action::Send(text.to_string()))
}

/// slash 命令分派。内置命令按 core 共享目录处理（提示带用法+作用）；
/// 已知技能 → 原文 Send（expand 在 run 层装载手册）。
fn dispatch_slash(
    state: &mut TuiState,
    ctl: &mut SessionCtl,
    raw_name: &str,
    args: &str,
) -> Action {
    let name = match mcode_core::commands::lookup(raw_name) {
        Some(spec) => spec.name,
        None => raw_name,
    };
    match name {
        "help" => {
            if args.is_empty() {
                for l in render_help() {
                    state.push_notice(l, NoticeKind::Info);
                }
            } else {
                // /help <command>：单条指令的用法+作用
                match mcode_core::commands::lookup(args) {
                    Some(c) => {
                        state.push_notice(format!("{} — {}", c.usage, c.desc), NoticeKind::Info)
                    }
                    None => state
                        .push_notice(format!("未知命令 /{args}；/help 看全部"), NoticeKind::Error),
                }
            }
            Action::Local("help")
        }
        "clear" => Action::Local("clear"),
        "compact" => {
            let a = args.trim();
            Action::Runtime(RuntimeCmd::Compact((!a.is_empty()).then(|| a.to_string())))
        }
        "context" => Action::Local("context"),
        "cost" => {
            state.push_notice(
                format!(
                    "累计 ↑{} ↓{} tokens（成本按 provider 计价，以控制台账单为准）",
                    style::compact_tokens(ctl.tokens_in),
                    style::compact_tokens(ctl.tokens_out)
                ),
                NoticeKind::Info,
            );
            Action::Local("cost")
        }
        "token" => Action::Local("token"),
        "diff" => Action::Local("diff"),
        "files" => Action::Local("files"),
        "doctor" => Action::Local("doctor"),
        "status" => Action::Local("status"),
        "permissions" => Action::Local("permissions"),
        "mcp" => Action::Local("mcp"),
        "hooks" => Action::Local("hooks"),
        "agents" => Action::Local("agents"),
        "skills" => Action::Local("skills"),
        "provider" => {
            let a = args.trim().to_string();
            Action::Runtime(RuntimeCmd::Provider((!a.is_empty()).then_some(a)))
        }
        "memory" => Action::Local("memory"),
        "init" => Action::Prompt {
            display: "/init".into(),
            payload: init_prompt(),
        },
        "effort" => {
            match args.trim().to_lowercase().as_str() {
                "" => {}
                "low" => ctl.effort = Some(mcode_core::api::ThinkingEffort::Low),
                "medium" => ctl.effort = Some(mcode_core::api::ThinkingEffort::Medium),
                "high" => ctl.effort = Some(mcode_core::api::ThinkingEffort::High),
                other => {
                    state.push_notice(
                        format!("未知级别 {other}；用法：/effort [low|medium|high]"),
                        NoticeKind::Error,
                    );
                    return Action::Local("effort");
                }
            }
            let cur = ctl
                .effort
                .map(label_of_effort)
                .unwrap_or("默认（由 API 决定）");
            state.push_notice(format!("推理努力级别：{cur}"), NoticeKind::Info);
            Action::Local("effort")
        }
        "mode" => {
            let a = args.trim().to_lowercase();
            if a.is_empty() {
                ctl.mode = ctl.mode.next();
                state.push_notice(format!("权限模式 → {}", ctl.mode.label()), NoticeKind::Info);
            } else {
                match mcode_core::permission::Mode::from_label(&a) {
                    Some(m) => {
                        ctl.mode = m;
                        state.push_notice(format!("权限模式 → {}", m.label()), NoticeKind::Info);
                    }
                    None => state.push_notice(
                        format!("未知模式 {a}；可用：manual（逐步确认）/ auto / plan / passon"),
                        NoticeKind::Error,
                    ),
                }
            }
            Action::Local("mode")
        }
        "model" => {
            if args.is_empty() {
                state.push_notice(
                    format!("当前模型：{}（切换：/model <name>）", state.model),
                    NoticeKind::Info,
                );
            } else {
                state.model = args.to_string();
                state.push_notice(
                    format!("模型已切换 → {args}（下一回合生效）"),
                    NoticeKind::Info,
                );
            }
            Action::Local("model")
        }
        "quit" => Action::Local("quit"),
        "rename" => {
            let t = args.trim().to_string();
            if t.is_empty() {
                state.push_notice("用法：/rename <title>", NoticeKind::Info);
                Action::Local("rename")
            } else {
                Action::Runtime(RuntimeCmd::Rename(t))
            }
        }
        "resume" => {
            let a = args.trim().to_string();
            Action::Runtime(RuntimeCmd::Resume((!a.is_empty()).then_some(a)))
        }
        "rewind" => Action::Local("rewind"),
        "save" => Action::Local("save"),
        "export" => {
            let a = args.trim().to_string();
            Action::Runtime(RuntimeCmd::Export((!a.is_empty()).then_some(a)))
        }
        "copy" => Action::Local("copy"),
        "goal" => {
            let obj = args.trim().to_string();
            if obj.is_empty() {
                if ctl.goal.take().is_some() {
                    state.push_notice("🎯 goal 循环已取消", NoticeKind::Info);
                } else {
                    state.push_notice(
                        "用法：/goal <objective>；进行中再次输入 /goal 取消",
                        NoticeKind::Info,
                    );
                }
                return Action::Local("goal");
            }
            if state.busy {
                state.push_notice("回合进行中，先 esc 打断再开 goal", NoticeKind::Warning);
                return Action::Local("goal");
            }
            let prompt = mcode_core::agent::goal::build_goal_prompt(&obj);
            ctl.goal = Some(GoalState {
                objective: obj.clone(),
                round: 0,
            });
            state.push_notice(
                format!("🎯 goal 开启（上限 12 轮）：{obj}；再次输入 /goal 取消"),
                NoticeKind::Info,
            );
            Action::Prompt {
                display: format!("/goal {obj}"),
                payload: prompt,
            }
        }
        other => {
            // 技能分派交由 run 层查 SkillRegistry（dispatch 不持引用）。
            // Send 载荷 = "/name args" 原文。
            Action::Send(format!("/{other} {args}").trim_end().to_string())
        }
    }
}

/// /init 的模型提示词：分析代码库生成 MCODE.md（普通回合，模型用工具自采信息）。
fn init_prompt() -> String {
    [
        "请分析当前代码库并生成 MCODE.md 项目说明文件（工作区根目录）。".to_string(),
        "内容包含：项目用途（一句话）、目录结构速览、构建/测试命令、代码约定。".into(),
        "已有 MCODE.md 则先读取并在其基础上增量完善；写完后只回复一句变更摘要。".into(),
    ]
    .join("\n")
}

/// effort 级别显示名。
fn label_of_effort(e: mcode_core::api::ThinkingEffort) -> &'static str {
    match e {
        mcode_core::api::ThinkingEffort::Low => "low",
        mcode_core::api::ThinkingEffort::Medium => "medium",
        mcode_core::api::ThinkingEffort::High => "high",
    }
}

/// /help 渲染：按 core 共享目录分组，每条指令「用法 — 作用」。
fn render_help() -> Vec<String> {
    let mut out = vec!["命令目录（/help <command> 看单条详情）：".to_string()];
    for (group, cmds) in mcode_core::commands::groups_in_order() {
        out.push(format!("· {}", group.title()));
        for c in cmds {
            out.push(format!("    {:<28} {}", c.usage, c.desc));
        }
    }
    out.push(
        "快捷键：shift+tab 切权限模式 · @文件 补全 · esc 打断 / 双 esc 回退 · ctrl+o 详情 · !cmd 跑 shell · ctrl+c 退出"
            .into(),
    );
    out
}

// ---------------------------------------------------------------------------
// 运行时命令与本地命令落地
// ---------------------------------------------------------------------------

/// `RuntimeCmd` 落地：需要会话/存储/后端配合的斜杠命令。
/// goal 续跑由主循环事件段注入 pending，不经此处。
async fn handle_runtime(
    rt: &Arc<SessionRuntime>,
    conv: &Arc<Mutex<Conversation>>,
    state: &mut TuiState,
    ctl: &mut SessionCtl,
    etx: &mpsc::UnboundedSender<AgentEvent>,
    cmd: RuntimeCmd,
) {
    match cmd {
        RuntimeCmd::Compact(instructions) => {
            // 锁探测：回合运行中不压缩（等锁会跟下一回合的持锁互相顶）
            if conv.try_lock().is_err() {
                state.push_notice("回合运行中，稍后再压缩", NoticeKind::Warning);
                return;
            }
            let rt2 = rt.clone();
            let conv2 = conv.clone();
            let etx2 = etx.clone();
            let emit: mcode_core::agent::EventSink = Arc::new(move |e| {
                let _ = etx2.send(e);
            });
            state.push_notice("正在压缩历史…", NoticeKind::Info);
            tokio::spawn(async move {
                let mut guard = conv2.lock().await;
                if let Err(e) = mcode_core::agent::compact::compact_conversation_with(
                    &rt2.deps,
                    &mut guard,
                    &emit,
                    instructions.as_deref(),
                )
                .await
                {
                    emit(AgentEvent::Error {
                        message: e.to_string(),
                    });
                }
            });
        }
        RuntimeCmd::Export(path) => {
            let path = path
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| std::path::PathBuf::from("mcode-export.md"));
            let guard = conv.lock().await;
            match commands::export_markdown(&guard, &path) {
                Ok(n) => state.push_notice(
                    format!("✓ 已导出 {n} 条消息 → {}", path.display()),
                    NoticeKind::Success,
                ),
                Err(e) => state.push_notice(format!("✗ 导出失败：{e:#}"), NoticeKind::Error),
            }
        }
        RuntimeCmd::Rename(title) => {
            let store = match mcode_core::sessions::SessionStore::for_project(&rt.deps.tool_ctx.cwd)
            {
                Ok(s) => s,
                Err(e) => {
                    state.push_notice(format!("✗ 会话存储不可用：{e}"), NoticeKind::Error);
                    return;
                }
            };
            let id = rt
                .deps
                .session
                .as_ref()
                .map(|s| {
                    s.path()
                        .file_stem()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_string()
                })
                .unwrap_or_default();
            match store.set_title(&id, Some(&title)) {
                Ok(_) => {
                    state.push_notice(format!("✓ 会话已重命名 → {title}"), NoticeKind::Success)
                }
                Err(e) => state.push_notice(format!("✗ 重命名失败：{e}"), NoticeKind::Error),
            }
        }
        RuntimeCmd::Resume(target) => {
            let store = match mcode_core::sessions::SessionStore::for_project(&rt.deps.tool_ctx.cwd)
            {
                Ok(s) => s,
                Err(e) => {
                    state.push_notice(format!("✗ 会话存储不可用：{e}"), NoticeKind::Error);
                    return;
                }
            };
            let list = store.list();
            let Some(id_prefix) = target else {
                state.push_notice("本项目会话：", NoticeKind::Info);
                for l in commands::resume_list_lines(&store) {
                    state.push_notice(l, NoticeKind::Info);
                }
                state.push_notice("用 /resume <id前缀> 打开", NoticeKind::Info);
                return;
            };
            let hits: Vec<_> = list
                .iter()
                .filter(|s| s.session_id.starts_with(&id_prefix))
                .collect();
            match hits.len() {
                0 => state.push_notice(
                    format!("未找到 id 前缀 {id_prefix} 的会话；/resume 看清单"),
                    NoticeKind::Warning,
                ),
                1 => {
                    ctl.resume_request = Some(hits[0].session_id.clone());
                    ctl.should_quit = true;
                }
                n => {
                    state.push_notice(
                        format!("前缀 {id_prefix} 命中 {n} 个会话："),
                        NoticeKind::Info,
                    );
                    for h in &hits {
                        let id8: String = h.session_id.chars().take(12).collect();
                        state.push_notice(
                            format!(
                                "  {id8}  「{}」",
                                h.title.as_deref().unwrap_or("（无标题）")
                            ),
                            NoticeKind::Info,
                        );
                    }
                    state.push_notice("再加长前缀消歧", NoticeKind::Info);
                }
            }
        }
        RuntimeCmd::Provider(name) => {
            let Some(name) = name else {
                return; // 无参清单在 handle_local("provider") 已覆盖
            };
            let mut cfg = match mcode_core::config::Config::load() {
                Ok(c) => c,
                Err(e) => {
                    state.push_notice(format!("✗ 配置读取失败：{e:#}"), NoticeKind::Error);
                    return;
                }
            };
            match cfg.set_active_provider(&name) {
                Ok(_) => match cfg.save() {
                    Ok(_) => state.push_notice(
                        format!("✓ provider → {name}（下一会话生效；/quit 后重进或 /resume 切换）"),
                        NoticeKind::Success,
                    ),
                    Err(e) => {
                        state.push_notice(format!("✗ 配置写入失败：{e:#}"), NoticeKind::Error)
                    }
                },
                Err(e) => state.push_notice(format!("✗ {e:#}"), NoticeKind::Error),
            }
        }
    }
}

/// `Local` 命令落地：直接读运行时/IO 的部分（通知已由 dispatch 发出的命令走空分支）。
#[allow(clippy::too_many_arguments)]
async fn handle_local(
    rt: &Arc<SessionRuntime>,
    conv: &Arc<Mutex<Conversation>>,
    state: &mut TuiState,
    ctl: &mut SessionCtl,
    config: &mcode_core::config::Config,
    term: &mut Term,
    frozen_upto: &mut usize,
    rewind_sel: &mut Option<usize>,
    name: &str,
) {
    match name {
        "quit" => ctl.should_quit = true,
        "clear" => {
            state.blocks.clear();
            ctl.checkpoints.clear();
            *frozen_upto = 0;
        }
        "mode" => rt.deps.perms.lock().await.set_mode(ctl.mode),
        "model" => {
            ctl.context_pct = {
                let g = conv.lock().await;
                context_pct_of(&g, &state.model)
            };
        }
        "rewind" => {
            if state.busy {
                state.push_notice("回合进行中，先 esc 打断再回退", NoticeKind::Warning);
            } else if ctl.checkpoints.is_empty() {
                state.push_notice("暂无可回退的检查点", NoticeKind::Info);
            } else {
                *rewind_sel = Some(0);
            }
        }
        "context" => {
            let guard = conv.lock().await;
            let used = mcode_core::token::raw_estimate_messages(&guard.active_messages());
            let window = mcode_core::agent::context_window_for_model(&rt.deps.model);
            let pct = used * 100 / window.max(1);
            state.push_notice(
                format!("上下文估算 ~{used} / {window} tokens（{pct}%）"),
                NoticeKind::Info,
            );
        }
        "token" => {
            let guard = conv.lock().await;
            let used = mcode_core::token::raw_estimate_messages(&guard.active_messages());
            let window = mcode_core::agent::context_window_for_model(&rt.deps.model);
            let pct = used * 100 / window.max(1);
            drop(guard);
            state.push_notice(
                format!(
                    "上下文 ~{used}/{window} tokens（{pct}%）· 本会话累计 ↑{} ↓{}",
                    style::compact_tokens(ctl.tokens_in),
                    style::compact_tokens(ctl.tokens_out)
                ),
                NoticeKind::Info,
            );
        }
        "status" => {
            let session = rt
                .deps
                .session
                .as_ref()
                .map(|s| {
                    s.path()
                        .file_stem()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_string()
                })
                .unwrap_or_else(|| "（未持久化）".into());
            let provider = config
                .active_provider
                .clone()
                .unwrap_or_else(|| "（未配置）".into());
            let effort = ctl.effort.map(label_of_effort).unwrap_or("默认");
            state.push_notice(
                format!(
                    "MCode v{} · 会话 {} · 模型 {} · provider {provider} · 权限 {} · effort {effort} · {}",
                    env!("CARGO_PKG_VERSION"),
                    session.chars().take(12).collect::<String>(),
                    state.model,
                    ctl.mode.label(),
                    if state.busy { "回合进行中" } else { "空闲" },
                ),
                NoticeKind::Info,
            );
            if let Some(g) = &ctl.goal {
                state.push_notice(
                    format!("🎯 goal 进行中（第 {} 轮）：{}", g.round, g.objective),
                    NoticeKind::Info,
                );
            }
        }
        "doctor" => {
            for c in commands::run_doctor(
                config,
                &mcode_core::config::DesktopConfig::load().unwrap_or_default(),
                &rt.deps.tool_ctx.cwd,
                &rt.mcp_report,
            ) {
                state.push_notice(
                    format!("{} {}：{}", if c.ok { "✓" } else { "✗" }, c.name, c.note),
                    if c.ok {
                        NoticeKind::Info
                    } else {
                        NoticeKind::Error
                    },
                );
            }
        }
        "permissions" => {
            let perms = rt.deps.perms.lock().await;
            let rules: Vec<String> = perms
                .rules()
                .iter()
                .map(|r| {
                    format!(
                        "  {} {} → {}",
                        r.tool,
                        r.pattern,
                        if r.allow { "ALLOW" } else { "DENY" }
                    )
                })
                .collect();
            state.push_notice(
                format!(
                    "权限模式：{} · 持久规则 {} 条\n{}",
                    perms.mode.label(),
                    rules.len(),
                    if rules.is_empty() {
                        "  （无）".to_string()
                    } else {
                        rules.join("\n")
                    }
                ),
                NoticeKind::Info,
            );
        }
        "mcp" => {
            let desktop = mcode_core::config::DesktopConfig::load().unwrap_or_default();
            if desktop.mcp_servers.is_empty() {
                state.push_notice(
                    "未配置 MCP server（desktop.json 的 mcp_servers）",
                    NoticeKind::Info,
                );
            }
            for srv in &desktop.mcp_servers {
                let target = match (&srv.command, &srv.url) {
                    (c, _) if !c.is_empty() => format!("stdio {c}"),
                    (_, Some(u)) => format!("http {u}"),
                    _ => "（未配置传输）".to_string(),
                };
                let warn = rt
                    .mcp_report
                    .iter()
                    .find(|w| w.contains(&format!("MCP server {}", srv.name)));
                let dot = if !srv.enabled {
                    "⚪"
                } else if warn.is_some() {
                    "🔴"
                } else {
                    "🟢"
                };
                state.push_notice(
                    format!(
                        "{} {}（{}）{}",
                        dot,
                        srv.name,
                        target,
                        warn.map(|w| format!(" — {w}")).unwrap_or_default()
                    ),
                    NoticeKind::Info,
                );
            }
        }
        "hooks" => match &rt.deps.hooks {
            None => state.push_notice("未配置 hooks（desktop.json 的 hooks）", NoticeKind::Info),
            Some(h) => {
                let cfgs = h.configs();
                state.push_notice(format!("hooks {} 条：", cfgs.len()), NoticeKind::Info);
                for c in cfgs {
                    state.push_notice(
                        format!(
                            "  {:?} match=`{}` → {}（超时 {}s）",
                            c.event,
                            c.matcher,
                            c.command,
                            c.timeout_secs.unwrap_or(60)
                        ),
                        NoticeKind::Info,
                    );
                }
            }
        },
        "agents" => {
            let agents = rt.agents.all();
            if agents.is_empty() {
                state.push_notice(
                    "未找到 subagent 定义（~/.mcode/agents 或 .mcode/agents）",
                    NoticeKind::Info,
                );
            }
            for a in agents {
                let when: String = a.when_to_use.trim().chars().take(52).collect();
                state.push_notice(
                    format!(
                        "  {} · {} · {}",
                        a.agent_type,
                        a.model.as_deref().unwrap_or("默认模型"),
                        when
                    ),
                    NoticeKind::Info,
                );
            }
        }
        "skills" => {
            let skills = rt.skills.all();
            if skills.is_empty() {
                state.push_notice(
                    "未找到技能（~/.mcode/skills 或 .mcode/skills）",
                    NoticeKind::Info,
                );
            }
            for sk in skills {
                let desc: String = sk.description.trim().chars().take(60).collect();
                state.push_notice(format!("  /{} — {}", sk.name, desc), NoticeKind::Info);
            }
            state.push_notice("提示：直接输入 /<技能名> 调用技能", NoticeKind::Info);
        }
        "provider" => {
            let ps: Vec<String> = config
                .providers
                .iter()
                .map(|p| {
                    if Some(&p.name) == config.active_provider.as_ref() {
                        format!("● {}", p.name)
                    } else {
                        format!("○ {}", p.name)
                    }
                })
                .collect();
            state.push_notice(
                format!("providers（切换：/provider <name>）：{}", ps.join("  ")),
                NoticeKind::Info,
            );
        }
        "files" => {
            let mut seen: Vec<String> = vec![];
            if let Some(sess) = &rt.deps.session {
                if let Ok(lines) = mcode_core::sessions::read_lines(sess.path()) {
                    for l in lines {
                        if let mcode_core::sessions::Line::Snapshot(snap) = l {
                            let p = snap.path.display().to_string();
                            if !seen.contains(&p) {
                                seen.push(p);
                            }
                        }
                    }
                }
            }
            if seen.is_empty() {
                state.push_notice(
                    "本会话还没有文件快照（写入类工具才会产生）",
                    NoticeKind::Info,
                );
            } else {
                state.push_notice(
                    format!("本会话修改过的文件 {} 个：", seen.len()),
                    NoticeKind::Info,
                );
                for f in seen {
                    state.push_notice(format!("  {f}"), NoticeKind::Info);
                }
            }
        }
        "diff" => match commands::git_diff_stat(&rt.deps.tool_ctx.cwd) {
            Some(stat) => {
                for l in stat.lines() {
                    state.push_notice(l.to_string(), NoticeKind::Info);
                }
            }
            None => state.push_notice("无 git 变更（或当前目录不是 git 仓库）", NoticeKind::Info),
        },
        "save" => match &rt.deps.session {
            Some(s) => state.push_notice(
                format!("✓ 会话实时落盘中：{}", s.path().display()),
                NoticeKind::Success,
            ),
            None => state.push_notice("当前会话未启用持久化", NoticeKind::Warning),
        },
        "copy" => {
            let text = state
                .blocks
                .iter()
                .rev()
                .find_map(|b| match b {
                    Block::Assistant { text, .. } => Some(text.clone()),
                    _ => None,
                })
                .unwrap_or_default();
            if text.is_empty() {
                state.push_notice("暂无助手回复可复制", NoticeKind::Warning);
            } else {
                match commands::copy_to_clipboard(&text) {
                    Ok(_) => state.push_notice("✓ 已复制最后一条回复到剪贴板", NoticeKind::Success),
                    Err(e) => state.push_notice(format!("✗ 复制失败：{e:#}"), NoticeKind::Error),
                }
            }
        }
        "memory" => {
            let path = rt.deps.tool_ctx.cwd.join("MCODE.md");
            if !path.exists() {
                let _ = std::fs::write(&path, "");
            }
            let editor = std::env::var("EDITOR")
                .or_else(|_| std::env::var("VISUAL"))
                .unwrap_or_else(|_| {
                    if cfg!(windows) {
                        "notepad".into()
                    } else {
                        "vi".into()
                    }
                });
            // 挂起 TUI：退出 raw mode → 编辑器接管 → 恢复（动态区整体重印）
            let _ = disable_raw_mode();
            let status = std::process::Command::new(&editor).arg(&path).status();
            let _ = enable_raw_mode();
            term.reset_dynamic();
            match status {
                Ok(st) if st.success() => state.push_notice(
                    format!("✓ MCODE.md 已保存（{}）", path.display()),
                    NoticeKind::Success,
                ),
                _ => state.push_notice(format!("✗ 编辑器 {editor} 未正常退出"), NoticeKind::Error),
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// 会话入口
// ---------------------------------------------------------------------------

/// 欢迎屏（CC 式：`✻ Welcome` + 环境速览 + 快捷键提示）。
fn welcome_lines(rt: &SessionRuntime, cwd: &Path, resume_note: &str) -> Vec<StyledLine> {
    let plugin_note = {
        let names = rt.plugins.names();
        if names.is_empty() {
            String::new()
        } else {
            format!(" ｜ 插件: {}", names.join(", "))
        }
    };
    let mut out = vec![
        StyledLine::new(vec![
            Span::colored(Color::Claude, "✻"),
            Span::new(format!(" Welcome to mcode v{}", env!("CARGO_PKG_VERSION"))).bold(),
        ]),
        StyledLine::new(vec![Span::dim_text(format!(
            "  模型 {} · {}{plugin_note}",
            rt.deps.model,
            cwd.display()
        ))]),
        StyledLine::new(vec![Span::dim_text(
            "  /help 命令 · shift+tab 权限模式 · 双 esc 回退 · !cmd 跑 shell · ctrl+o 详情",
        )]),
    ];
    if !resume_note.is_empty() {
        out.push(StyledLine::new(vec![Span::dim_text(format!(
            "  {resume_note}"
        ))]));
    }
    out.push(StyledLine::blank());
    out
}

/// 单次会话的退出方式。
enum SessionExit {
    Quit,
    /// /resume <id>：退出当前运行时，重建后载入该会话。
    Switch(String),
}

/// TUI 入口（/resume 在会话间切换 = 重建运行时再进一轮）。
pub async fn run(model: Option<&str>, fallback: Option<Vec<String>>, resume: Resume) -> Result<()> {
    style::init_from_env();
    let mut resume = resume;
    loop {
        match run_session(model, fallback.clone(), resume.clone()).await? {
            SessionExit::Quit => return Ok(()),
            SessionExit::Switch(id) => resume = Resume::Id(id),
        }
    }
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

async fn run_session(
    model: Option<&str>,
    fallback: Option<Vec<String>>,
    resume: Resume,
) -> Result<SessionExit> {
    let cwd = std::env::current_dir()?;
    let config = mcode_core::config::Config::load()?;
    let desktop = mcode_core::config::DesktopConfig::load()?;
    let rt = Arc::new(build_runtime(&cwd, &config, &desktop, model, fallback, None).await?);

    let (etx, mut erx) = mpsc::unbounded_channel::<AgentEvent>();

    // 恢复会话：Fresh 起新会话；Continue/Id 加载并重建 transcript；Pick 列清单即出。
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
            println!("本项目会话（用 `mcode --resume <id>` 打开）:");
            for s in &list {
                let id8: String = s.session_id.chars().take(12).collect();
                let title: String = s
                    .title
                    .as_deref()
                    .unwrap_or("（无标题）")
                    .chars()
                    .take(40)
                    .collect();
                println!("  {id8}  「{title}」  {} 条消息", s.message_count);
            }
            if list.is_empty() {
                println!("（本项目暂无历史会话）");
            }
            return Ok(SessionExit::Quit);
        }
    };

    let mut term = Term::enable()?;
    let mut state = TuiState::new(&rt.deps.model);
    let mut ctl = SessionCtl {
        mode: config.default_mode,
        ..Default::default()
    };
    rt.deps.perms.lock().await.set_mode(ctl.mode);

    // 欢迎屏直接冻结打印；重放块进 state（首帧自然冻结到 scrollback）
    term.freeze(&welcome_lines(&rt, &cwd, &resume_note))?;
    let replay = replay_blocks(&conv);
    state.blocks.extend(replay);

    let conv = Arc::new(Mutex::new(conv));
    let slash_table = slash_command_table(&rt);
    ctl.context_pct = {
        let g = conv.lock().await;
        context_pct_of(&g, &rt.deps.model)
    };

    // 待发起的回合（队列出队后在这里落地）：(transcript 展示名, 发送载荷)
    let mut pending: Option<(String, String)> = None;
    let mut confirm_dlg: Option<ConfirmDialog> = None;
    let mut confirm_responder: Option<tokio::sync::oneshot::Sender<Decision>> = None;
    let mut completion: Option<CompletionState> = None;
    let mut input = InputBuffer::new();
    let mut rewind_sel: Option<usize> = None;
    let mut last_esc: Option<Instant> = None;
    let mut last_ctrl_c: Option<Instant> = None;
    let mut frame_tick: u64 = 0;
    let mut frozen_upto: usize = 0;
    // goal 裁判 verdict 通道（判官任务在回合 Done 后 spawn，结果回流主循环）
    let (gtx, mut grx) = mpsc::unbounded_channel::<bool>();
    // 后台任务轮询：上次状态快照 + 分频计数（POLL_MS 粒度凑出 ~1s）
    let mut task_seen: BTreeMap<String, AgentTaskStatus> = BTreeMap::new();
    let mut poll_tick: u32 = 0;

    // 退出值由 loop 的 break 携带；/resume 切换走循环后的 return
    let exit: SessionExit = loop {
        frame_tick = frame_tick.wrapping_add(1);

        // 1) 回合事件 → 状态（Done 由回合契约保证必达，收尾 + 续发队列）
        while let Ok(e) = erx.try_recv() {
            if let AgentEvent::Usage(u) = &e {
                ctl.tokens_in += u.input_tokens;
                ctl.tokens_out += u.output_tokens;
            }
            let done_reply = match &e {
                AgentEvent::Done { reply } => Some(reply.clone()),
                _ => None,
            };
            state.apply(e);
            if let Some(reply) = done_reply {
                // 上下文占比刷新（回合一结束就反映新水位）
                ctl.context_pct = {
                    let g = conv.lock().await;
                    context_pct_of(&g, &rt.deps.model)
                };
                // 队列续发
                if pending.is_none() {
                    pending = state.queue.pop_front();
                }
                // goal 外层循环：回合结束问裁判（esc 打断的空回复不算熔断）
                if let Some(g) = ctl.goal.clone() {
                    let cancelled = *rt.deps.tool_ctx.cancel.borrow();
                    if reply.trim().is_empty() && !cancelled {
                        // 空回复熔断：模型无产出还硬循环 = 烧钱不打粮食
                        ctl.goal = None;
                        state.push_notice(
                            "🎯 goal 中止：模型空回复（防失控熔断，可重新 /goal 开启）",
                            NoticeKind::Warning,
                        );
                    } else {
                        let model = state.model.clone();
                        let make_backend = rt.deps.make_backend.clone();
                        let gtx = gtx.clone();
                        let cancel_rx = rt.deps.cancel_tx.subscribe();
                        let objective = g.objective.clone();
                        tokio::spawn(async move {
                            let verdict = match make_backend(&model) {
                                Ok(b) => {
                                    mcode_core::agent::goal::check_goal_complete(
                                        b.as_ref(),
                                        &model,
                                        &objective,
                                        cancel_rx,
                                    )
                                    .await
                                }
                                Err(_) => false,
                            };
                            let _ = gtx.send(verdict);
                        });
                    }
                }
            }
        }

        // 1.4) goal 裁判 verdict：达成收官 / 否决续跑（轮数上限熔断）
        while let Ok(accepted) = grx.try_recv() {
            let Some(g) = ctl.goal.clone() else {
                continue;
            };
            if accepted {
                ctl.goal = None;
                state.push_notice("🎯 goal 已达成", NoticeKind::Success);
            } else if g.round + 1 >= mcode_core::agent::goal::MAX_GOAL_ROUNDS {
                ctl.goal = None;
                state.push_notice(
                    format!(
                        "🎯 goal 已达 {} 轮上限，循环终止",
                        mcode_core::agent::goal::MAX_GOAL_ROUNDS
                    ),
                    NoticeKind::Warning,
                );
            } else {
                ctl.goal = Some(GoalState {
                    round: g.round + 1,
                    ..g
                });
                state.push_notice(
                    format!("🎯 goal 第 {} 轮", ctl.goal.as_ref().unwrap().round),
                    NoticeKind::Info,
                );
                let payload = mcode_core::agent::goal::build_goal_prompt(
                    &ctl.goal.as_ref().unwrap().objective,
                );
                let entry = ("🎯 goal 续跑".to_string(), payload);
                if state.busy {
                    state.queue.push_back(entry);
                } else {
                    pending = Some(entry);
                }
            }
        }

        // 1.5) 权限确认请求 → 弹窗（运行中随时可能到达）
        {
            let mut rx = rt.confirm_rx.lock().await;
            while let Ok(req) = rx.try_recv() {
                confirm_dlg = Some(ConfirmDialog::new(
                    req.tool.clone(),
                    req.args_summary.clone(),
                ));
                confirm_responder = Some(req.respond);
            }
        }

        // 1.6) 后台任务状态轮询（终态迁移 → 通知）
        poll_tick = poll_tick.wrapping_add(1);
        if poll_tick as u64 >= 1000 / POLL_MS {
            poll_tick = 0;
            // std Mutex：中毒 = 有任务线程 panic，跳过本轮轮询即可
            if let Ok(table) = rt.task_table.lock() {
                poll_task_table(&table, &mut task_seen, &mut state);
            }
        }

        // 2) 发起回合（不 busy 时；expand 失败只报错不进模型）
        if let Some((display, payload)) = pending.take() {
            if state.busy {
                state.queue.push_back((display, payload));
            } else {
                let expanded_prompt = match expand_prompt(&rt, &payload) {
                    Ok(p) => p,
                    Err(msg) => {
                        state.push_notice(msg, NoticeKind::Error);
                        continue;
                    }
                };
                let emit: mcode_core::agent::EventSink = {
                    let etx = etx.clone();
                    Arc::new(move |e| {
                        let _ = etx.send(e);
                    })
                };
                state.begin_turn();
                state.push_user(&display);
                let block_index = state.blocks.len() - 1;
                // checkpoint：TUI 自己 push 拿消息 id（此刻无回合持锁）
                let (cp_id, parent) = {
                    let mut guard = conv.lock().await;
                    let parent = guard.leaf_id.clone();
                    let cp_id = guard.push(Message::user_text(&expanded_prompt));
                    rt.deps
                        .record_message(guard.messages.last().expect("刚 push 必有"));
                    (cp_id, parent)
                };
                ctl.checkpoints.push(Checkpoint {
                    id: cp_id,
                    parent,
                    prompt: display.clone(),
                    block_index,
                });
                let rt2 = rt.clone();
                let conv2 = conv.clone();
                let model_override = Some(state.model.clone());
                let effort = ctl.effort;
                tokio::spawn(async move {
                    rt2.deps.cancel_tx.send_replace(false);
                    let mut guard = conv2.lock().await;
                    run_prepared(&rt2, &mut guard, model_override.as_deref(), effort, emit).await;
                });
            }
        }

        // 3) 冻结新定稿的块 → scrollback（先于动态区重绘）
        let (width, _h) = term.refresh_size();
        let boundary = state.frozen_boundary().min(state.blocks.len());
        if boundary > frozen_upto {
            let mut lines: Vec<StyledLine> = Vec::new();
            for b in &state.blocks[frozen_upto..boundary] {
                lines.extend(render::render_frozen(b, state.detail_view, width));
            }
            term.freeze(&lines)?;
            frozen_upto = boundary;
        }

        // 4) 键盘（弹窗模态最高优先）
        if event::poll(Duration::from_millis(POLL_MS))? {
            if let Event::Key(k) = event::read()? {
                if k.kind == crossterm::event::KeyEventKind::Press {
                    let key = translate_key(k);
                    if let Some(d) = confirm_dlg.as_mut() {
                        // 权限确认模态：全部按键拦截
                        match key {
                            KeyKind::CtrlC => {
                                if let Some(tx) = confirm_responder.take() {
                                    let _ = tx.send(Decision::Deny);
                                }
                                confirm_dlg = None;
                            }
                            other => {
                                if let Some(decision) = d.on_key(other) {
                                    let tool = d.tool.clone();
                                    match decision {
                                        Decision::Allow => state.push_notice(
                                            format!("已允许 {tool}"),
                                            NoticeKind::Success,
                                        ),
                                        Decision::AllowAlways => {
                                            state.push_notice(
                                                format!("本会话总是允许 {tool}（已持久化）"),
                                                NoticeKind::Success,
                                            );
                                            if let Err(e) = persist_allow_always(&tool) {
                                                state.push_notice(
                                                    format!("规则落盘失败：{e:#}"),
                                                    NoticeKind::Error,
                                                );
                                            }
                                        }
                                        Decision::Deny => state.push_notice(
                                            format!("已拒绝 {tool}"),
                                            NoticeKind::Warning,
                                        ),
                                    }
                                    if let Some(tx) = confirm_responder.take() {
                                        let _ = tx.send(decision);
                                    }
                                    confirm_dlg = None;
                                }
                            }
                        }
                    } else if rewind_sel.is_some() {
                        // 回退列表模态
                        match key {
                            KeyKind::CtrlC => ctl.should_quit = true,
                            KeyKind::Up | KeyKind::ShiftTab => {
                                let len = ctl.checkpoints.len();
                                if len > 0 {
                                    let sel = rewind_sel.as_mut().unwrap();
                                    *sel = (*sel as i32 - 1).clamp(0, len as i32 - 1) as usize;
                                }
                            }
                            KeyKind::Down | KeyKind::Tab => {
                                let len = ctl.checkpoints.len();
                                if len > 0 {
                                    let sel = rewind_sel.as_mut().unwrap();
                                    *sel = (*sel as i32 + 1).clamp(0, len as i32 - 1) as usize;
                                }
                            }
                            KeyKind::Enter => {
                                let cp = rewind_sel
                                    .as_ref()
                                    .and_then(|sel| {
                                        ctl.checkpoints
                                            .len()
                                            .checked_sub(1 + *sel)
                                            .and_then(|i| ctl.checkpoints.get(i))
                                    })
                                    .cloned();
                                rewind_sel = None;
                                if let Some(cp) = cp {
                                    apply_rewind(
                                        &rt,
                                        &conv,
                                        &mut state,
                                        &mut ctl,
                                        &mut frozen_upto,
                                        &cp,
                                    )
                                    .await;
                                }
                            }
                            KeyKind::Esc => rewind_sel = None,
                            _ => {}
                        }
                    } else if completion.is_some() {
                        // 补全菜单：↑↓ 选候选，Tab/Enter 接受，Esc 关闭；
                        // 其余按键照常编辑并刷新候选
                        match key {
                            KeyKind::CtrlC => handle_ctrl_c(&mut input, &mut ctl, &mut last_ctrl_c),
                            KeyKind::Up => {
                                if let Some(c) = completion.as_mut() {
                                    c.up();
                                }
                            }
                            KeyKind::Down => {
                                if let Some(c) = completion.as_mut() {
                                    c.down();
                                }
                            }
                            KeyKind::Tab | KeyKind::Enter => {
                                let keep = completion
                                    .as_ref()
                                    .map(|c| c.accept(&mut input))
                                    .unwrap_or(false);
                                completion = if keep {
                                    CompletionState::refresh(&input, &slash_table, Path::new(&cwd))
                                } else {
                                    None
                                };
                            }
                            KeyKind::Esc => completion = None,
                            other => {
                                if edit_input(&mut input, other) {
                                    completion = CompletionState::refresh(
                                        &input,
                                        &slash_table,
                                        Path::new(&cwd),
                                    );
                                }
                            }
                        }
                    } else {
                        match key {
                            KeyKind::CtrlC => handle_ctrl_c(&mut input, &mut ctl, &mut last_ctrl_c),
                            KeyKind::CtrlD => {
                                if input.is_empty() {
                                    ctl.should_quit = true;
                                } else {
                                    input.delete();
                                    completion = CompletionState::refresh(
                                        &input,
                                        &slash_table,
                                        Path::new(&cwd),
                                    );
                                }
                            }
                            KeyKind::CtrlO => state.detail_view = !state.detail_view,
                            KeyKind::ShiftTab => {
                                ctl.mode = ctl.mode.next();
                                rt.deps.perms.lock().await.set_mode(ctl.mode);
                            }
                            KeyKind::Esc => {
                                let now = Instant::now();
                                let is_double = last_esc
                                    .map(|t| {
                                        now.duration_since(t)
                                            <= Duration::from_millis(DOUBLE_ESC_MS)
                                    })
                                    .unwrap_or(false);
                                last_esc = Some(now);
                                if is_double {
                                    if state.busy {
                                        state.push_notice(
                                            "回合进行中，先 esc 打断再回退",
                                            NoticeKind::Warning,
                                        );
                                    } else if ctl.checkpoints.is_empty() {
                                        state.push_notice("暂无可回退的检查点", NoticeKind::Info);
                                    } else {
                                        rewind_sel = Some(0);
                                    }
                                } else if state.busy {
                                    let _ = rt.deps.cancel_tx.send(true);
                                    state.interrupt_current();
                                    if ctl.goal.is_some() {
                                        state.push_notice(
                                            "正在打断…（goal 循环继续，输入 /goal 取消）",
                                            NoticeKind::Warning,
                                        );
                                    } else {
                                        state.push_notice("正在打断…", NoticeKind::Info);
                                    }
                                } else {
                                    input.clear();
                                    completion = None;
                                }
                            }
                            KeyKind::Enter => {
                                // 行尾 `\` + Enter = 软换行
                                if input.buf[..input.cursor].ends_with('\\') {
                                    input.buf.replace_range(input.cursor - 1..input.cursor, "");
                                    input.cursor -= 1;
                                    input.newline();
                                    completion = CompletionState::refresh(
                                        &input,
                                        &slash_table,
                                        Path::new(&cwd),
                                    );
                                } else {
                                    let text = input.text().trim().to_string();
                                    input.submit();
                                    completion = None;
                                    if !text.is_empty() {
                                        match classify_input(&text, &mut state, &mut ctl) {
                                            Some(Action::Send(payload)) => {
                                                let entry = (text, payload);
                                                if state.busy {
                                                    state.queue.push_back(entry);
                                                } else {
                                                    pending = Some(entry);
                                                }
                                            }
                                            Some(Action::Prompt { display, payload }) => {
                                                let entry = (display, payload);
                                                if state.busy {
                                                    state.queue.push_back(entry);
                                                } else {
                                                    pending = Some(entry);
                                                }
                                            }
                                            Some(Action::Bash(cmd)) => {
                                                state.push_user(&text);
                                                spawn_shell(
                                                    etx.clone(),
                                                    Arc::new(rt.deps.tool_ctx.cwd.clone()),
                                                    cmd,
                                                );
                                            }
                                            Some(Action::Runtime(cmd)) => {
                                                handle_runtime(
                                                    &rt, &conv, &mut state, &mut ctl, &etx, cmd,
                                                )
                                                .await;
                                            }
                                            Some(Action::Local(name)) => {
                                                handle_local(
                                                    &rt,
                                                    &conv,
                                                    &mut state,
                                                    &mut ctl,
                                                    &config,
                                                    &mut term,
                                                    &mut frozen_upto,
                                                    &mut rewind_sel,
                                                    name,
                                                )
                                                .await;
                                            }
                                            None => {}
                                        }
                                    }
                                }
                            }
                            KeyKind::AltEnter | KeyKind::CtrlJ => input.newline(),
                            KeyKind::Up => {
                                if !input.up() {
                                    input.history_prev();
                                }
                            }
                            KeyKind::Down => {
                                if !input.down() {
                                    input.history_next();
                                }
                            }
                            other => {
                                if matches!(other, KeyKind::Char('?')) && input.is_empty() {
                                    for l in render_help() {
                                        state.push_notice(l, NoticeKind::Info);
                                    }
                                } else if edit_input(&mut input, other) {
                                    completion = CompletionState::refresh(
                                        &input,
                                        &slash_table,
                                        Path::new(&cwd),
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }

        // 5) 绘制动态区 + 退出判定
        let mode_label = ctl.mode.label().to_string();
        let bash_mode = input.text().starts_with('!');
        let rewind_data = rewind_sel.as_ref().map(|sel| {
            let items: Vec<String> = ctl
                .checkpoints
                .iter()
                .rev()
                .map(|c| truncate_chars(&c.prompt, 60))
                .collect();
            (items, *sel)
        });
        let ctx = FrameCtx {
            state: &state,
            input: &input,
            completion: completion.as_ref(),
            confirm: confirm_dlg.as_ref(),
            bash_mode,
            mode: &mode_label,
            context_pct: ctl.context_pct,
            rewind: rewind_data
                .as_ref()
                .map(|(items, sel)| (items.as_slice(), *sel)),
            width,
            frame_tick,
        };
        let frame = render::render_frame(&ctx);
        term.draw_dynamic(&frame.lines, frame.cursor)?;

        if ctl.should_quit {
            break SessionExit::Quit;
        }
    };

    // 告别行（随机一条，CC 式收尾）
    let bye = GOODBYES[spinner::time_nonce() % GOODBYES.len()];
    let _ = term.freeze(&[StyledLine::new(vec![Span::colored(Color::Dim, bye)])]);
    term.restore();

    // /resume 切换：带上目标 id 退出，外层重建运行时再进
    if let Some(id) = ctl.resume_request.take() {
        return Ok(SessionExit::Switch(id));
    }
    Ok(exit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcode_core::message::Message;

    #[test]
    fn replay_extracts_active_path_blocks_only() {
        let mut conv = Conversation::new();
        conv.push(Message::user_text("第一问"));
        conv.push(Message::assistant_text("第一答"));
        let id2 = conv.push(Message::user_text("第二问"));

        let blocks = replay_blocks(&conv);
        assert_eq!(blocks.len(), 3);
        assert!(matches!(&blocks[0], Block::User { text } if text == "第一问"));
        assert!(matches!(&blocks[1], Block::Assistant { .. }));

        // rewind 后只重放活跃路径
        conv.rewind_before(&id2);
        let blocks = replay_blocks(&conv);
        assert_eq!(blocks.len(), 2, "被回退的消息不重放");
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

    #[test]
    fn crossterm_keys_translate_to_keykinds() {
        use crossterm::event::{KeyCode, KeyEventKind, KeyEventState};
        let k = |code: KeyCode, mods: KeyModifiers| KeyEvent {
            code,
            modifiers: mods,
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        };
        let none = KeyModifiers::empty();
        assert!(matches!(
            translate_key(k(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            KeyKind::CtrlC
        ));
        assert!(matches!(
            translate_key(k(KeyCode::Char('c'), none)),
            KeyKind::Char('c')
        ));
        assert!(matches!(
            translate_key(k(KeyCode::BackTab, none)),
            KeyKind::ShiftTab
        ));
        assert!(matches!(
            translate_key(k(KeyCode::Tab, KeyModifiers::SHIFT)),
            KeyKind::ShiftTab
        ));
        assert!(matches!(translate_key(k(KeyCode::Tab, none)), KeyKind::Tab));
        assert!(matches!(
            translate_key(k(KeyCode::Enter, KeyModifiers::ALT)),
            KeyKind::AltEnter
        ));
        assert!(matches!(
            translate_key(k(KeyCode::Enter, none)),
            KeyKind::Enter
        ));
        assert!(matches!(translate_key(k(KeyCode::Esc, none)), KeyKind::Esc));
        assert!(matches!(
            translate_key(k(KeyCode::Char('中'), none)),
            KeyKind::Char('中')
        ));
    }

    #[test]
    fn classify_input_routes_bash_slash_and_plain() {
        let mut s = TuiState::new("m");
        let mut c = SessionCtl::default();
        assert_eq!(
            classify_input("你好", &mut s, &mut c),
            Some(Action::Send("你好".into()))
        );
        assert_eq!(
            classify_input("!git status", &mut s, &mut c),
            Some(Action::Bash("git status".into()))
        );
        assert_eq!(classify_input("   ", &mut s, &mut c), None, "空白忽略");
        assert_eq!(
            classify_input("/context", &mut s, &mut c),
            Some(Action::Local("context"))
        );
    }

    #[test]
    fn slash_dispatch_help_lists_usage_and_purpose() {
        let mut s = TuiState::new("m");
        let mut c = SessionCtl::default();
        let a = dispatch_slash(&mut s, &mut c, "help", "");
        assert!(matches!(a, Action::Local("help")));
        let joined = s
            .blocks
            .iter()
            .filter_map(|b| match b {
                Block::Notice { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        // 每条指令的提示都带用法+作用
        assert!(joined.contains("/model [name]"), "{joined}");
        assert!(joined.contains("查看或切换模型"), "{joined}");
        assert!(joined.contains("/goal <objective>"), "{joined}");
        assert!(joined.contains("/compact [instructions]"), "{joined}");
        // /help <cmd> 单条
        let a = dispatch_slash(&mut s, &mut c, "help", "model");
        assert!(matches!(a, Action::Local("help")));
        let last = match s.blocks.last().unwrap() {
            Block::Notice { text, .. } => text.clone(),
            other => panic!("{other:?}"),
        };
        assert!(last.contains("查看或切换模型"), "{last}");
    }

    #[test]
    fn goal_dispatch_starts_and_cancels() {
        let mut s = TuiState::new("m");
        let mut c = SessionCtl::default();
        let a = dispatch_slash(&mut s, &mut c, "goal", "把测试修绿");
        match a {
            Action::Prompt { display, payload } => {
                assert_eq!(display, "/goal 把测试修绿");
                assert!(payload.contains("[goal] 把测试修绿"));
                assert!(payload.contains("GOAL_COMPLETE"));
            }
            other => panic!("应为 Prompt，实际 {other:?}"),
        }
        assert_eq!(c.goal.as_ref().unwrap().objective, "把测试修绿");

        // 空参 = 取消
        let a = dispatch_slash(&mut s, &mut c, "goal", "");
        assert!(matches!(a, Action::Local("goal")));
        assert!(c.goal.is_none(), "循环已取消");
    }

    #[test]
    fn runtime_cmds_carry_args() {
        let mut s = TuiState::new("m");
        let mut c = SessionCtl::default();
        assert_eq!(
            dispatch_slash(&mut s, &mut c, "compact", "保留测试相关内容"),
            Action::Runtime(RuntimeCmd::Compact(Some("保留测试相关内容".into())))
        );
        assert_eq!(
            dispatch_slash(&mut s, &mut c, "compact", ""),
            Action::Runtime(RuntimeCmd::Compact(None))
        );
        assert_eq!(
            dispatch_slash(&mut s, &mut c, "resume", ""),
            Action::Runtime(RuntimeCmd::Resume(None))
        );
        assert_eq!(
            dispatch_slash(&mut s, &mut c, "resume", "abc123"),
            Action::Runtime(RuntimeCmd::Resume(Some("abc123".into())))
        );
        assert_eq!(
            dispatch_slash(&mut s, &mut c, "rename", "新标题"),
            Action::Runtime(RuntimeCmd::Rename("新标题".into()))
        );
        assert_eq!(
            dispatch_slash(&mut s, &mut c, "export", "out.md"),
            Action::Runtime(RuntimeCmd::Export(Some("out.md".into())))
        );
        // 别名：exit → quit；? → help
        assert_eq!(
            dispatch_slash(&mut s, &mut c, "exit", ""),
            Action::Local("quit")
        );
        assert_eq!(
            dispatch_slash(&mut s, &mut c, "?", ""),
            Action::Local("help")
        );
    }

    #[test]
    fn model_switch_via_slash() {
        let mut s = TuiState::new("glm-5.3");
        let mut c = SessionCtl::default();
        assert!(matches!(
            dispatch_slash(&mut s, &mut c, "model", ""),
            Action::Local("model")
        ));
        assert_eq!(s.model, "glm-5.3", "无参只展示");
        assert!(matches!(
            dispatch_slash(&mut s, &mut c, "model", "deepseek-chat"),
            Action::Local("model")
        ));
        assert_eq!(s.model, "deepseek-chat");
    }

    #[test]
    fn effort_dispatch_sets_levels() {
        let mut s = TuiState::new("m");
        let mut c = SessionCtl::default();
        dispatch_slash(&mut s, &mut c, "effort", "high");
        assert_eq!(c.effort, Some(mcode_core::api::ThinkingEffort::High));
        dispatch_slash(&mut s, &mut c, "effort", "");
        assert_eq!(c.effort, Some(mcode_core::api::ThinkingEffort::High));
        dispatch_slash(&mut s, &mut c, "effort", "bogus");
        assert_eq!(
            c.effort,
            Some(mcode_core::api::ThinkingEffort::High),
            "坏参数不改值"
        );
        dispatch_slash(&mut s, &mut c, "effort", "low");
        assert_eq!(c.effort, Some(mcode_core::api::ThinkingEffort::Low));
    }

    #[test]
    fn edit_input_handles_emacs_keys() {
        let mut b = InputBuffer::new();
        b.set_text("foo bar baz");
        edit_input(&mut b, KeyKind::CtrlW);
        assert_eq!(b.text(), "foo bar ", "ctrl+w 删一个词");
        edit_input(&mut b, KeyKind::CtrlU);
        assert_eq!(b.text(), "", "ctrl+u 清到行首");
        b.set_text("hello");
        edit_input(&mut b, KeyKind::CtrlA);
        assert_eq!(b.cursor, 0);
        edit_input(&mut b, KeyKind::CtrlK);
        assert_eq!(b.text(), "", "ctrl+k 删到行尾");
        b.set_text("ab\ncd");
        b.cursor = b.buf.len();
        edit_input(&mut b, KeyKind::CtrlU);
        assert_eq!(b.text(), "ab\n", "ctrl+u 只清当前行（行作用域）");
        assert_eq!(b.cursor, 3);
        assert!(!edit_input(&mut b, KeyKind::Other), "未知键不动缓冲");
    }
}
