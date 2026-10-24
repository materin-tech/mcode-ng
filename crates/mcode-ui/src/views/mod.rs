// MCode_Desktop — AI coding agent desktop app
// Copyright (c) 2026 Materin-tech. All rights reserved.
// SPDX-License-Identifier: MIT

//! 页面视图 — Settings / Extensions / Sessions / Onboarding 四视图渲染
//! （chat 布局在 chat_view.rs；文件超 800 行时按规格拆分）。
//! 布局规范 = docs/prototype/screens/{settings,extensions,sessions,onboarding}.html。

pub mod chat_view;
pub mod context_menu;
pub mod dialogs;
pub mod file_explorer;
pub mod i18n;
pub mod sidebar_view;
pub mod text_input;
pub mod theme;
pub mod transcript_view;
pub mod workspace_picker;

use gpui::{
    AnyElement, InteractiveElement, SharedString, StatefulInteractiveElement, div, prelude::*, px,
    rgb,
};
use mcode_core::permission::Mode;
use mcode_core::transcript::NoticeKind;

use crate::app::msg::trunc;
use crate::app::session_ops::{lang_label, lang_next};
use crate::app::{App, View};
use crate::views::chat_view::badge;
use crate::views::theme::*;

const MODES: [Mode; 4] = [Mode::Passon, Mode::Manual, Mode::Auto, Mode::Plan];

fn mode_hint(m: Mode) -> &'static str {
    m.hint()
}

/// 小号操作按钮（列表行的 编辑/删除 等）
fn small_btn(
    label: &str,
    id: String,
    on_click: impl Fn(&gpui::ClickEvent, &mut gpui::Window, &mut gpui::App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(SharedString::from(id))
        .px_2()
        .py_1()
        .rounded_sm()
        .border_1()
        .border_color(BORDER)
        .bg(BG_PANEL)
        .text_size(px(11.))
        .text_color(TEXT_2)
        .cursor_pointer()
        .hover(|s| s.bg(BG_HOVER))
        .on_click(on_click)
        .child(label.to_string())
}

// ---------- Onboarding ----------

/// 协议选择 chip（选中高亮 ACCENT；on_click 由调用方以 cx.listener 包装并写状态）
pub(crate) fn protocol_chip(
    id: &'static str,
    label: &str,
    selected: bool,
    on_click: impl Fn(&gpui::ClickEvent, &mut gpui::Window, &mut gpui::App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    let (bg, fg, border) = if selected {
        (ACCENT, rgb(0xffffff), ACCENT)
    } else {
        (BG_PANEL, TEXT_2, BORDER)
    };
    div()
        .id(SharedString::from(id))
        .px_3()
        .py_1()
        .rounded_sm()
        .border_1()
        .border_color(border)
        .bg(bg)
        .text_size(px(11.))
        .text_color(fg)
        .cursor_pointer()
        .hover(|s| s.bg(ACCENT).text_color(rgb(0xffffff)))
        .on_click(on_click)
        .child(label.to_string())
}

/// 「API 协议」说明行文案（onboarding 与 Provider 表单共用）
pub(crate) const PROTOCOL_HINT: &str = "API 协议（私有化部署 vLLM/SGLang 等通常选 chat）";

pub fn render_onboarding(app: &mut App, cx: &mut gpui::Context<App>) -> AnyElement {
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .bg(BG_WINDOW)
        .text_color(TEXT_1)
        .child(
            div()
                .w(px(620.))
                .flex()
                .flex_col()
                .gap_3()
                .child(
                    div()
                        .rounded_xl()
                        .border_1()
                        .border_color(BORDER_STRONG)
                        .bg(BG_PANEL)
                        .p_6()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .child(div().text_size(px(20.)).child("👋 欢迎使用 MCode Desktop"))
                        .child(
                            div()
                                .text_size(px(13.))
                                .text_color(TEXT_2)
                                .child("首次启动需要配置模型供应商。所有配置写入 ~/.mcode/config.json，与 CLI 版完全互通。"),
                        )
                        .child(field("Provider 名称", app.ob_name.clone()))
                        .child(field("Base URL", app.ob_url.clone()))
                        .child(field("API Key（环境变量名，不落明文）", app.ob_key.clone()))
                        .child(field("可用模型（逗号分隔）", app.ob_models.clone()))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(
                                    div()
                                        .text_size(px(12.))
                                        .text_color(TEXT_2)
                                        .child(PROTOCOL_HINT),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .gap_2()
                                        .child(protocol_chip(
                                            "ob-proto-responses",
                                            "Responses",
                                            app.ob_protocol == mcode_core::config::Protocol::Responses,
                                            cx.listener(|this, _: &gpui::ClickEvent, _, cx| {
                                                this.ob_protocol =
                                                    mcode_core::config::Protocol::Responses;
                                                cx.notify();
                                            }),
                                        ))
                                        .child(protocol_chip(
                                            "ob-proto-chat",
                                            "Chat Completions（vLLM/DeepSeek 私有化等）",
                                            app.ob_protocol == mcode_core::config::Protocol::Chat,
                                            cx.listener(|this, _: &gpui::ClickEvent, _, cx| {
                                                this.ob_protocol =
                                                    mcode_core::config::Protocol::Chat;
                                                cx.notify();
                                            }),
                                        )),
                                ),
                        )
                        .child(
                            div()
                                .text_size(px(11.5))
                                .text_color(TEXT_3)
                                .child("apiKey 只存环境变量名（或明文 key 兜底）；带 multimodal 标记的模型才接收图片附件（可在 desktop.json 扩展配置）。"),
                        )
                        .child(
                            div().flex().justify_end().child(
                                div()
                                    .id("btn-onboard-save")
                                    .px_4()
                                    .py_1p5()
                                    .rounded_md()
                                    .bg(ACCENT)
                                    .text_color(rgb(0xffffff))
                                    .text_size(px(12.5))
                                    .cursor_pointer()
                                    .on_click(cx.listener(|this, _: &gpui::ClickEvent, _, cx| {
                                        this.onboarding_save(cx);
                                    }))
                                    .child("保存并进入对话 →"),
                            ),
                        ),
                ),
        )
        .into_any_element()
}

pub(crate) fn field(
    label: &str,
    input: gpui::Entity<crate::views::text_input::TextInput>,
) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_size(px(12.))
                .text_color(TEXT_2)
                .child(label.to_string()),
        )
        .child(
            div()
                .rounded_md()
                .border_1()
                .border_color(BORDER_STRONG)
                .bg(rgb(0x222228))
                .overflow_hidden()
                .child(input),
        )
}

