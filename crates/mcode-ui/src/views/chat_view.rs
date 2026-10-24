// MCode_Desktop — AI coding agent desktop app
// Copyright (c) 2026 Materin-tech. All rights reserved.
// SPDX-License-Identifier: MIT

//! chat 布局渲染 — 布局规范 = docs/prototype/screens/chat.html：
//! 侧栏（会话/占用）+ 顶栏（模型/路径/模式/goal 徽章）+ 消息流（工具卡/diff/Notice）+ Composer。
//! 共享渲染小件（badge / tool_icon）也在这里，供 views.rs 的页面视图复用。

use gpui::{
    AnyElement, Context, Div, Focusable, InteractiveElement, SharedString,
    StatefulInteractiveElement, Window, div, prelude::*, px, rgb,
};
use mcode_core::permission::Decision;

use crate::app::goal::MAX_GOAL_ROUNDS;
use crate::app::msg::ChatMsg;
use crate::app::msg::trunc;
use crate::app::{App, View};
use crate::views::sidebar_view::{render_sidebar, render_sidebar_collapsed};
use crate::views::theme::*;

/// 渲染分发：Onboarding / Settings / Extensions / Sessions / Chat
pub(crate) fn render_app(app: &mut App, window: &mut Window, cx: &mut Context<App>) -> AnyElement {
    let base = match app.view {
        View::Onboarding => crate::views::render_onboarding(app, cx),
        View::Settings => crate::views::render_settings(app, cx),
        View::Extensions => crate::views::render_extensions(app, cx),
        View::Sessions => crate::views::render_sessions(app, cx),
        View::Chat => render_chat(app, window, cx),
        View::WorkspacePicker => crate::views::workspace_picker::render_workspace_picker(app, cx),
    };
    // 浮层统一挂窗口根层（render_app 顶层）——gpui 按树序绘制，后挂的画在最上
    // （用户实测：右键浮窗必须展示在最上层；旧实现挂在 render_chat 内部，
    //   Sessions 等整页视图根本不渲染，且会被管理对话框盖住）
    // z 序（低 → 高）：页面 → 管理对话框 → 重命名弹窗 → 文件右键菜单 → 会话右键菜单
    let mut root = div().size_full().relative().child(base);
    // 管理对话框浮层（设置/扩展页增删改表单）叠加在页面之上
    if let Some(dlg) = crate::views::dialogs::render_dialog(app, cx) {
        root = root.child(dlg);
    }
    if let Some(m) = crate::views::file_explorer::render_rename_modal(app, cx) {
        root = root.child(m);
    }
    if let Some(m) = crate::views::context_menu::render_context_menu(app, cx) {
        root = root.child(m);
    }
    if let Some(m) = crate::views::context_menu::render_session_menu(app, cx) {
        root = root.child(m);
    }
    root.into_any_element()
}

