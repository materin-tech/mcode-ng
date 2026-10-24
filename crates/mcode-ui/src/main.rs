// MCode_Desktop — AI coding agent desktop app
// Copyright (c) 2026 Materin-tech. All rights reserved.
// SPDX-License-Identifier: MIT

//! MCode_Desktop — GPUI 桌面端入口

// Windows：release 构建按 GUI 子系统链接。Rust 在 Windows 默认是 console 子系统，
// 系统会在启动时给进程分配一个控制台窗口并随进程常驻（实测：桌面端启动后多一个
// 黑色 cmd 窗口关不掉）。debug 构建保留控制台，便于看 eprintln/panic 输出。
#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

mod app;
#[cfg(test)]
mod tests;
mod views;

use views::file_explorer;

use app::App;
use gpui::{
    Application, Bounds, KeyBinding, Menu, MenuItem, TitlebarOptions, WindowBounds, WindowOptions,
    actions, prelude::*, px, size,
};

actions!(desktop, [Quit]);

fn main() {
    // macOS 标准行为：关窗保留进程，Dock 图标 reopen（applicationShouldHandleReopen）
    // 重建窗口——gpui 0.2 经 Application::on_reopen 暴露该回调
    let app = Application::new();
    app.on_reopen(|cx| {
        if cx.windows().is_empty() {
            let root = spawn_main_window(cx);
            root.update(cx, focus_composer).ok();
        }
        cx.activate(true);
    });
    app.run(|cx: &mut gpui::App| {
        cx.bind_keys([
            // TextInput 编辑动作
            // 修饰键用 secondary（gpui 跨平台别名：macOS→cmd，Windows/Linux→ctrl）。
            // 不能写 cmd-：gpui 0.2 的 Windows 后端把 platform 修饰键映射为 Win 键，
            // cmd-v 在 Windows 上实际是 Win+V，Ctrl+V 永远不命中（实测引导页无法粘贴的根因）。
            KeyBinding::new("backspace", ti::Backspace, None),
            KeyBinding::new("delete", ti::Delete, None),
            KeyBinding::new("left", ti::Left, None),
            KeyBinding::new("right", ti::Right, None),
            KeyBinding::new("shift-left", ti::SelectLeft, None),
            KeyBinding::new("shift-right", ti::SelectRight, None),
            KeyBinding::new("secondary-a", ti::SelectAll, None),
            KeyBinding::new("secondary-v", ti::Paste, None),
            KeyBinding::new("secondary-c", ti::Copy, None),
            KeyBinding::new("secondary-x", ti::Cut, None),
            KeyBinding::new("home", ti::Home, None),
            KeyBinding::new("end", ti::End, None),
            KeyBinding::new("enter", ti::Submit, None),
            // 应用动作
            KeyBinding::new("shift-tab", app::CycleMode, None),
            KeyBinding::new("up", app::PaletteUp, None),
            KeyBinding::new("down", app::PaletteDown, None),
            KeyBinding::new("tab", app::PaletteComplete, None),
            KeyBinding::new("escape", app::ClosePalette, None),
            KeyBinding::new("secondary-n", app::NewChat, None),
            KeyBinding::new("secondary-,", app::OpenSettings, None),
            KeyBinding::new("secondary-e", app::OpenExtensions, None),
            KeyBinding::new("secondary-j", app::OpenSessions, None),
            KeyBinding::new("ctrl-o", app::ToggleDetail, None),
            // 文件浏览器键上下文（焦点在浏览器时优先于全局绑定）
            KeyBinding::new("secondary-c", file_explorer::FsCopy, Some("FileExplorer")),
            KeyBinding::new("secondary-x", file_explorer::FsCut, Some("FileExplorer")),
            KeyBinding::new("secondary-v", file_explorer::FsPaste, Some("FileExplorer")),
            KeyBinding::new(
                "secondary-backspace",
                file_explorer::FsDelete,
                Some("FileExplorer"),
            ),
            KeyBinding::new("up", file_explorer::FsSelUp, Some("FileExplorer")),
            KeyBinding::new("down", file_explorer::FsSelDown, Some("FileExplorer")),
            KeyBinding::new("right", file_explorer::FsToggleDir, Some("FileExplorer")),
            KeyBinding::new(
                "secondary-n",
                file_explorer::FsNewFile,
                Some("FileExplorer"),
            ),
            KeyBinding::new("secondary-q", Quit, None),
        ]);
        cx.on_action(|_: &Quit, cx| cx.quit());

        // 菜单栏（macOS 顶栏）。菜单点击经 App::dispatch_action 走活动窗口的
        // 焦点路径分发——与按键共用 chat_view 里的 on_action 监听器，无需新注册。
        cx.set_menus(app_menus());
        // Dock 图标右键菜单：关窗后台常驻时从这里回主界面
        cx.set_dock_menu(vec![MenuItem::action("新对话", app::NewChat)]);

        // 全局句柄必须先初始化——spawn_main_window 会读写它（曾因缺失导致启动即崩）
        cx.set_global(MainWindowHandle::default());
        let root = spawn_main_window(cx);
        root.update(cx, focus_composer).ok();
        cx.activate(true);
    });
}

/// ti 模块别名（KeyBinding 列表内引用 text_input 的动作类型）
use views::text_input as ti;

/// 应用菜单栏。关窗后进程常驻（见 on_reopen），菜单栏仍在——这是
/// 「状态栏运行」的入口：从菜单/Dock 图标随时唤回主窗口。
fn app_menus() -> Vec<Menu> {
    vec![
        Menu {
            name: "MCode Desktop".into(),
            items: vec![
                MenuItem::action("设置…", app::OpenSettings),
                MenuItem::separator(),
                MenuItem::action("退出 MCode Desktop", Quit),
            ],
        },
        Menu {
            name: "文件".into(),
            items: vec![
                MenuItem::action("新对话", app::NewChat),
                MenuItem::action("会话记录…", app::OpenSessions),
                MenuItem::separator(),
                MenuItem::action("扩展…", app::OpenExtensions),
            ],
        },
        Menu {
            name: "编辑".into(),
            items: vec![
                MenuItem::action("剪切", ti::Cut),
                MenuItem::action("拷贝", ti::Copy),
                MenuItem::action("粘贴", ti::Paste),
                MenuItem::separator(),
                MenuItem::action("全选", ti::SelectAll),
            ],
        },
    ]
}

/// 主窗口句柄全局（Dock reopen 重建用）
#[derive(Default)]
struct MainWindowHandle(std::cell::RefCell<Option<gpui::WindowHandle<App>>>);
impl gpui::Global for MainWindowHandle {}

/// 创建主窗口（启动与 Dock reopen 共用）
fn spawn_main_window(cx: &mut gpui::App) -> gpui::WindowHandle<App> {
    let bounds = Bounds::centered(None, size(px(1280.), px(820.)), cx);
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: Some(TitlebarOptions {
            title: Some("MCode Desktop".into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let root = cx.open_window(options, |_, cx| cx.new(App::new)).unwrap();
    cx.global::<MainWindowHandle>().0.borrow_mut().replace(root);
    root
}

/// 启动即聚焦 Composer:gpui 的按键 action 只沿焦点路径分发,无焦点时 Enter(Submit)
/// 根本到不了 TextInput——用户实测"输入后零响应"的根因即此(窗口打开后无任何元素持焦)。
fn focus_composer(app: &mut App, window: &mut gpui::Window, cx: &mut gpui::Context<App>) {
    let handle = app.input.read(cx).focus_handle.clone();
    handle.focus(window);
}
