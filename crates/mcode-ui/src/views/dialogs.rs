// MCode_Desktop — AI coding agent desktop app
// Copyright (c) 2026 Materin-tech. All rights reserved.
// SPDX-License-Identifier: MIT

//! 管理对话框浮层 — 设置/扩展页的增删改表单（遮罩 + 居中卡片）。
//! 状态与提交逻辑在 app/manage.rs；这里只做渲染。

use gpui::{
    AnyElement, InteractiveElement, SharedString, StatefulInteractiveElement, div, prelude::*, px,
    rgb,
};

use super::i18n;
use crate::app::{App, Dialog, InstallKind, RemoveAction};
use crate::views::theme::*;

/// 有对话框时返回遮罩 + 卡片浮层，None = 不渲染（由 chat_view 叠加）
pub fn render_dialog(app: &mut App, cx: &mut gpui::Context<App>) -> Option<AnyElement> {
    let loc = i18n::s(app.cfg.language);
    // 确认卡片单独渲染（无输入框）
    if let Some(Dialog::ConfirmRemove { action }) = &app.dialog {
        let action = action.clone();
        return Some(render_confirm(app, &action, cx));
    }
    let dialog = app.dialog.as_ref()?;
    let (title, fields, kind) = match dialog {
        Dialog::ProviderForm { .. } => (
            if matches!(
                dialog,
                Dialog::ProviderForm {
                    editing: Some(_),
                    ..
                }
            ) {
                loc.dlg_edit_provider
            } else {
                loc.dlg_new_provider
            },
            match dialog {
                Dialog::ProviderForm {
                    name,
                    url,
                    key,
                    models,
                    mm,
                    ..
                } => vec![
                    (loc.f_name, name.clone()),
                    (loc.f_base_url, url.clone()),
                    (loc.f_api_key, key.clone()),
                    (loc.f_models, models.clone()),
                    (loc.f_multimodal, mm.clone()),
                ],
                _ => unreachable!(),
            },
            DialogKind::Provider,
        ),
        Dialog::AgentCreate { name, desc, prompt } => (
            loc.dlg_create_agent,
            vec![
                (loc.f_name, name.clone()),
                (loc.f_agent_desc, desc.clone()),
                (loc.f_agent_prompt, prompt.clone()),
            ],
            DialogKind::Agent,
        ),
        Dialog::SkillCreate { name, desc } => (
            loc.dlg_create_skill,
            vec![(loc.f_name, name.clone()), (loc.f_agent_desc, desc.clone())],
            DialogKind::Skill,
        ),
        Dialog::Install { kind, path } => (
            match kind {
                InstallKind::Skill => loc.dlg_install_skill,
                InstallKind::Agent => loc.dlg_install_agent,
            },
            vec![(loc.f_path, path.clone())],
            DialogKind::Install,
        ),
        Dialog::McpForm { name, command } => (
            loc.dlg_new_mcp,
            vec![
                (loc.f_name, name.clone()),
                (loc.f_mcp_command, command.clone()),
            ],
            DialogKind::Mcp,
        ),
        Dialog::ConfirmRemove { .. } => unreachable!(),
    };
    let _ = &fields;
    let field_list: Vec<gpui::Div> = fields
        .iter()
        .map(|(label, input)| super::field(label, input.clone()))
        .collect();
    let kind2 = kind;
    // Provider 表单独有：API 协议选择行（点 chip 直接改 dialog 状态）
    let protocol_extra = match dialog {
        Dialog::ProviderForm { protocol, .. } => {
            use mcode_core::config::Protocol;
            let selected = *protocol;
            Some(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(TEXT_2)
                            .child(super::PROTOCOL_HINT),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(super::protocol_chip(
                                "dlg-proto-responses",
                                "Responses",
                                selected == Protocol::Responses,
                                cx.listener(|this, _: &gpui::ClickEvent, _, cx| {
                                    if let Some(Dialog::ProviderForm { protocol, .. }) =
                                        &mut this.dialog
                                    {
                                        *protocol = Protocol::Responses;
                                    }
                                    cx.notify();
                                }),
                            ))
                            .child(super::protocol_chip(
                                "dlg-proto-chat",
                                "Chat Completions（vLLM/DeepSeek 私有化等）",
                                selected == Protocol::Chat,
                                cx.listener(|this, _: &gpui::ClickEvent, _, cx| {
                                    if let Some(Dialog::ProviderForm { protocol, .. }) =
                                        &mut this.dialog
                                    {
                                        *protocol = Protocol::Chat;
                                    }
                                    cx.notify();
                                }),
                            )),
                    ),
            )
        }
        _ => None,
    };
    Some(
        overlay(
            div()
                .flex()
                .flex_col()
                .gap_3()
                .child(div().text_size(px(15.)).child(title))
                .children(field_list)
                .children(protocol_extra)
                .child(
                    div()
                        .flex()
                        .justify_end()
                        .gap_2()
                        .child(cancel_btn(loc.cancel, cx))
                        .child(
                            div()
                                .id("btn-dialog-save")
                                .px_4()
                                .py_1p5()
                                .rounded_md()
                                .bg(ACCENT)
                                .text_color(rgb(0xffffff))
                                .text_size(px(12.5))
                                .cursor_pointer()
                                .on_click(cx.listener(move |this, _: &gpui::ClickEvent, _, cx| {
                                    match kind2 {
                                        DialogKind::Provider => this.dialog_save_provider(cx),
                                        DialogKind::Agent => this.dialog_agent_create(cx),
                                        DialogKind::Skill => this.dialog_skill_create(cx),
                                        DialogKind::Install => this.dialog_install(cx),
                                        DialogKind::Mcp => this.dialog_save_mcp(cx),
                                    }
                                    cx.notify();
                                }))
                                .child(match kind2 {
                                    DialogKind::Provider => loc.save,
                                    DialogKind::Agent => loc.create,
                                    DialogKind::Skill => loc.create,
                                    DialogKind::Install => loc.install,
                                    DialogKind::Mcp => loc.save,
                                }),
                        ),
                ),
        )
        .into_any_element(),
    )
}