fn render_chat(app: &mut App, _window: &mut Window, cx: &mut Context<App>) -> AnyElement {
    let mc = mode_color(app.mode);
    let model_label: SharedString = format!(
        "🧠 {}",
        app.cfg
            .active_model()
            .unwrap_or_else(|| "未选择模型".into())
    )
    .into();
    let cwd_label: SharedString = trunc(&app.cwd.to_string_lossy(), 46).into();
    let mode_label: SharedString = format!("⇥ {}", app.mode.label()).into();
    let goal_badge = app.goal.as_ref().map(|g| {
        let label: SharedString = format!(
            "🎯 {g}（{r}/{max}）",
            r = app.goal_rounds,
            max = MAX_GOAL_ROUNDS
        )
        .into();
        badge(label, YELLOW)
    });

    // 消息流
    let last_idx = app.messages.len().saturating_sub(1);
    let scroll = &app.thinking_scroll;
    let message_list: Vec<_> = app
        .messages
        .iter()
        .enumerate()
        .map(|(i, m)| render_msg(i, i == last_idx, m, scroll, cx))
        .collect();

    // 补全面板
    let palette_open = app.completion.is_open() && !app.candidates.is_empty();
    let palette_items: Vec<_> = app
        .candidates
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let sel = i == app.completion.selected;
            div()
                .px_3()
                .py_1()
                .text_size(px(12.))
                .flex()
                .gap_2()
                .when(sel, |d| d.bg(rgb(0x3a2a24)))
                .child(
                    div()
                        .text_color(if sel { ACCENT } else { TEXT_1 })
                        .font_family("Menlo")
                        .child(c.name.clone()),
                )
                .when(!c.desc.is_empty(), |d| {
                    d.child(div().text_color(TEXT_3).child(c.desc.clone()))
                })
        })
        .collect();

    // 排队插话
    let queue_len = app.queue.len();
    let queue_banner = if queue_len > 0 {
        div().px_6().pb_1().child(
            div()
                .rounded_full()
                .border_1()
                .border_color(BLUE)
                .text_color(BLUE)
                .px_3()
                .py_1()
                .text_size(px(11.5))
                .child(format!("📥 {} 条排队插话", queue_len)),
        )
    } else {
        div()
    };

    // 权限弹窗（允许 / 总是允许 / 拒绝 — respond oneshot 回传 runner）
    let perm_overlay = app.pending_permission.as_ref().map(|p| {
        let tool: SharedString = p.tool.clone().into();
        let args: SharedString = p.args_summary.clone().into();
        let mode_label: SharedString = app.mode.label().into();
        div()
            .absolute()
            .size_full()
            .top_0()
            .left_0()
            .bg(gpui::hsla(0., 0., 0., 0.55))
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .w(px(520.))
                    .rounded_xl()
                    .border_1()
                    .border_color(YELLOW)
                    .bg(BG_PANEL)
                    .p_4()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(div().text_color(YELLOW).child(SharedString::from(format!(
                        "⚠ 权限确认 — {mode_label} 模式：{tool}"
                    ))))
                    .child(
                        div()
                            .rounded_md()
                            .bg(BG_WINDOW)
                            .p_2()
                            .font_family("Menlo")
                            .text_size(px(11.5))
                            .text_color(TEXT_2)
                            .child(args),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(perm_btn(
                                "允许",
                                BORDER_STRONG,
                                TEXT_1,
                                cx.listener(|this, _: &gpui::ClickEvent, _, cx| {
                                    this.resolve_permission(Decision::Allow, cx);
                                }),
                            ))
                            .child(perm_btn(
                                "总是允许",
                                GREEN,
                                GREEN,
                                cx.listener(|this, _: &gpui::ClickEvent, _, cx| {
                                    this.resolve_permission(Decision::AllowAlways, cx);
                                }),
                            ))
                            .child(perm_btn(
                                "拒绝",
                                RED,
                                RED,
                                cx.listener(|this, _: &gpui::ClickEvent, _, cx| {
                                    this.resolve_permission(Decision::Deny, cx);
                                }),
                            )),
                    ),
            )
    });

    div()
        .key_context("AppRoot")
        .track_focus(&app.focus_handle(cx))
        .on_action(cx.listener(|this, _: &crate::app::CycleMode, _, cx| {
            this.cycle_mode();
            cx.notify();
        }))
        .on_action(cx.listener(|this, _: &crate::app::PaletteUp, _, cx| {
            if this.completion.is_open() {
                this.completion.move_up(this.candidates.len());
            } else if let Some(t) = this.history_recall_up(&this.input.read(cx).text()) {
                this.input.update(cx, |i, cx| i.set_text(&t, cx));
            }
            cx.notify();
        }))
        .on_action(cx.listener(|this, _: &crate::app::PaletteDown, _, cx| {
            if this.completion.is_open() {
                this.completion.move_down(this.candidates.len());
            } else if let Some(t) = this.history_recall_down() {
                this.input.update(cx, |i, cx| i.set_text(&t, cx));
            }
            cx.notify();
        }))
        .on_action(cx.listener(|this, _: &crate::app::PaletteComplete, _, cx| {
            if let Some(c) = this.candidates.get(this.completion.selected).cloned() {
                let text = this.input.read(cx).text();
                let new_text = this.completion.apply(&text, &c);
                this.input.update(cx, |inp, cx| inp.set_text(&new_text, cx));
            }
            this.completion.close();
            cx.notify();
        }))
        .on_action(cx.listener(|this, _: &crate::app::ClosePalette, _, cx| {
            if this.dialog.take().is_some() {
                cx.notify();
                return;
            }
            this.interrupt(cx);
        }))
        .on_action(cx.listener(|this, _: &crate::app::NewChat, _, cx| {
            this.new_chat(cx);
            cx.notify();
        }))
        .on_action(cx.listener(|this, _: &crate::app::OpenSettings, _, cx| {
            this.view = View::Settings;
            cx.notify();
        }))
        .on_action(cx.listener(|this, _: &crate::app::OpenExtensions, _, cx| {
            this.view = View::Extensions;
            this.refresh_extensions();
            cx.notify();
        }))
        .on_action(cx.listener(|this, _: &crate::app::OpenSessions, _, cx| {
            this.view = View::Sessions;
            cx.notify();
        }))
        .on_action(cx.listener(|this, _: &crate::app::BackToChat, _, cx| {
            this.view = View::Chat;
            cx.notify();
        }))
        .relative()
        .size_full()
        .flex()
        .flex_col()
        .bg(BG_MAIN)
        .text_color(TEXT_1)
        // 顶栏
        .child(
            div()
                .h_11()
                .flex()
                .flex()
                .items_center()
                .gap_2()
                .px_3()
                .border_b_1()
                .border_color(BORDER)
                .bg(BG_MAIN)
                .child(badge(model_label, TEXT_2))
                .child(
                    div()
                        .id("cwd-badge")
                        .px_2()
                        .py_0p5()
                        .rounded_full()
                        .border_1()
                        .border_color(BORDER)
                        .bg(BG_PANEL)
                        .font_family("Menlo")
                        .text_size(px(11.))
                        .text_color(TEXT_3)
                        .cursor_pointer()
                        .hover(|s| s.border_color(BORDER_STRONG))
                        .on_click(cx.listener(|this, _: &gpui::ClickEvent, _, cx| {
                            this.view = View::WorkspacePicker;
                            cx.notify();
                        }))
                        .child(cwd_label),
                )
                .child(
                    div()
                        .id("mode-badge")
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _: &gpui::ClickEvent, _, cx| {
                            this.cycle_mode();
                            cx.notify();
                        }))
                        .child(badge(mode_label, mc)),
                )
                .children(goal_badge)
                .child(div().flex_1())
                .child(
                    div()
                        .id("btn-explorer-toggle")
                        .cursor_pointer()
                        .px_2()
                        .text_size(px(12.))
                        .text_color(if app.explorer_visible { ACCENT } else { TEXT_2 })
                        .on_click(cx.listener(|this, _: &gpui::ClickEvent, _, cx| {
                            this.explorer_visible = !this.explorer_visible;
                            cx.notify();
                        }))
                        .child("🗂 文件"),
                )
                .child(
                    div()
                        .id("btn-settings")
                        .cursor_pointer()
                        .px_2()
                        .text_size(px(12.))
                        .text_color(TEXT_2)
                        .on_click(cx.listener(|this, _: &gpui::ClickEvent, _, cx| {
                            this.view = View::Settings;
                            cx.notify();
                        }))
                        .child("⚙ 设置"),
                )
                .child(
                    div()
                        .id("btn-extensions")
                        .cursor_pointer()
                        .px_2()
                        .text_size(px(12.))
                        .text_color(TEXT_2)
                        .on_click(cx.listener(|this, _: &gpui::ClickEvent, _, cx| {
                            this.view = View::Extensions;
                            cx.notify();
                        }))
                        .child("🧩 扩展"),
                ),
        )
        // 主体：侧栏 + 消息流
        .child(
            div()
                .flex_1()
                .min_h_0()
                .flex()
                .child(if app.sidebar_collapsed {
                    render_sidebar_collapsed(app, cx)
                } else {
                    render_sidebar(app, cx)
                })
                .child(
                    // 主列
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(
                            // 消息滚动区（外层 relative 承载"回到底部"浮钮；
                            // 跟随模式每帧钉底，新内容到达即贴底）
                            div()
                                .relative()
                                .flex_1()
                                .min_h_0()
                                .flex()
                                .flex_col()
                                .when(app.chat_follow, |d| {
                                    app.chat_scroll.scroll_to_bottom();
                                    d
                                })
                                .child(
                                    div()
                                        .id("messages")
                                        .flex_1()
                                        .min_h_0()
                                        .overflow_y_scroll()
                                        .track_scroll(&app.chat_scroll)
                                        .on_scroll_wheel(cx.listener(
                                            |this, event: &gpui::ScrollWheelEvent, _, _| {
                                                let dy = event.delta.pixel_delta(px(16.)).y;
                                                if dy > gpui::Pixels::ZERO {
                                                    // 上翻：暂停跟随
                                                    this.chat_follow = false;
                                                    this.chat_last_down_y = None;
                                                } else if dy < gpui::Pixels::ZERO {
                                                    // 贴底探测：连续两次下滚 offset 不再变化
                                                    // = 已到底（或内容不足一屏），恢复跟随
                                                    let y = this.chat_scroll.offset().y;
                                                    if this.chat_last_down_y == Some(y) {
                                                        this.chat_follow = true;
                                                    }
                                                    this.chat_last_down_y = Some(y);
                                                }
                                            },
                                        ))
                                        .px_5()
                                        .py_3()
                                        .children(message_list),
                                )
                                .when(!app.chat_follow, |d| {
                                    d.child(
                                        div()
                                            .id("btn-chat-bottom")
                                            .absolute()
                                            .bottom_3()
                                            .right_6()
                                            .px_3()
                                            .py_1p5()
                                            .rounded_full()
                                            .border_1()
                                            .border_color(BORDER_STRONG)
                                            .bg(BG_PANEL)
                                            .text_color(TEXT_2)
                                            .text_size(px(11.))
                                            .cursor_pointer()
                                            .on_click(cx.listener(
                                                |this, _: &gpui::ClickEvent, _, cx| {
                                                    this.chat_follow = true;
                                                    cx.notify();
                                                },
                                            ))
                                            .child("↓ 回到底部"),
                                    )
                                }),
                        )
                        .child(queue_banner)
                        .child(
                            // Composer
                            div().px_5().pb_3()
                                .when(palette_open, |d| {
                                    d.child(
                                        div()
                                            .id("palette")
                                            .rounded_lg()
                                            .border_1()
                                            .border_color(BORDER_STRONG)
                                            .bg(BG_PANEL)
                                            .overflow_hidden()
                                            .max_h(px(260.))
                                            .overflow_y_scroll()
                                            .children(palette_items),
                                    )
                                })
                                .child(
                                    div()
                                        .rounded_lg()
                                        .border_1()
                                        .border_color(BORDER_STRONG)
                                        .bg(BG_PANEL)
                                        .overflow_hidden()
                                        .child(app.input.clone())
                                        .child(
                                            div()
                                                .flex()
                                                .items_center()
                                                .gap_2()
                                                .px_3()
                .items_center()
                                                .pb_2()
                                                .text_size(px(10.5))
                                                .text_color(TEXT_3)
                                                .child("⏎ 发送 · / 指令 · @ 文件 · ⇧Tab 模式 · ESC 打断"),
                                        ),
                                ),
                        ),
                )
                .when(app.explorer_visible, |d| {
                    d.child(crate::views::file_explorer::render_file_explorer(app, cx))
                }),
        )
        // 权限弹窗挂最后：按树序绘制保证盖住主内容
        // （重命名弹窗/右键菜单已上移到 render_app 顶层，全视图可用且盖住管理对话框）
        .when_some(perm_overlay, |d, ov| d.child(ov))
        .into_any_element()
}