// ---------- Settings ----------

pub fn render_settings(app: &mut App, cx: &mut gpui::Context<App>) -> AnyElement {
    let desktop = mcode_core::config::DesktopConfig::load().unwrap_or_default();
    let loc = i18n::s(app.cfg.language);
    let mut root = page_scaffold(
        loc.settings_title,
        loc.settings_sub,
        cx.listener(|this, _: &gpui::ClickEvent, _, cx| {
            this.view = View::Chat;
            cx.notify();
        }),
    )
    .flex()
    .flex_col()
    .gap_5()
    .p_6();

    // 供应商列表：点击行激活；行侧 编辑/删除；头部新增（i18n 随语言切换）
    let loc = i18n::s(app.cfg.language);
    let providers: Vec<mcode_core::config::Provider> = app.cfg.providers.clone();
    root = root.child(
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().text_size(px(15.)).child(loc.providers_section))
                    .child(div().flex_1())
                    .child(small_btn(
                        loc.add_provider,
                        "btn-provider-add".into(),
                        cx.listener(|this, _: &gpui::ClickEvent, _, cx| {
                            this.open_provider_form(None, cx);
                            cx.notify();
                        }),
                    )),
            )
            .children(providers.iter().map(|p| {
                let active = app.cfg.active_provider.as_deref() == Some(p.name.as_str());
                let pname = p.name.clone();
                let p_edit = p.clone();
                let p_del = p.name.clone();
                let meta: SharedString = format!("{} · key: ${}", p.base_url, p.api_key_env).into();
                let models: SharedString = format!("models: {}", p.models.join(", ")).into();
                div()
                    .flex()
                    .items_start()
                    .gap_2()
                    .child(
                        div()
                            .id(SharedString::from(format!("prov-{}", p.name)))
                            .flex_1()
                            .rounded_lg()
                            .border_1()
                            .border_color(if active { ACCENT } else { BORDER })
                            .bg(BG_PANEL)
                            .p_3()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .cursor_pointer()
                            .hover(|s| s.bg(BG_HOVER))
                            .on_click(cx.listener(move |this, _: &gpui::ClickEvent, _, cx| {
                                if this.cfg.set_active_provider(&pname).is_ok() {
                                    this.save_cfg_and_rebuild(cx);
                                    let m = this.cfg.active_model().unwrap_or_default();
                                    this.push_notice(format!("{pname} → {m}"), NoticeKind::Success);
                                }
                                cx.notify();
                            }))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(div().text_size(px(13.)).child(p.name.clone()))
                                    .when(active, |d| d.child(badge(loc.active_badge, ACCENT))),
                            )
                            .child(
                                div()
                                    .font_family("Menlo")
                                    .text_size(px(11.))
                                    .text_color(TEXT_3)
                                    .child(meta),
                            )
                            .child(
                                div()
                                    .font_family("Menlo")
                                    .text_size(px(11.))
                                    .text_color(TEXT_3)
                                    .child(models),
                            ),
                    )
                    .child(small_btn(
                        loc.edit,
                        format!("prov-edit-{}", p.name),
                        cx.listener(move |this, _: &gpui::ClickEvent, _, cx| {
                            this.open_provider_form(Some(&p_edit), cx);
                            cx.notify();
                        }),
                    ))
                    .child(small_btn(
                        loc.delete,
                        format!("prov-del-{}", p.name),
                        cx.listener(move |this, _: &gpui::ClickEvent, _, cx| {
                            this.open_confirm_remove(crate::app::RemoveAction::Provider(
                                p_del.clone(),
                            ));
                            cx.notify();
                        }),
                    ))
            })),
    );

    // 当前供应商的模型列表（点击切换 → save + rebuild harness）
    root = root.child(
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(div().text_size(px(15.)).child(loc.models_section))
            .children(app.cfg.active_provider().map(|p| {
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .children(p.models.iter().enumerate().map(|(i, m)| {
                        let active = app.cfg.active_model().as_deref() == Some(m.as_str());
                        let model = m.clone();
                        div()
                            .id(SharedString::from(format!("model-{i}")))
                            .px_3()
                            .py_1p5()
                            .rounded_sm()
                            .flex()
                            .items_center()
                            .gap_2()
                            .text_size(px(12.))
                            .font_family("Menlo")
                            .text_color(if active { ACCENT } else { TEXT_1 })
                            .cursor_pointer()
                            .hover(|s| s.bg(BG_HOVER))
                            .on_click(cx.listener(move |this, _: &gpui::ClickEvent, _, cx| {
                                if this.set_model(&model) {
                                    this.save_cfg_and_rebuild(cx);
                                    this.push_notice(format!("→ {model}"), NoticeKind::Success);
                                }
                                cx.notify();
                            }))
                            .child(if active {
                                format!("▸ {m} · {}", loc.using_now)
                            } else {
                                m.clone()
                            })
                    }))
            })),
    );

    // 语言 / 默认模式 / PUA
    root = root
        .child(
            div()
                .flex()
                .flex_col()
                .gap_2()
                .child(div().text_size(px(15.)).child(loc.language_section))
                .child(
                    div()
                        .id("lang-cycle")
                        .w(px(280.))
                        .px_3()
                        .py_2()
                        .rounded_md()
                        .border_1()
                        .border_color(BORDER_STRONG)
                        .bg(BG_PANEL)
                        .cursor_pointer()
                        .hover(|s| s.bg(BG_HOVER))
                        .text_size(px(12.))
                        .on_click(cx.listener(|this, _: &gpui::ClickEvent, _, cx| {
                            this.cfg.language = lang_next(this.cfg.language);
                            this.save_cfg_and_rebuild(cx);
                            cx.notify();
                        }))
                        .child(SharedString::from(lang_label(app.cfg.language).to_string())),
                ),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .gap_2()
                .child(div().text_size(px(15.)).child(loc.default_mode_section))
                .child(div().flex().gap_2().children(MODES.map(|m| {
                    let sel = app.cfg.default_mode == m;
                    div()
                        .id(SharedString::from(format!("mode-{}", m.label())))
                        .flex_1()
                        .p_3()
                        .rounded_md()
                        .border_1()
                        .border_color(if sel { mode_color(m) } else { BORDER_STRONG })
                        .bg(BG_PANEL)
                        .cursor_pointer()
                        .hover(|s| s.bg(BG_HOVER))
                        .on_click(cx.listener(move |this, _: &gpui::ClickEvent, _, cx| {
                            this.set_default_mode(m, cx);
                            cx.notify();
                        }))
                        .child(
                            div()
                                .text_size(px(13.))
                                .child(SharedString::from(format!("● {}", m.label()))),
                        )
                        .child(
                            div()
                                .text_size(px(11.))
                                .text_color(TEXT_3)
                                .child(mode_hint(m)),
                        )
                }))),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(div().text_size(px(15.)).child(loc.pua_section))
                .child(
                    div()
                        .id("pua-toggle")
                        .px_3()
                        .py_1p5()
                        .rounded_md()
                        .border_1()
                        .border_color(if app.cfg.pua_always_on {
                            GREEN
                        } else {
                            BORDER_STRONG
                        })
                        .bg(BG_PANEL)
                        .cursor_pointer()
                        .text_size(px(12.))
                        .text_color(if app.cfg.pua_always_on { GREEN } else { TEXT_2 })
                        .on_click(cx.listener(|this, _: &gpui::ClickEvent, _, cx| {
                            let on = this.cfg.pua_always_on;
                            this.set_pua(!on, cx);
                            cx.notify();
                        }))
                        .child(if app.cfg.pua_always_on {
                            loc.pua_on
                        } else {
                            loc.pua_off
                        }),
                ),
        );

    // 权限规则（desktop.json 来源）
    root = root.child(
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(div().text_size(px(15.)).child(loc.permission_rules))
            .child(if desktop.permission_rules.is_empty() {
                div()
                    .text_size(px(12.))
                    .text_color(TEXT_3)
                    .child(loc.no_rules)
            } else {
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .children(desktop.permission_rules.iter().map(|r| {
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .px_3()
                            .py_1p5()
                            .rounded_sm()
                            .bg(BG_PANEL)
                            .text_size(px(11.5))
                            .font_family("Menlo")
                            .child(div().text_color(TEXT_1).child(r.tool.clone()))
                            .child(div().text_color(TEXT_3).child(r.pattern.clone()))
                            .child(
                                div()
                                    .ml_auto()
                                    .text_color(if r.allow { GREEN } else { RED })
                                    .child(if r.allow { "ALLOW" } else { "DENY" }),
                            )
                    }))
            }),
    );

    // 重新运行引导
    root = root.child(
        div().flex().child(
            div()
                .id("btn-rerun-onboarding")
                .px_3()
                .py_1p5()
                .rounded_md()
                .border_1()
                .border_color(BORDER_STRONG)
                .bg(BG_PANEL)
                .cursor_pointer()
                .text_size(px(12.))
                .on_click(cx.listener(|this, _: &gpui::ClickEvent, _, cx| {
                    this.view = View::Onboarding;
                    cx.notify();
                }))
                .child(loc.rerun_onboarding),
        ),
    );

    root.into_any_element()
}

