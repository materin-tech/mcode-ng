// MCode_Desktop — AI coding agent desktop app
// Copyright (c) 2026 Materin-tech. All rights reserved.
// SPDX-License-Identifier: MIT

//! 启动工作区选择器 — IDEA 式首屏：最近打开（desktop.json recentWorkspaces，
//! 最近在前、上限 8、选中置顶去重）+ 打开其他目录（绝对路径输入，~ 展开，is_dir 校验）。
//! 选定后：cwd → 记录 recent → 重扫 file_tree（respect show_hidden）→ rebuild_harness → 刷侧栏 → 进 Chat。
//! 首次无 recents 时以 ~/.mcode/projects/*/sessions/*.meta.json 的 cwd 字段做种子。

use gpui::{
    AnyElement, InteractiveElement, SharedString, StatefulInteractiveElement, div, prelude::*, px,
    rgb,
};
use mcode_core::config::DesktopConfig;
use std::path::PathBuf;

use crate::app::msg::trunc;
use crate::app::{App, View};
use crate::views::file_explorer::scan_file_tree;
use crate::views::theme::*;
use mcode_core::transcript::NoticeKind;

/// 读取最近工作区（desktop.json 为空时用历史会话目录做种子）
pub(crate) fn load_recents() -> Vec<String> {
    let desktop = DesktopConfig::load().unwrap_or_default();
    if !desktop.recent_workspaces.is_empty() {
        return desktop.recent_workspaces;
    }
    let mut out: Vec<String> = vec![];
    // 新内核会话 = projects/<slug>/sessions/*.jsonl，首行 SessionMeta（含 cwd）
    let Some(projects) = mcode_core::util::mcode_dir().map(|d| d.join("projects")) else {
        return out;
    };
    let Ok(dirs) = std::fs::read_dir(&projects) else {
        return out;
    };
    for d in dirs.flatten() {
        let sess = d.path().join("sessions");
        if let Ok(files) = std::fs::read_dir(&sess) {
            for f in files.flatten() {
                let p = f.path();
                if p.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                    continue;
                }
                let Ok(content) = std::fs::read_to_string(&p) else {
                    continue;
                };
                let Some(first) = content.lines().next() else {
                    continue;
                };
                if let Ok(m) = serde_json::from_str::<mcode_core::sessions::SessionMeta>(first)
                    && !out.contains(&m.cwd)
                {
                    out.push(m.cwd);
                }
            }
        }
        if out.len() >= 8 {
            break;
        }
    }
    out
}

impl App {
    /// 记录最近工作区：置顶去重，上限 8，写回 desktop.json
    pub(crate) fn record_recent(&mut self) {
        let cur = self.cwd.to_string_lossy().into_owned();
        let mut desktop = DesktopConfig::load().unwrap_or_default();
        desktop.recent_workspaces.retain(|p| p != &cur);
        desktop.recent_workspaces.insert(0, cur);
        desktop.recent_workspaces.truncate(8);
        desktop.save().ok();
    }

    /// 进入工作区：cwd → recent → 清会话态 → 重扫 file_tree → rebuild harness → 刷侧栏 → Chat
    pub(crate) fn enter_workspace(&mut self, path: PathBuf, cx: &mut gpui::Context<Self>) {
        if !path.is_dir() {
            self.push_notice(format!("不是目录：{}", path.display()), NoticeKind::Error);
            cx.notify();
            return;
        }
        self.cwd = path;
        self.record_recent();
        self.new_chat(cx);
        self.fs_selected = None;
        self.fs_delete_armed = None;
        self.fs_clipboard = None;
        self.context_menu = None;
        self.rename_state = None;
        self.session_menu = None;
        self.session_rename = None;
        self.expanded_dirs.clear();
        self.file_tree = scan_file_tree(&self.cwd, self.show_hidden);
        self.rebuild_harness(cx);
        self.refresh_sessions();
        self.view = View::Chat;
        self.push_notice(
            format!("📂 工作区：{}", trunc(&self.cwd.to_string_lossy(), 60)),
            NoticeKind::Info,
        );
        cx.notify();
    }

