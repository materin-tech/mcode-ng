// MCode Desktop — AI coding agent desktop app
// Copyright (c) 2026 Materin-tech. All rights reserved.
// SPDX-License-Identifier: MIT

//! 指令系统与会话操作 — slash 指令本地解释（不进模型回合）、会话列表/恢复、
//! 设置类操作（模型/语言/模式/PUA 切换并重建 harness）、快照导出。

use mcode_core::agent::compact::compact_conversation;
use mcode_core::agent::context_window_for_model;
use mcode_core::config::Language;
use mcode_core::message::{ContentBlock, Role};
use mcode_core::token::raw_estimate_messages;
use mcode_core::util::mcode_dir;

use crate::app::App;
use crate::app::View;
use crate::app::assets::{init_mcode_doc, mcode_md_paths};
use crate::app::extension_manage::session_id_of;
use crate::app::goal::MAX_GOAL_ROUNDS;
use crate::app::msg::{ChatMsg, default_fold_for, trunc};
use crate::app::runtime::event_sink;
use std::sync::Arc;

/// 语言循环顺序（设置视图 / /language 用）
pub const LANGS: [Language; 5] = [
    Language::ZhCn,
    Language::ZhTw,
    Language::EnUs,
    Language::JaJp,
    Language::KoKr,
];

pub fn lang_label(l: Language) -> &'static str {
    match l {
        Language::ZhCn => "简体中文 zh_CN",
        Language::ZhTw => "繁體中文 zh_TW",
        Language::EnUs => "English en_US",
        Language::JaJp => "日本語 ja_JP",
        Language::KoKr => "한국어 ko_KR",
    }
}

pub fn lang_next(l: Language) -> Language {
    let i = LANGS.iter().position(|x| *x == l).unwrap_or(0);
    LANGS[(i + 1) % LANGS.len()]
}

impl App {
    // ---------- 通用小工具 ----------

    /// 推一条 Notice 消息
    pub fn push_notice(&mut self, text: String, error: bool) {
        self.messages.push(ChatMsg::Notice {
            text: text.into(),
            error,
        });
    }

    /// 刷新会话列表（当前 cwd 的会话桶）
    pub fn refresh_sessions(&mut self) {
        self.sessions = self.store.as_ref().map(|s| s.list()).unwrap_or_default();
    }

    /// 刷新上下文占用缓存（conv 锁空闲时才更新；回合运行中渲染沿用缓存）
    pub fn refresh_ctx(&mut self) {
        if let Ok(h) = self.hist.try_lock() {
            let model = self
                .harness
                .as_ref()
                .map(|h| h.deps.current_model())
                .unwrap_or_default();
            let used = raw_estimate_messages(&h.active_messages());
            let _ = model;
            self.ctx_tokens = used;
        }
    }

    /// 新建会话：清消息/队列/历史（goal 保持）。
    /// 会话在 harness 装配时创建并出现在列表。防垃圾条目：当前会话还没有
    /// 任何主链消息时复用现有 harness，不重复建。
    pub fn new_chat(&mut self, cx: &mut gpui::Context<Self>) {
        self.messages.clear();
        self.chat_follow = true;
        self.chat_last_down_y = None;
        self.queue.clear();
        let conv_empty = self
            .hist
            .try_lock()
            .map(|h| h.active_messages().is_empty())
            .unwrap_or(true);
        self.running = false;
        if !conv_empty {
            if let Ok(mut h) = self.hist.try_lock() {
                *h = mcode_core::message::Conversation::new();
            }
            // 重建 harness = 新会话文件 + 新对话（旧泵随 ui_tx 关闭自然退出）
            self.rebuild_harness(cx);
        }
        self.refresh_sessions();
        // 可见反馈：空会话下点击「新建会话」也有变化（用户实测反馈）
        self.push_notice("✨ 已新建会话，开始提问吧".to_string(), false);
        cx.notify();
    }

    /// 权限模式循环（真值在 harness.deps.perms；锁被持有时退化为本地缓存循环）
    pub fn cycle_mode(&mut self) {
        self.mode = match self.harness.as_ref().map(|h| h.deps.perms.try_lock()) {
            Some(Ok(mut p)) => p.cycle_mode(),
            _ => self.mode.next(),
        };
    }