#[derive(Clone, Copy)]
enum DialogKind {
    Provider,
    Agent,
    Skill,
    Install,
    Mcp,
}

fn render_confirm(app: &mut App, action: &RemoveAction, cx: &mut gpui::Context<App>) -> AnyElement {
    let loc = i18n::s(app.cfg.language);
    let what: SharedString = match action {
        RemoveAction::Provider(n) => format!("{} `{n}`", loc.remove_btn).into(),
        RemoveAction::Skill(n) => format!("{} `{n}`", loc.remove_btn).into(),
        RemoveAction::Agent(n) => format!("{} `{n}`", loc.remove_btn).into(),
        RemoveAction::Mcp(n) => format!("{} `{n}`", loc.remove_btn).into(),
    };
    overlay(
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(div().text_size(px(14.)).child(SharedString::from(format!(
                "{} {what}?",
                loc.confirm_remove_prefix
            ))))
            .child(
                div()
                    .text_size(px(11.5))
                    .text_color(TEXT_3)
                    .child(loc.confirm_irreversible),
            )
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(cancel_btn(loc.cancel, cx))
                    .child(
                        div()
                            .id("btn-dialog-confirm")
                            .px_4()
                            .py_1p5()
                            .rounded_md()
                            .bg(RED)
                            .text_color(rgb(0xffffff))
                            .text_size(px(12.5))
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _: &gpui::ClickEvent, _, cx| {
                                this.dialog_confirm_remove(cx);
                                cx.notify();
                            }))
                            .child(loc.remove_btn),
                    ),
            ),
    )
    .into_any_element()
}

/// 遮罩 + 居中卡片容器
fn overlay(content: gpui::Div) -> gpui::Div {
    div()
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .bg(rgb(0x000000))
        .opacity(0.55)
        .flex()
        .items_center()
        .justify_center()
        .child(
            div()
                .w(px(520.))
                .rounded_xl()
                .border_1()
                .border_color(BORDER_STRONG)
                .bg(BG_PANEL)
                .p_6()
                .text_color(TEXT_1)
                .child(content),
        )
}

fn cancel_btn(label: &str, cx: &mut gpui::Context<App>) -> gpui::Stateful<gpui::Div> {
    div()
        .id("btn-dialog-cancel")
        .px_4()
        .py_1p5()
        .rounded_md()
        .border_1()
        .border_color(BORDER_STRONG)
        .bg(BG_PANEL)
        .text_color(TEXT_2)
        .text_size(px(12.5))
        .cursor_pointer()
        .on_click(cx.listener(|this, _: &gpui::ClickEvent, _, cx| {
            this.close_dialog();
            cx.notify();
        }))
        .child(label.to_string())
}