// ---------- Extensions ----------

pub fn render_extensions(app: &mut App, cx: &mut gpui::Context<App>) -> AnyElement {
    let loc = i18n::s(app.cfg.language);
    let mut root = page_scaffold(
        loc.extensions_title,
        loc.extensions_sub,
        cx.listener(|this, _: &gpui::ClickEvent, _, cx| {
            this.view = View::Chat;
            cx.notify();
        }),
    )
    .flex()
    .flex_col()
    .gap_5()
    .p_6();

    // ---------- 技能（创建 / 本地目录安装 / 移除） ----------
    let skills: Vec<mcode_core::skills::SkillDefinition> = app.ext_skills.clone();
    let skill_rows: Vec<gpui::Div> = skills
        .iter()
        .map(|s| {
            let name = s.name.clone();
            let source = s
                .source_dir
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "local".to_string());
            // 用户/项目目录下的技能才可移除（删除按钮走 manage::remove_skill）
            let builtin = !mcode_core::util::mcode_dir()
                .is_some_and(|d| s.source_dir.starts_with(&d))
                && !s.source_dir.starts_with(&app.cwd);
            let src_tag = if builtin {
                (loc.builtin_tag, TEXT_3)
            } else {
                (loc.removable_tag, GREEN)
            };
            let del_name = name.clone();
            div()
                .flex()
                .items_center()
                .gap_2()
                .px_3()
                .py_1p5()
                .rounded_sm()
                .bg(BG_PANEL)
                .text_size(px(12.))
                .child(div().text_color(TEXT_1).child(name))
                .child(
                    div()
                        .text_color(src_tag.1)
                        .text_size(px(10.5))
                        .child(format!("[{source}]")),
                )
                .child(
                    div()
                        .text_color(TEXT_3)
                        .text_size(px(10.5))
                        .child(src_tag.0),
                )
                .when(!builtin, |d| {
                    d.child(small_btn(
                        loc.delete,
                        format!("skill-del-{}", s.name),
                        cx.listener(move |this, _: &gpui::ClickEvent, _, cx| {
                            this.open_confirm_remove(crate::app::RemoveAction::Skill(
                                del_name.clone(),
                            ));
                            cx.notify();
                        }),
                    ))
                })
        })
        .collect();
    root = root.child(
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().text_size(px(15.)).child(loc.skills_section))
                    .child(div().flex_1())
                    .child(small_btn(
                        loc.create_skill,
                        "btn-skill-create".into(),
                        cx.listener(|this, _: &gpui::ClickEvent, _, cx| {
                            this.open_skill_create(cx);
                            cx.notify();
                        }),
                    ))
                    .child(small_btn(
                        loc.install_skill,
                        "btn-skill-install".into(),
                        cx.listener(|this, _: &gpui::ClickEvent, _, cx| {
                            this.open_install(crate::app::InstallKind::Skill, cx);
                            cx.notify();
                        }),
                    )),
            )
            .when(skills.is_empty(), |d| {
                d.child(
                    div()
                        .text_size(px(12.))
                        .text_color(TEXT_3)
                        .child(loc.no_skills),
                )
            })
            .children(skill_rows),
    );

    // ---------- Subagents（创建 / 本地 .md 安装 / 移除） ----------
    let agents: Vec<mcode_core::subagents::AgentDefinition> = app.ext_agents.clone();
    let agent_rows: Vec<gpui::Div> = agents
        .iter()
        .map(|a| {
            let name = a.agent_type.clone();
            let desc: SharedString = crate::app::msg::trunc(&a.when_to_use, 52).into();
            let is_user = a.source != mcode_core::subagents::AgentSource::Builtin;
            let del_name = name.clone();
            div()
                .flex()
                .items_center()
                .gap_2()
                .px_3()
                .py_1p5()
                .rounded_sm()
                .bg(BG_PANEL)
                .text_size(px(12.))
                .child(div().text_color(TEXT_1).child(name))
                .child(div().text_color(TEXT_3).text_size(px(11.)).child(desc))
                .child(
                    div()
                        .text_color(if is_user { GREEN } else { TEXT_3 })
                        .text_size(px(10.5))
                        .child(if is_user {
                            loc.removable_tag
                        } else {
                            loc.builtin_tag
                        }),
                )
                .when(is_user, |d| {
                    d.child(small_btn(
                        loc.delete,
                        format!("agent-del-{}", a.agent_type),
                        cx.listener(move |this, _: &gpui::ClickEvent, _, cx| {
                            this.open_confirm_remove(crate::app::RemoveAction::Agent(
                                del_name.clone(),
                            ));
                            cx.notify();
                        }),
                    ))
                })
        })
        .collect();
    root = root.child(
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().text_size(px(15.)).child(loc.agents_section))
                    .child(div().flex_1())
                    .child(small_btn(
                        loc.create_agent,
                        "btn-agent-create".into(),
                        cx.listener(|this, _: &gpui::ClickEvent, _, cx| {
                            this.open_agent_create(cx);
                            cx.notify();
                        }),
                    ))
                    .child(small_btn(
                        loc.install_agent,
                        "btn-agent-install".into(),
                        cx.listener(|this, _: &gpui::ClickEvent, _, cx| {
                            this.open_install(crate::app::InstallKind::Agent, cx);
                            cx.notify();
                        }),
                    )),
            )
            .when(agents.is_empty(), |d| {
                d.child(
                    div()
                        .text_size(px(12.))
                        .text_color(TEXT_3)
                        .child(loc.no_agents),
                )
            })
            .children(agent_rows),
    );

    // ---------- MCP Servers（新增 / 启停 / 移除，写 desktop.json） ----------
    let desktop = mcode_core::config::DesktopConfig::load().unwrap_or_default();
    let mut mcp_rows: Vec<gpui::Div> = vec![];
    for cfg in &desktop.mcp_servers {
        // mcp_report = 失败 server 的警告串（"MCP server {name}: {e}"），有命中即连接失败
        let warning = app.harness.as_ref().and_then(|h| {
            h.mcp_report
                .iter()
                .find(|w| w.contains(&format!("MCP server {}", cfg.name)))
        });
        let (dot, state): (&'static str, SharedString) = if !cfg.enabled {
            ("⚪", loc.disable.to_string().into())
        } else {
            match warning {
                Some(w) => ("🔴", crate::app::msg::trunc(w, 60).into()),
                None => ("🟢", "connected".into()),
            }
        };
        let name: SharedString = cfg.name.clone().into();
        let meta: SharedString = format!("{} · {}", cfg.command, cfg.args.join(" ")).into();
        let tog_name = cfg.name.clone();
        let now_enabled = cfg.enabled;
        let del_name = cfg.name.clone();
        mcp_rows.push(
            div()
                .flex()
                .items_center()
                .gap_3()
                .p_3()
                .rounded_lg()
                .border_1()
                .border_color(BORDER)
                .bg(BG_PANEL)
                .child(div().text_size(px(14.)).child(dot))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .child(div().text_size(px(12.5)).text_color(TEXT_1).child(name))
                        .child(
                            div()
                                .font_family("Menlo")
                                .text_size(px(10.5))
                                .text_color(TEXT_3)
                                .child(meta),
                        ),
                )
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(if cfg.enabled && warning.is_none() {
                            GREEN
                        } else {
                            TEXT_3
                        })
                        .child(state),
                )
                .child(small_btn(
                    if now_enabled { loc.disable } else { loc.enable },
                    format!("mcp-tog-{}", cfg.name),
                    cx.listener(move |this, _: &gpui::ClickEvent, _, cx| {
                        let mut d = mcode_core::config::DesktopConfig::load().unwrap_or_default();
                        if d.set_mcp_enabled(&tog_name, !now_enabled).is_ok() {
                            d.save().ok();
                            this.rebuild_harness(cx);
                            this.push_notice(
                                format!("MCP `{tog_name}` 已切换"),
                                NoticeKind::Success,
                            );
                        }
                        cx.notify();
                    }),
                ))
                .child(small_btn(
                    loc.delete,
                    format!("mcp-del-{}", cfg.name),
                    cx.listener(move |this, _: &gpui::ClickEvent, _, cx| {
                        this.open_confirm_remove(crate::app::RemoveAction::Mcp(del_name.clone()));
                        cx.notify();
                    }),
                )),
        );
    }
    root = root.child(
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().text_size(px(15.)).child(loc.mcp_section))
                    .child(div().flex_1())
                    .child(small_btn(
                        loc.add_mcp,
                        "btn-mcp-add".into(),
                        cx.listener(|this, _: &gpui::ClickEvent, _, cx| {
                            this.open_mcp_form(cx);
                            cx.notify();
                        }),
                    )),
            )
            .when(desktop.mcp_servers.is_empty(), |d| {
                d.child(
                    div()
                        .text_size(px(12.))
                        .text_color(TEXT_3)
                        .child(loc.no_mcp),
                )
            })
            .children(mcp_rows),
    );

    // ---------- Hooks（只读展示） ----------
    root = root.child(
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(div().text_size(px(15.)).child(loc.hooks_section))
            .child(if desktop.hooks.is_empty() {
                div()
                    .text_size(px(12.))
                    .text_color(TEXT_3)
                    .child(loc.no_hooks)
            } else {
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .children(desktop.hooks.iter().map(|h| {
                        let event: SharedString = format!("{:?}", h.event).into();
                        let matcher: SharedString = h.matcher.clone().into();
                        let command: SharedString = crate::app::msg::trunc(&h.command, 60).into();
                        div()
                            .flex()
                            .items_center()
                            .gap_3()
                            .px_3()
                            .py_1p5()
                            .rounded_sm()
                            .bg(BG_PANEL)
                            .text_size(px(11.5))
                            .font_family("Menlo")
                            .child(div().text_color(PURPLE).child(event))
                            .child(div().text_color(TEXT_3).child(matcher))
                            .child(div().text_color(TEXT_2).child(command))
                    }))
            }),
    );

    root.into_any_element()
}