// ---------- 消息渲染 ----------

/// 消息折叠头部行（💭/回复 · 行数 · ▾/▸，点击循环三态）
fn fold_head(
    id: &'static str,
    label: SharedString,
    idx: usize,
    cx: &mut Context<App>,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(SharedString::from(format!("{id}-{idx}")))
        .flex()
        .items_center()
        .cursor_pointer()
        .hover(|s| s.bg(BG_HOVER))
        .text_size(px(11.5))
        .text_color(TEXT_3)
        .on_click(cx.listener(move |this, _: &gpui::ClickEvent, _, cx| {
            this.cycle_message_fold(idx);
            cx.notify();
        }))
        .child(label)
}

fn render_msg(
    idx: usize,
    is_last: bool,
    m: &ChatMsg,
    scroll: &gpui::ScrollHandle,
    cx: &mut Context<App>,
) -> AnyElement {
    match m {
        ChatMsg::User { text, refs } => div()
            .mb_3()
            .max_w(px(860.))
            .child(
                div()
                    .rounded_lg()
                    .border_1()
                    .border_color(BORDER)
                    .bg(BG_PANEL)
                    .px_3()
                    .py_2()
                    .text_color(TEXT_1)
                    .child(text.clone()),
            )
            .children(refs.iter().map(|r| {
                div()
                    .mr_1()
                    .mt_1()
                    .px_1p5()
                    .rounded_sm()
                    .text_size(px(11.))
                    .text_color(BLUE)
                    .child(r.clone())
            })),
        ChatMsg::Assistant { text, fold } => {
            let n_lines = text.lines().count();
            let head: SharedString = match fold {
                crate::app::msg::FoldState::Expanded => format!("▾ 回复 · {n_lines} 行").into(),
                crate::app::msg::FoldState::Preview => {
                    format!("▸ 回复 · 前 10 行 / 共 {n_lines} 行").into()
                }
                crate::app::msg::FoldState::Collapsed => format!("▸ 回复 · 共 {n_lines} 行").into(),
            };
            let body = match fold {
                crate::app::msg::FoldState::Expanded => {
                    crate::views::markdown_view::render_markdown(text)
                }
                crate::app::msg::FoldState::Preview => {
                    // 预览：前 10 行对应的 markdown（尾部提示在头部行）
                    let head_part: String = text.lines().take(10).collect::<Vec<_>>().join("\n");
                    crate::views::markdown_view::render_markdown(&head_part)
                }
                crate::app::msg::FoldState::Collapsed => {
                    let first: SharedString = trunc(text.lines().next().unwrap_or(""), 80).into();
                    div().text_color(TEXT_2).text_size(px(12.5)).child(first)
                }
            };
            div()
                .mb_3()
                .max_w(px(860.))
                .flex()
                .flex_col()
                .child(fold_head("reply-head", head, idx, cx).pl_1())
                .child(body)
        }
        ChatMsg::Thinking { text, fold } => {
            let count = text.chars().count();
            let state_label = match fold {
                crate::app::msg::FoldState::Expanded => "▾",
                _ => "▸",
            };
            let head: SharedString = format!(
                "💭 {} · {count} 字符 {state_label}",
                if is_last {
                    "思考中…"
                } else {
                    "思考过程"
                }
            )
            .into();
            let base = div()
                .mb_2()
                .max_w(px(860.))
                .rounded_sm()
                .border_1()
                .border_color(BORDER)
                .bg(BG_PANEL);
            match fold {
                crate::app::msg::FoldState::Expanded => base
                    .flex()
                    .flex_col()
                    .child(fold_head("think-head", head, idx, cx))
                    .child(
                        // 展开态：限高 160px 滚动区，流式写入时 scroll_to_bottom 跟随
                        div()
                            .id(SharedString::from(format!("think-body-{idx}")))
                            .mx_2()
                            .mb_2()
                            .max_h(px(160.))
                            .overflow_y_scroll()
                            .track_scroll(scroll)
                            .rounded_sm()
                            .bg(BG_WINDOW)
                            .p_2()
                            .font_family("Menlo")
                            .text_size(px(10.5))
                            .text_color(TEXT_3)
                            .child(SharedString::from(trunc(text, 30_000))),
                    ),
                crate::app::msg::FoldState::Preview => base
                    .flex()
                    .flex_col()
                    .child(fold_head("think-head", head, idx, cx))
                    .child(
                        div()
                            .id(SharedString::from(format!("think-body-{idx}")))
                            .mx_2()
                            .mb_2()
                            .max_h(px(80.))
                            .overflow_y_scroll()
                            .rounded_sm()
                            .bg(BG_WINDOW)
                            .p_2()
                            .font_family("Menlo")
                            .text_size(px(10.5))
                            .text_color(TEXT_3)
                            .child(SharedString::from(trunc(text, 3_000))),
                    ),
                crate::app::msg::FoldState::Collapsed => {
                    base.child(fold_head("think-head", head, idx, cx))
                }
            }
        }
        ChatMsg::ToolCard {
            name,
            args,
            summary,
            running,
            success,
        } => {
            let (icon, color) = tool_icon(name);
            let status: SharedString = if *running {
                "◌ 运行中".into()
            } else if *success {
                format!("✓ {summary}").into()
            } else {
                format!("✗ {summary}").into()
            };
            div()
                .mb_2()
                .max_w(px(860.))
                .rounded_md()
                .border_1()
                .border_color(BORDER)
                .bg(BG_PANEL)
                .px_3()
                .py_1p5()
                .flex()
                .items_center()
                .gap_2()
                .text_size(px(12.))
                .child(div().text_color(color).child(icon.to_string()))
                .child(div().text_color(TEXT_1).child(name.clone()))
                .child(div().text_color(TEXT_3).child(args.clone()))
                .child(
                    div()
                        .ml_auto()
                        .text_color(if *running {
                            YELLOW
                        } else if *success {
                            GREEN
                        } else {
                            RED
                        })
                        .child(status),
                )
        }
        ChatMsg::Diff { path, lines } => {
            let mut wrap = div()
                .mb_2()
                .max_w(px(860.))
                .rounded_md()
                .border_1()
                .border_color(BORDER)
                .bg(BG_WINDOW)
                .overflow_hidden()
                .child(
                    div()
                        .px_3()
                        .py_1()
                        .text_size(px(11.))
                        .text_color(TEXT_3)
                        .child(path.clone()),
                );
            for l in lines {
                let (bg, fg) = if l.hunk {
                    (BG_WINDOW, PURPLE)
                } else if l.add {
                    (rgb(0x1b2a1e), rgb(0x9fd9a3))
                } else if l.del {
                    (rgb(0x2d1b1b), rgb(0xeb9a9a))
                } else {
                    (BG_WINDOW, TEXT_3)
                };
                let prefix = if l.add {
                    "+"
                } else if l.del {
                    "-"
                } else {
                    " "
                };
                wrap = wrap.child(
                    div()
                        .bg(bg)
                        .px_3()
                        .text_size(px(11.5))
                        .font_family("Menlo")
                        .text_color(fg)
                        .child(SharedString::from(format!("{prefix}{}", l.text))),
                );
            }
            wrap
        }
        ChatMsg::Notice { text, error } => div()
            .mb_2()
            .max_w(px(860.))
            .rounded_sm()
            .border_1()
            .border_color(if *error { RED } else { BORDER })
            .bg(if *error { rgb(0x2d1b1b) } else { BG_PANEL })
            .px_2()
            .py_1()
            .text_size(px(11.5))
            .text_color(if *error { RED } else { TEXT_2 })
            .child(text.clone()),
        ChatMsg::Running => div()
            .mb_3()
            .text_color(ACCENT)
            .child("◌ 运行中…  （ESC 打断）"),
    }
    .into_any_element()
}

