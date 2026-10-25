// MCode_Desktop — AI coding agent desktop app
// Copyright (c) 2026 Materin-tech. All rights reserved.
// SPDX-License-Identifier: MIT

//! chat 布局渲染 — 布局规范 = docs/prototype/screens/chat.html：
//! 侧栏（会话/占用）+ 顶栏（模型/路径/模式/goal 徽章）+ 消息流（工具卡/diff/Notice）+ Composer。
//! 共享渲染小件（badge）也在这里，供 views.rs 的页面视图复用。

use gpui::{
    AnyElement, Context, Div, Focusable, InteractiveElement, SharedString,
    StatefulInteractiveElement, Window, div, prelude::*, px, rgb,
};
use mcode_core::permission::Decision;
use mcode_core::transcript::Block;

use crate::app::goal::MAX_GOAL_ROUNDS;
use crate::app::msg::trunc;
use crate::app::{App, View};
use crate::views::sidebar_view::{render_sidebar, render_sidebar_collapsed};
use crate::views::theme::*;
use crate::views::transcript_view::{line_div, styled_line};

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
    let bash_mode = app.input.read(cx).text().starts_with('!');
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

    // 消息流：transcript 块 → CLI 共享渲染核（render_block，width=MAX 由 gpui 折行）
    // → StyledText。冻结边界之前的块按定稿样式画；Thinking 头行可点切换详情（CLI ctrl+o）。
    let detail = app.transcript.detail_view;
    let blink_on = (app.frame_tick / 10).is_multiple_of(2);
    let boundary = app.transcript.frozen_boundary();
    let message_list: Vec<AnyElement> = app
        .transcript
        .blocks
        .iter()
        .enumerate()
        .flat_map(|(i, b)| {
            let lines =
                mcode_core::transcript::render_block(b, detail, usize::MAX, blink_on, i < boundary);
            let mut els: Vec<AnyElement> = lines
                .iter()
                .enumerate()
                .map(|(j, l)| {
                    let clickable = matches!(b, Block::Thinking { .. }) && j == 0;
                    if clickable {
                        div()
                            .id(SharedString::from(format!("think-head-{i}")))
                            .cursor_pointer()
                            .hover(|s| s.bg(crate::views::theme::BG_HOVER))
                            .on_click(cx.listener(|this, _: &gpui::ClickEvent, _, cx| {
                                this.toggle_detail();
                                cx.notify();
                            }))
                            .child(line_div(l))
                            .into_any_element()
                    } else {
                        line_div(l).into_any_element()
                    }
                })
                .collect();
            // 块间空行分隔（CLI 帧内块的语义间隔）
            els.push(div().h_2().into_any_element());
            els
        })
        .collect();

    // spinner 状态行（CLI：✻ Verb… (esc to interrupt · …)）；宽度给 MAX 保留全部片段
    let spinner_line = if app.transcript.busy {
        let elapsed = app
            .transcript
            .turn_started
            .map(|t| t.elapsed().as_secs())
            .unwrap_or(0);
        let frame = mcode_core::transcript::spinner::frame_at(app.frame_tick);
        Some(styled_line(&mcode_core::transcript::spinner::status_line(
            app.transcript.spinner_verb,
            frame,
            app.frame_tick,
            elapsed,
            app.transcript.turn_tokens,
            usize::MAX,
            None,
        )))
    } else {
        None
    };

    // footer（CLI 口径）：detail 提示 / 模式 chip / 距 auto-compact 百分比 / 快捷键兜底
    let mut footer_chips: Vec<(SharedString, gpui::Rgba)> = Vec::new();
    if app.transcript.detail_view {
        footer_chips.push((
            "Showing detailed transcript · ctrl+o to toggle".into(),
            TEXT_3,
        ));
    }
    match app.mode {
        mcode_core::permission::Mode::Plan => {
            footer_chips.push(("⏸ plan mode on (⇧Tab)".into(), BLUE))
        }
        mcode_core::permission::Mode::Auto => {
            footer_chips.push(("⏵⵵ accept edits on (⇧Tab)".into(), YELLOW))
        }
        mcode_core::permission::Mode::Passon => {
            footer_chips.push(("⏵⵵ bypass permissions on (⇧Tab)".into(), RED))
        }
        mcode_core::permission::Mode::Manual => {}
    }
    {
        let model = app
            .harness
            .as_ref()
            .map(|h| h.deps.current_model())
            .unwrap_or_default();
        let cw = mcode_core::agent::context_window_for_model(&model);
        if cw > 0
            && let Some(pct) = (app.ctx_tokens * 100).checked_div(cw)
        {
            footer_chips.push((format!("{pct}% until auto-compact").into(), TEXT_3));
        }
    }

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
                        .font_family(MONO_FONT)
                        // 候选统一带 `/` 前缀展示（core 指令与技能同口径；insert 由 apply 拼回）
                        .child(format!("/{}", c.name)),
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
                            .font_family(MONO_FONT)
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
            } else if !this.input.read(cx).is_composing() {
                // 组词中 ↑ 可能被 IME 穿透过来：此时召回历史会 set_text 整体替换
                // 内容并杀掉 marked，entity 与 IME 脱钩（后续退格误删已上屏汉字）
                if let Some(t) = this.history_recall_up(&this.input.read(cx).text()) {
                    this.input.update(cx, |i, cx| i.set_text(&t, cx));
                }
            }
            cx.notify();
        }))
        .on_action(cx.listener(|this, _: &crate::app::PaletteDown, _, cx| {
            if this.completion.is_open() {
                this.completion.move_down(this.candidates.len());
            } else if !this.input.read(cx).is_composing() {
                // 同 PaletteUp：组词中不放行历史召回
                if let Some(t) = this.history_recall_down() {
                    this.input.update(cx, |i, cx| i.set_text(&t, cx));
                }
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
                        .font_family(MONO_FONT)
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
                                        // 输出区终端帧复刻：等宽 + 13px 基准（⏺/⎿ 对齐、
                                        // 管道表格列宽、代码缩进依赖等宽；与输入框同源）
                                        .font_family(MONO_FONT)
                                        .text_size(px(13.))
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
                        .when_some(spinner_line, |d, sp| {
                            d.child(
                                div()
                                    .px_5()
                                    .py_1()
                                    .text_size(px(12.))
                                    .child(sp),
                            )
                        })
                        .child(
                            // Composer（bash 模式：`!` 前缀 → CLI 同款橙色边框）
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
                                        .border_color(if bash_mode { ACCENT } else { BORDER_STRONG })
                                        .bg(BG_PANEL)
                                        .overflow_hidden()
                                        .child(app.input.clone())
                                        .child(
                                            div()
                                                .flex()
                                                .items_center()
                                                .gap_2()
                                                .px_3()
                                                .pb_2()
                                                .text_size(px(10.5))
                                                .text_color(TEXT_3)
                                                .child(
                                                    "⏎ 发送 · / 指令 · @ 文件 · ! bash · ⇧Tab 模式 · ESC 打断",
                                                ),
                                        ),
                                ),
                        )
                        .when(!footer_chips.is_empty(), |d| {
                            d.child(
                                div()
                                    .px_5()
                                    .pb_2()
                                    .flex()
                                    .flex_col()
                                    .gap_0p5()
                                    .children(footer_chips.iter().map(|(label, color)| {
                                        div()
                                            .text_size(px(10.5))
                                            .text_color(*color)
                                            .child(label.clone())
                                    })),
                            )
                        }),
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