    // ---------- 指令系统（返回 true = 本地已解释，不进模型回合） ----------

    pub fn handle_command(&mut self, text: &str, cx: &mut gpui::Context<Self>) -> bool {
        let (cmd, arg) = text.split_once(' ').unwrap_or((text, ""));
        let cmd = cmd.trim().to_lowercase();
        let arg = arg.trim();
        match cmd.as_str() {
            "/help" => {
                // 与 CLI 同源：每条指令展示「用法 — 作用」（共享 core 目录）
                let list: String = mcode_core::commands::COMMANDS
                    .iter()
                    .map(|c| format!("{}  {}", c.usage, c.desc))
                    .collect::<Vec<_>>()
                    .join("\n");
                self.push_notice(
                    format!("可用指令（/help <command> 看单条详情）：\n{list}"),
                    false,
                );
            }
            "/status" => {
                self.push_notice(
                    format!(
                        "MCode Desktop v{} · 模型 {} · cwd {} · 模式 {} · tokens {}",
                        env!("CARGO_PKG_VERSION"),
                        self.cfg.active_model().unwrap_or_default(),
                        self.cwd.display(),
                        self.mode.label(),
                        self.tokens_total
                    ),
                    false,
                );
            }
            "/doctor" => {
                let ready = self.cfg.provider_ready().map(|_| "OK").unwrap_or("异常");
                let key = self
                    .cfg
                    .active_provider()
                    .and_then(|p| p.resolve_api_key())
                    .map(|_| "OK")
                    .unwrap_or("缺失");
                self.push_notice(
                    format!(
                        "诊断：provider={ready} · api_key={key} · cwd={}",
                        self.cwd.display()
                    ),
                    false,
                );
            }
            "/token" => {
                self.push_notice(
                    format!("累计 token：{}（input+output）", self.tokens_total),
                    false,
                );
            }
            "/context" => {
                let window = self
                    .harness
                    .as_ref()
                    .map(|h| context_window_for_model(&h.deps.current_model()))
                    .unwrap_or(0);
                self.push_notice(
                    format!(
                        "上下文估算 ~{} / {window} tokens（{}%）",
                        self.ctx_tokens,
                        self.ctx_tokens * 100 / window.max(1)
                    ),
                    false,
                );
            }
            "/model" => {
                if arg.is_empty() {
                    let models = self
                        .cfg
                        .active_provider()
                        .map(|p| p.models.join("  "))
                        .unwrap_or_else(|| "（未配置供应商）".into());
                    self.push_notice(format!("可选模型：{models}（/model <name> 切换）"), false);
                } else if self.set_model(arg) {
                    self.push_notice(format!("✓ 模型 → {arg}"), false);
                } else {
                    self.push_notice(format!("✗ 模型 {arg} 不在当前供应商列表中"), true);
                }
            }
            "/language" => {
                if arg.is_empty() {
                    self.push_notice(
                        format!(
                            "当前语言：{}（/language <zh_CN|zh_TW|en_US|ja_JP|ko_KR>）",
                            lang_label(self.cfg.language)
                        ),
                        false,
                    );
                } else if let Some(l) = parse_language(arg) {
                    self.cfg.language = l;
                    self.save_cfg_and_rebuild(cx);
                    self.push_notice(format!("✓ 语言 → {}", lang_label(l)), false);
                } else {
                    self.push_notice(format!("✗ 无法识别语言 {arg}"), true);
                }
            }
            "/effort" => {
                if arg.is_empty() {
                    let cur = mcode_core::config::DesktopConfig::load()
                        .unwrap_or_default()
                        .thinking();
                    self.push_notice(
                        format!(
                            "当前思考深度：{}（可选 default/low/medium/high，/effort <level> 切换，下一回合生效）",
                            effort_label(cur)
                        ),
                        false,
                    );
                } else if let Some(level) = mcode_core::config::ThinkingEffort::parse(arg) {
                    let mut desktop = mcode_core::config::DesktopConfig::load().unwrap_or_default();
                    desktop.thinking_effort = effort_label(level).to_string();
                    desktop.save().ok();
                    self.push_notice(
                        format!("🧠 思考深度 → {}（下一回合生效）", effort_label(level)),
                        false,
                    );
                } else {
                    self.push_notice(
                        format!("✗ 无法识别思考深度 {arg}（可选 default/low/medium/high）"),
                        true,
                    );
                }
            }
            "/clear" => {
                self.messages.clear();
                self.chat_follow = true;
                self.chat_last_down_y = None;
                self.running = false;
                // 复用 new_chat 的防垃圾语义：空会话只清 UI；有内容则换新会话文件
                let had_content = !self
                    .hist
                    .try_lock()
                    .map(|h| h.active_messages().is_empty())
                    .unwrap_or(true);
                if had_content {
                    if let Ok(mut h) = self.hist.try_lock() {
                        *h = mcode_core::message::Conversation::new();
                    }
                    self.rebuild_harness(cx);
                }
                self.refresh_sessions();
            }
            "/compact" => {
                self.spawn_compact();
            }
            "/goal" => {
                if arg.is_empty() || arg == "clear" {
                    self.goal = None;
                    self.goal_rounds = 0;
                    self.push_notice("goal 已清除".to_string(), false);
                } else {
                    self.goal = Some(arg.to_string());
                    self.goal_rounds = 0;
                    self.push_notice(
                        format!(
                            "🎯 goal 已设置：{arg}（回合完成后自动推进，上限 {MAX_GOAL_ROUNDS} 轮）"
                        ),
                        false,
                    );
                }
            }
            "/agents" | "/skills" | "/mcp" | "/hooks" => {
                self.view = View::Extensions;
            }
            "/memory" => {
                let paths = mcode_md_paths(&self.cwd);
                let list = paths
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join("\n");
                self.push_notice(format!("记忆文件（nearest-first）：\n{list}"), false);
            }
            "/init" => match init_mcode_doc(&self.cwd) {
                Ok((path, created)) => {
                    if created {
                        self.push_notice(format!("✓ 已生成 {}", path.display()), false);
                    } else {
                        self.push_notice("MCODE.md 已存在，跳过生成".to_string(), false);
                    }
                }
                Err(e) => self.push_notice(format!("✗ 生成失败：{e}"), true),
            },
            "/save" => {
                self.save_snapshot();
            }
            "/resume" => {
                self.refresh_sessions();
                self.view = View::Sessions;
            }
            "/pua:on" => {
                self.cfg.pua_always_on = true;
                self.save_cfg_and_rebuild(cx);
                self.push_notice("✓ PUA 收敛纪律已开启（每会话常驻）".to_string(), false);
            }
            "/pua:off" => {
                self.cfg.pua_always_on = false;
                self.save_cfg_and_rebuild(cx);
                self.push_notice("✓ PUA 收敛纪律已关闭".to_string(), false);
            }
            "/config" => {
                self.view = View::Settings;
            }
            "/permissions" => {
                let desktop = mcode_core::config::DesktopConfig::load().unwrap_or_default();
                if desktop.permission_rules.is_empty() {
                    self.push_notice(
                        "权限规则为空（~/.mcode/desktop.json permissionRules）".to_string(),
                        false,
                    );
                } else {
                    let list = desktop
                        .permission_rules
                        .iter()
                        .map(|r| {
                            format!(
                                "{}({}) → {}",
                                r.tool,
                                r.pattern,
                                if r.allow { "ALLOW" } else { "DENY" }
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    self.push_notice(format!("权限规则（desktop.json）：\n{list}"), false);
                }
            }
            "/mode" => {
                if arg.is_empty() {
                    self.cycle_mode();
                    self.push_notice(format!("权限模式 → {}", self.mode.label()), false);
                } else {
                    match mcode_core::permission::Mode::from_label(&arg.to_lowercase()) {
                        Some(m) => {
                            self.mode = m;
                            if let Some(h) = self.harness.as_ref()
                                && let Ok(mut p) = h.deps.perms.try_lock()
                            {
                                p.set_mode(m);
                            }
                            self.push_notice(
                                format!("权限模式 → {}（{}）", m.label(), m.hint()),
                                false,
                            );
                        }
                        None => {
                            self.push_notice(
                                format!(
                                    "未知模式 {arg}；可用：manual（逐步确认）/ auto / plan / passon"
                                ),
                                true,
                            );
                        }
                    }
                }
            }
            "/exit" | "/quit" => {
                cx.quit();
            }
            // 其余 / 开头的输入视为 skill 调用，交给模型回合（与 CLI 一致）
            _ => return false,
        }
        true
    }

    /// spawn 手动压缩（rt 线程：compact_conversation 走 LLM 摘要，Compacted
    /// 事件经 sink 回流主线程；conv 锁忙 = 回合运行中，直接拒绝）
    fn spawn_compact(&mut self) {
        let Some(h) = self.harness.clone() else {
            self.push_notice("未初始化完成，无法压缩".to_string(), true);
            return;
        };
        // 先探测锁：回合运行中不压缩（等锁会跟下一回合的持锁互相顶）
        if self.hist.try_lock().is_err() {
            self.push_notice("回合运行中，稍后再压缩".to_string(), true);
            return;
        }
        let deps = h.deps.clone();
        let hist = self.hist.clone();
        let sink = event_sink(h.ui_tx.clone());
        self.rt.spawn(async move {
            let mut guard = hist.lock().await;
            // Compacted 事件由 compact_conversation 经 sink 发出；失败经 Error
            // 事件回流主线程（无 Running 占位时 UI 只展示提示，不动状态机）
            if let Err(e) = compact_conversation(&deps, &mut guard, &sink).await {
                sink(mcode_core::agent::AgentEvent::Error {
                    message: e.to_string(),
                });
            }
        });
    }

    // ---------- 配置操作（设置视图复用） ----------

    /// 切换 active_model（校验在当前供应商模型列表内）
    pub fn set_model(&mut self, model: &str) -> bool {
        let valid = self
            .cfg
            .active_provider()
            .map(|p| p.models.iter().any(|m| m == model))
            .unwrap_or(false);
        if !valid {
            return false;
        }
        self.cfg.active_model = Some(model.to_string());
        self.cfg.save().ok();
        true
    }

    /// 配置保存 + harness 重建（旧 harness drop 后确认泵随 ui_tx 关闭退出）
    pub fn save_cfg_and_rebuild(&mut self, cx: &mut gpui::Context<Self>) {
        self.cfg.save().ok();
        self.rebuild_harness(cx);
    }

    /// PUA 开关（设置视图）
    pub fn set_pua(&mut self, on: bool, cx: &mut gpui::Context<Self>) {
        self.cfg.pua_always_on = on;
        self.save_cfg_and_rebuild(cx);
    }

    /// 默认权限模式（设置视图：写配置，重建 harness 生效）
    pub fn set_default_mode(
        &mut self,
        mode: mcode_core::permission::Mode,
        cx: &mut gpui::Context<Self>,
    ) {
        self.cfg.default_mode = mode;
        self.save_cfg_and_rebuild(cx);
    }

    // ---------- 会话恢复 ----------

    /// 恢复历史会话：重建 harness（对话从 jsonl 活跃路径重建）+ 消息流重放。
    pub fn resume_session(&mut self, id: &str, cx: &mut gpui::Context<Self>) {
        let Some(store) = self.store.clone() else {
            self.push_notice("会话存储未初始化".to_string(), true);
            cx.notify();
            return;
        };
        match crate::app::runtime::build_harness_resumed(&self.cfg, &self.cwd, &store, id, &self.rt)
        {
            Ok((h, conv)) => {
                self.ext_skills = h.skills.all().into_iter().cloned().collect();
                self.ext_agents = h.agents.all().into_iter().cloned().collect();
                let harness = Arc::new(h);
                for w in &harness.mcp_report {
                    self.push_notice(format!("⚠ MCP：{w}"), true);
                }
                self.harness = Some(harness);
                self.hist = conv;
                self.attach_pump(cx);
            }
            Err(e) => {
                self.push_notice(format!("恢复会话失败：{e}"), true);
                cx.notify();
                return;
            }
        }
        // jsonl → ChatMsg 重放（主链活跃路径；synthetic 摘要渲染为折叠条）
        let mut chat = vec![];
        let mut tokens = 0u64;
        if let Ok(h) = self.hist.try_lock() {
            for m in h.active_messages() {
                if let Some(u) = &m.usage {
                    tokens += u.input_tokens + u.output_tokens;
                }
                let mut texts: Vec<String> = vec![];
                let mut tool_uses: Vec<(String, String)> = vec![];
                let mut tool_results: Vec<(String, bool)> = vec![];
                for c in &m.content {
                    match c {
                        ContentBlock::Text { text } => texts.push(text.clone()),
                        ContentBlock::ToolUse { name, input, .. } => {
                            tool_uses.push((name.clone(), input.to_string()))
                        }
                        ContentBlock::ToolResult {
                            content, is_error, ..
                        } => tool_results.push((content.clone(), !*is_error)),
                        _ => {}
                    }
                }
                if m.synthetic {
                    for text in texts {
                        chat.push(ChatMsg::Notice {
                            text: trunc(&text, 400).into(),
                            error: false,
                        });
                    }
                    continue;
                }
                for text in texts {
                    match m.role {
                        Role::User => chat.push(ChatMsg::User {
                            text: text.clone().into(),
                            refs: vec![],
                        }),
                        Role::Assistant => chat.push(ChatMsg::Assistant {
                            text: text.clone().into(),
                            fold: default_fold_for(&text),
                        }),
                    }
                }
                for (name, input) in tool_uses {
                    chat.push(ChatMsg::ToolCard {
                        name: name.into(),
                        args: trunc(&input, 200).into(),
                        summary: "".into(),
                        running: false,
                        success: true,
                    });
                }
                for (output, success) in tool_results {
                    chat.push(ChatMsg::Notice {
                        text: trunc(&output, 400).into(),
                        error: !success,
                    });
                }
            }
        }
        self.messages = chat;
        self.chat_follow = true;
        self.chat_last_down_y = None;
        self.tokens_total = tokens;
        self.running = false;
        self.queue.clear();
        self.refresh_ctx();
        self.view = View::Chat;
        cx.notify();
    }

    // ---------- /save 快照 ----------

    // ---------- 补全/上下文/输入历史（字段私有，供渲染层访问） ----------

    /// 上次缓存的上下文 token 估算（渲染用）
    pub fn ctx_tokens_display(&self) -> usize {
        self.ctx_tokens
    }

    pub(crate) fn history_recall_up(&mut self, current: &str) -> Option<String> {
        self.history.recall_up(current)
    }

    pub(crate) fn history_recall_down(&mut self) -> Option<String> {
        self.history.recall_down()
    }

    /// Onboarding 表单保存：构造 Config → save → build_harness → attach_pump → 进入工作区选择
    pub fn onboarding_save(&mut self, cx: &mut gpui::Context<Self>) {
        let name = self.ob_name.read(cx).text().trim().to_string();
        let url = self.ob_url.read(cx).text().trim().to_string();
        let key = self.ob_key.read(cx).text().trim().to_string();
        let models_raw = self.ob_models.read(cx).text();
        let models: Vec<String> = models_raw
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if name.is_empty() || url.is_empty() || models.is_empty() {
            self.push_notice("✗ 名称 / baseURL / 模型列表不能为空".to_string(), true);
            cx.notify();
            return;
        }
        let provider = mcode_core::config::Provider {
            name: name.clone(),
            base_url: url,
            api_key_env: key,
            models: models.clone(),
            multimodal_models: vec![],
            protocol: self.ob_protocol,
        };
        self.cfg.providers = vec![provider];
        self.cfg.active_provider = Some(name);
        self.cfg.active_model = Some(models[0].clone());
        self.cfg.save().ok();
        self.rebuild_harness(cx);
        if self.harness.is_some() {
            // 引导完成后进工作区选择器（IDEA 式首屏链路）
            self.view = View::WorkspacePicker;
            self.push_notice(
                format!(
                    "✓ 供应商已配置：{}（模型 {}）",
                    self.cfg.active_provider.clone().unwrap_or_default(),
                    models[0]
                ),
                false,
            );
        }
        cx.notify();
    }

    /// 把当前消息流渲染成 markdown 快照写 ~/.mcode/session-data/<date>-<id>.md
    pub fn save_snapshot(&mut self) {
        let Some(dir) = mcode_dir() else {
            self.push_notice("✗ ~/.mcode 目录不可用".to_string(), true);
            return;
        };
        let dir = dir.join("session-data");
        if let Err(e) = std::fs::create_dir_all(&dir) {
            self.push_notice(format!("✗ 快照目录创建失败：{e}"), true);
            return;
        }
        let id = self
            .harness
            .as_ref()
            .map(|h| session_id_of(&h.session))
            .unwrap_or_else(|| "adhoc".into());
        let date = chrono_now_date();
        let path = dir.join(format!("{date}-{id}.md"));
        let mut md = String::new();
        for m in &self.messages {
            match m {
                ChatMsg::User { text, .. } => md.push_str(&format!("## 用户\n\n{text}\n\n")),
                ChatMsg::Assistant { text, .. } => md.push_str(&format!("## MCode\n\n{text}\n\n")),
                ChatMsg::Thinking { text, .. } => {
                    md.push_str(&format!("> 💭 {}\n\n", trunc(text, 200)))
                }
                ChatMsg::ToolCard {
                    name,
                    args,
                    summary,
                    ..
                } => md.push_str(&format!("- 🔧 {name} `{args}` {summary}\n")),
                ChatMsg::Diff { path, .. } => md.push_str(&format!("- ✏️ diff {path}\n")),
                ChatMsg::Notice { text, error } => {
                    md.push_str(&format!("- {} {}\n", if *error { "⚠" } else { "ℹ" }, text))
                }
                ChatMsg::Running => {}
            }
        }
        match std::fs::write(&path, md) {
            Ok(_) => self.push_notice(format!("✓ 会话快照已写入 {}", path.display()), false),
            Err(e) => self.push_notice(format!("✗ 快照写入失败：{e}"), true),
        }
    }
}

/// 当前思考深度 → API 思考配置（launch_turn 每回合读 desktop.json）
pub(crate) fn current_effort() -> Option<mcode_core::api::ThinkingConfig> {
    mcode_core::config::DesktopConfig::load()
        .unwrap_or_default()
        .thinking()
        .thinking()
}

/// ThinkingEffort → desktop.json 持久化串（新核未提供 as_str，本地对齐）
fn effort_label(e: mcode_core::config::ThinkingEffort) -> &'static str {
    match e {
        mcode_core::config::ThinkingEffort::Default => "default",
        mcode_core::config::ThinkingEffort::Low => "low",
        mcode_core::config::ThinkingEffort::Medium => "medium",
        mcode_core::config::ThinkingEffort::High => "high",
    }
}

fn parse_language(s: &str) -> Option<Language> {
    match s.to_lowercase().as_str() {
        "zh_cn" => Some(Language::ZhCn),
        "zh_tw" => Some(Language::ZhTw),
        "en_us" => Some(Language::EnUs),
        "ja_jp" => Some(Language::JaJp),
        "ko_kr" => Some(Language::KoKr),
        _ => None,
    }
}

fn chrono_now_date() -> String {
    // chrono 已在内核依赖树内，但 mcode-ui 未直接依赖；用 std 时间推日期（UTC）
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    ms_to_date((secs * 1000) as i64)
}

/// 毫秒时间戳 → `YYYYMMDD`（UTC；会话列表/侧栏展示用）
pub(crate) fn ms_to_date(ms: i64) -> String {
    let (y, m, d) = civil_from_days(ms.div_euclid(86_400_000));
    format!("{y:04}{m:02}{d:02}")
}

/// 天数 → (年, 月, 日)（Howard Hinnant 的 civil_from_days 算法）
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}