    /// 系统原生目录选择对话框（prompt_for_paths directories 模式）。
    /// 结果回主线程走 enter_workspace 链；取消不动当前状态。
    /// 注意：测试平台 prompt_for_paths 未实现（unimplemented!()），无头测试直调 enter_workspace 覆盖链路。
    pub(crate) fn start_dir_picker(&mut self, cx: &mut gpui::Context<Self>) {
        let rx = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("选择工作目录".into()),
        });
        cx.spawn(async move |this, cx| {
            let result = rx.await;
            let _ = this.update(cx, |a, cx| {
                match result {
                    Ok(Ok(Some(mut paths))) => {
                        if let Some(p) = paths.pop() {
                            a.enter_workspace(p, cx);
                        }
                    }
                    Ok(Ok(None)) => {} // 用户取消，不动当前状态
                    Ok(Err(e)) => a.push_notice(format!("目录选择失败：{e:#}"), NoticeKind::Error),
                    Err(_) => {}
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 校验并规范化输入的目录路径（~ 展开 + is_dir）
    pub(crate) fn validate_ws_input(&mut self, cx: &mut gpui::Context<Self>) {
        let raw = self.ws_input.read(cx).text().trim().to_string();
        let expanded = if let Some(rest) = raw.strip_prefix('~') {
            format!("{}{}", std::env::var("HOME").unwrap_or_default(), rest)
        } else {
            raw.clone()
        };
        let p = PathBuf::from(expanded);
        if raw.is_empty() || !p.is_dir() {
            self.push_notice(format!("目录不存在：{raw}"), NoticeKind::Error);
            cx.notify();
            return;
        }
        self.enter_workspace(p, cx);
    }
}

/// 工作区选择器整屏渲染
pub fn render_workspace_picker(app: &mut App, cx: &mut gpui::Context<App>) -> AnyElement {
    let recents = load_recents();
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .bg(BG_WINDOW)
        .text_color(TEXT_1)
        .child(
            div()
                .w(px(640.))
                .rounded_xl()
                .border_1()
                .border_color(BORDER_STRONG)
                .bg(BG_PANEL)
                .p_6()
                .flex()
                .flex_col()
                .gap_4()
                .child(div().text_size(px(20.)).child("📂 选择工作区"))
                .child(
                    div().text_size(px(12.5)).text_color(TEXT_2).child(
                        "工具的工作目录决定会话存储与文件操作范围（与 CLI 的 ~/.mcode/projects 完全互通）。\
                         选定后可随时点击顶栏路径徽章切换。",
                    ),
                )
                // 最近打开
                .child(
                    div().flex().flex_col().gap_1()
                        .child(div().text_size(px(12.)).text_color(TEXT_3).child("最近打开"))
                        .when(recents.is_empty(), |d| {
                            d.child(
                                div().text_size(px(12.)).text_color(TEXT_3)
                                    .child("暂无记录——在下方输入目录路径开始使用"),
                            )
                        })
                        .children(recents.iter().enumerate().map(|(i, r)| {
                            let path = PathBuf::from(r);
                            div()
                                .id(SharedString::from(format!("ws-{i}")))
                                .flex()
                                .items_center()
                                .gap_2()
                                .px_3()
                                .py_2()
                                .rounded_md()
                                .border_1()
                                .border_color(BORDER)
                                .bg(BG_PANEL)
                                .cursor_pointer()
                                .hover(|s| s.bg(BG_HOVER))
                                .on_click(cx.listener(move |this, _: &gpui::ClickEvent, _, cx| {
                                    this.enter_workspace(path.clone(), cx);
                                }))
                                .child(div().text_size(px(11.5)).child("📁"))
                                .child(
                                    div().font_family(MONO_FONT).text_size(px(11.5)).text_color(TEXT_2)
                                        .child(SharedString::from(trunc(r, 64))),
                                )
                        })),
                )
                // 打开其他目录：系统原生目录选择对话框（主）+ 手动输入（次）
                .child(
                    div().flex().flex_col().gap_1()
                        .child(
                            div()
                                .id("btn-ws-browse")
                                .w_full()
                                .rounded_md()
                                .border_1()
                                .border_color(BORDER_STRONG)
                                .bg(BG_PANEL)
                                .py_2()
                                .text_center()
                                .text_size(px(12.5))
                                .text_color(TEXT_1)
                                .cursor_pointer()
                                .hover(|s| s.bg(BG_HOVER))
                                .on_click(cx.listener(|this, _: &gpui::ClickEvent, _, cx| {
                                    this.start_dir_picker(cx);
                                }))
                                .child("📁 选择目录…"),
                        )
                        .child(div().text_size(px(11.)).text_color(TEXT_3).child("或输入路径："))
                        .child(
                            div().flex().gap_2()
                                .child(
                                    div()
                                        .flex_1()
                                        .rounded_md()
                                        .border_1()
                                        .border_color(BORDER_STRONG)
                                        .bg(rgb(0x222228))
                                        .overflow_hidden()
                                        .child(app.ws_input.clone()),
                                )
                                .child(
                                    div()
                                        .id("btn-ws-open")
                                        .px_4()
                                        .py_2()
                                        .rounded_md()
                                        .bg(ACCENT)
                                        .text_color(rgb(0xffffff))
                                        .text_size(px(12.))
                                        .cursor_pointer()
                                        .on_click(cx.listener(|this, _: &gpui::ClickEvent, _, cx| {
                                            this.validate_ws_input(cx);
                                        }))
                                        .child("打开"),
                                ),
                        ),
                ),
        )
        .into_any_element()
}