// ---------- 共享渲染小件 ----------

pub(crate) fn badge(label: impl Into<SharedString>, color: gpui::Rgba) -> Div {
    div()
        .px_2()
        .py_0p5()
        .rounded_full()
        .text_size(px(11.))
        .text_color(color)
        .border_1()
        .border_color(color)
        .child(label.into())
}

/// 权限弹窗按钮
fn perm_btn(
    label: &str,
    border: gpui::Rgba,
    text: gpui::Rgba,
    on_click: impl Fn(&gpui::ClickEvent, &mut gpui::Window, &mut gpui::App) + 'static,
) -> gpui::Stateful<Div> {
    div()
        .id(SharedString::from(format!("perm-{}", label)))
        .px_3()
        .py_1p5()
        .rounded_md()
        .border_1()
        .border_color(border)
        .text_color(text)
        .text_size(px(12.))
        .cursor_pointer()
        .hover(|s| s.bg(BG_HOVER))
        .on_click(on_click)
        .child(label.to_string())
}

/// 工具类别 → (图标, 语义色)
pub(crate) fn tool_icon(name: &str) -> (&'static str, gpui::Rgba) {
    if name == "task" {
        ("⚡", PURPLE)
    } else if name == "bash" {
        ("▶", YELLOW)
    } else if matches!(name, "read_file" | "glob" | "grep" | "ls") {
        ("📄", BLUE)
    } else if matches!(name, "edit_file" | "write_file") {
        ("✏️", GREEN)
    } else if name.starts_with("mcp__") {
        ("🔗", BLUE)
    } else {
        ("✏️", GREEN)
    }
}