// ---------- Sessions ----------

pub fn render_sessions(app: &mut App, cx: &mut gpui::Context<App>) -> AnyElement {
    let mut root = page_scaffold(
        "🗂 会话列表",
        "当前目录历史会话 · 点击恢复 · 右键重命名/删除（/resume）",
        cx.listener(|this, _: &gpui::ClickEvent, _, cx| {
            this.view = View::Chat;
            cx.notify();
        }),
    )
    .flex()
    .flex_col()
    .gap_2()
    .p_6();

    if app.sessions.is_empty() {
        root = root.child(
            div()
                .text_size(px(12.))
                .text_color(TEXT_3)
                .child("暂无会话记录（点击「新建会话」后立即创建）"),
        );
    }
    root = root.children(app.sessions.iter().map(|m| {
        let custom = app
            .store
            .as_ref()
            .and_then(|s| crate::app::extension_manage::custom_session_title(s, &m.session_id));
        let title: SharedString = trunc(
            custom
                .as_deref()
                .or(m.title.as_deref())
                .unwrap_or("(空会话)"),
            40,
        )
        .into();
        let meta: SharedString = format!(
            "{}条 · {}",
            m.message_count,
            crate::app::session_ops::ms_to_date(m.updated_ms)
        )
        .into();
        let id = m.session_id.clone();
        let rc_id = m.session_id.clone();
        div()
            .id(SharedString::from(format!("page-sess-{}", m.session_id)))
            .flex()
            .items_center()
            .gap_3()
            .px_3()
            .py_2()
            .rounded_lg()
            .border_1()
            .border_color(BORDER)
            .bg(BG_PANEL)
            .cursor_pointer()
            .hover(|s| s.bg(BG_HOVER))
            .on_click(cx.listener(move |this, _: &gpui::ClickEvent, _, cx| {
                this.resume_session(&id, cx);
            }))
            // 右键 = 会话右键菜单（重命名/删除）；菜单浮层挂 render_app 顶层，盖住本页
            .on_mouse_down(
                gpui::MouseButton::Right,
                cx.listener(move |this, ev: &gpui::MouseDownEvent, window, cx| {
                    this.open_session_menu(
                        rc_id.clone(),
                        ev.position.x.to_f64() as f32,
                        ev.position.y.to_f64() as f32,
                        crate::views::context_menu::window_viewport(window),
                        cx,
                    );
                    cx.notify();
                }),
            )
            .child(div().text_size(px(13.)).text_color(TEXT_1).child(title))
            .child(
                div()
                    .ml_auto()
                    .text_size(px(11.))
                    .text_color(TEXT_3)
                    .child(meta),
            )
    }));

    root.into_any_element()
}

// ---------- 页面骨架（顶部标题栏 + 返回按钮） ----------

fn page_scaffold(
    title: &str,
    sub: &str,
    on_back: impl Fn(&gpui::ClickEvent, &mut gpui::Window, &mut gpui::App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    div()
        .size_full()
        .flex()
        .flex_col()
        .bg(BG_MAIN)
        .text_color(TEXT_1)
        .child(
            div()
                .h_11()
                .flex()
                .items_center()
                .gap_2()
                .px_3()
                .border_b_1()
                .border_color(BORDER)
                .child(div().text_size(px(12.5)).child(title.to_string()))
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(TEXT_3)
                        .child(sub.to_string()),
                )
                .child(div().flex_1())
                .child(
                    div()
                        .id("btn-back")
                        .px_2()
                        .cursor_pointer()
                        .text_size(px(12.))
                        .text_color(TEXT_2)
                        .hover(|s| s.bg(BG_HOVER))
                        .on_click(on_back)
                        .child("← 返回对话（ESC）"),
                ),
        )
        .id("page-root")
        .overflow_y_scroll()
}
