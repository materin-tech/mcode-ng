// MCode_Desktop — AI coding agent desktop app
// Copyright (c) 2026 Materin-tech. All rights reserved.
// SPDX-License-Identifier: MIT

//! 工作区文件浏览器 — VS Code 式三栏布局的右栏：
//! 懒加载文件树（read_dir 逐层展开）+ 选中态 + 文件级剪贴板（cmd-c/x/v）+
//! 新建文件/目录 + 删除二次确认。所有路径经工作区校验，操作后刷新补全 file_tree。

use gpui::{
    Context, InteractiveElement, SharedString, StatefulInteractiveElement, actions, div,
    prelude::*, px, rgb,
};
use mcode_core::util::paths::contain_to_workspace;
use std::path::{Path, PathBuf};

use crate::app::App;
use crate::app::msg::trunc;
use crate::views::theme::*;

actions!(
    explorer,
    [
        FsCopy,
        FsCut,
        FsPaste,
        FsNewFile,
        FsNewDir,
        FsDelete,
        FsSelUp,
        FsSelDown,
        FsToggleDir
    ]
);

/// 扩展名 → 图标
fn file_icon(name: &str, is_dir: bool, open: bool) -> &'static str {
    if is_dir {
        return if open { "📂" } else { "📁" };
    }
    match name
        .rsplit('.')
        .next()
        .unwrap_or_default()
        .to_lowercase()
        .as_str()
    {
        "rs" => "🦀",
        "md" => "📝",
        "json" => "🔧",
        "toml" => "⚙️",
        "png" | "jpg" | "jpeg" | "gif" | "webp" => "🖼️",
        "lock" => "🔒",
        "ts" | "tsx" | "js" | "jsx" | "mjs" => "📜",
        "html" | "css" => "🎨",
        "sh" | "zsh" => "🖥️",
        "txt" => "📃",
        _ => "📄",
    }
}

/// 文件树可见行（渲染用）
pub(crate) struct FsRow {
    pub path: PathBuf,
    pub name: String,
    pub is_dir: bool,
    pub depth: usize,
    pub open: bool,
}

impl App {
    /// 当前文件树可见行（懒加载：只读已展开目录，目录优先 + 字典序，上限 2000 行）
    pub(crate) fn fs_rows(&self) -> Vec<FsRow> {
        let mut rows = vec![];
        self.fs_walk(&self.cwd.clone(), 0, &mut rows);
        rows
    }

    fn fs_walk(&self, dir: &Path, depth: usize, rows: &mut Vec<FsRow>) {
        if depth > 8 || rows.len() >= 2000 {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut items: Vec<(String, PathBuf, bool)> = entries
            .flatten()
            .filter(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                // 隐藏文件开关（默认关）：点开头条目整树过滤；target/node_modules/dist 恒过滤
                if matches!(name.as_str(), "target" | "node_modules" | "dist") {
                    return false;
                }
                if !self.show_hidden && name.starts_with('.') {
                    return false;
                }
                true
            })
            .map(|e| {
                let p = e.path();
                let is_dir = p.is_dir();
                (e.file_name().to_string_lossy().into_owned(), p, is_dir)
            })
            .collect();
        items.sort_by(|a, b| (b.2, &a.0).cmp(&(a.2, &b.0)));
        for (name, path, is_dir) in items {
            let open = is_dir && self.expanded_dirs.contains(&path);
            rows.push(FsRow {
                path: path.clone(),
                name,
                is_dir,
                depth,
                open,
            });
            if is_dir && open {
                self.fs_walk(&path, depth + 1, rows);
            }
        }
    }
}

impl App {
    // ---------- 文件操作（全部 std::fs + 工作区校验） ----------

    /// 路径必须在 cwd 内（复用内核 contain_to_workspace 校验）
    fn ws_ok(&self, p: &Path) -> Option<PathBuf> {
        let rel = p
            .strip_prefix(&self.cwd)
            .ok()?
            .to_string_lossy()
            .into_owned();
        contain_to_workspace(&self.cwd, &rel).ok()
    }

    /// 重名加 " copy" / " copy 2" 后缀（保留扩展名）
    fn dedup_name(dir: &Path, name: &str) -> String {
        if !dir.join(name).exists() {
            return name.to_string();
        }
        let (stem, ext) = match name.rsplit_once('.') {
            Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
            _ => (name.to_string(), String::new()),
        };
        for n in 1..100 {
            let candidate = if n == 1 {
                format!("{stem} copy{ext}")
            } else {
                format!("{stem} copy {n}{ext}")
            };
            if !dir.join(&candidate).exists() {
                return candidate;
            }
        }
        format!(
            "{stem}-{}/copy",
            std::time::SystemTime::now()
                .elapsed()
                .unwrap_or_default()
                .as_millis()
        )
    }

    /// 递归复制目录
    fn copy_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dst)?;
        for e in std::fs::read_dir(src)?.flatten() {
            let from = e.path();
            let to = dst.join(e.file_name());
            if from.is_dir() {
                Self::copy_tree(&from, &to)?;
            } else {
                std::fs::copy(&from, &to)?;
            }
        }
        Ok(())
    }

    /// 粘贴目标目录：选中目录用之，选中文件用其父目录，未选中用 cwd
    fn paste_dir(&self) -> PathBuf {
        match &self.fs_selected {
            Some(p) if p.is_dir() => p.clone(),
            Some(p) => p
                .parent()
                .map(|d| d.to_path_buf())
                .unwrap_or_else(|| self.cwd.clone()),
            None => self.cwd.clone(),
        }
    }

    pub(crate) fn fs_paste(&mut self) {
        let Some((src, cut)) = self.fs_clipboard.clone() else {
            self.push_notice("文件剪贴板为空".to_string(), true);
            return;
        };
        let Some(src_ok) = self.ws_ok(&src) else {
            self.push_notice("✗ 剪贴板路径已不在工作区内".to_string(), true);
            self.fs_clipboard = None;
            return;
        };
        let dir = self.paste_dir();
        let name = src_ok
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let dest = dir.join(Self::dedup_name(&dir, &name));
        let result = if cut {
            std::fs::rename(&src_ok, &dest)
        } else {
            if src_ok.is_dir() {
                Self::copy_tree(&src_ok, &dest)
            } else {
                std::fs::copy(&src_ok, &dest).map(|_| ())
            }
        };
        match result {
            Ok(_) => {
                self.push_notice(
                    format!("✓ {} → {}", src_ok.display(), dest.display()),
                    false,
                );
                if cut {
                    self.fs_clipboard = None;
                    if self.fs_selected.as_ref() == Some(&src_ok) {
                        self.fs_selected = None;
                    }
                }
            }
            Err(e) => self.push_notice(format!("✗ 粘贴失败：{e}"), true),
        }
        self.fs_refresh();
    }

    pub(crate) fn fs_copy_entry(&mut self, cut: bool) {
        match &self.fs_selected {
            Some(p) => {
                self.fs_clipboard = Some((p.clone(), cut));
                self.push_notice(
                    format!(
                        "{} {}（cmd-v 粘贴）",
                        if cut { "✂" } else { "📄" },
                        p.display()
                    ),
                    false,
                );
            }
            None => self.push_notice("未选中文件".to_string(), true),
        }
    }

    pub(crate) fn fs_new(&mut self, is_dir: bool) {
        let dir = self.paste_dir();
        let base = if is_dir { "新目录" } else { "new_file.txt" };
        let name = Self::dedup_name(&dir, base);
        let target = dir.join(&name);
        let r = if is_dir {
            std::fs::create_dir(&target)
        } else {
            std::fs::write(&target, "")
        };
        match r {
            Ok(_) => {
                self.fs_selected = Some(target);
                self.push_notice(format!("✓ 已创建 {}", name), false);
            }
            Err(e) => self.push_notice(format!("✗ 创建失败：{e}"), true),
        }
        self.fs_refresh();
    }

    /// 删除：第一次点击武装（Notice 提示），同一路径再点一次执行
    pub(crate) fn fs_delete(&mut self) {
        let Some(p) = self.fs_selected.clone() else {
            self.push_notice("未选中文件".to_string(), true);
            return;
        };
        if self.ws_ok(&p).is_none() {
            self.push_notice("✗ 路径不在工作区内".to_string(), true);
            return;
        }
        if self.fs_delete_armed.as_ref() != Some(&p) {
            self.fs_delete_armed = Some(p.clone());
            self.push_notice(
                format!(
                    "⚠ 将删除 {}（目录将递归删除）——再次点击删除按钮确认",
                    p.display()
                ),
                true,
            );
            return;
        }
        let r = if p.is_dir() {
            std::fs::remove_dir_all(&p)
        } else {
            std::fs::remove_file(&p)
        };
        match r {
            Ok(_) => self.push_notice(format!("✓ 已删除 {}", p.display()), false),
            Err(e) => self.push_notice(format!("✗ 删除失败：{e}"), true),
        }
        // 清理已删目录的展开状态（含子孙路径）——残留会让同名新建目录继承陈旧展开态
        self.expanded_dirs
            .retain(|d| !d.starts_with(&p) && !p.starts_with(d));
        self.fs_delete_armed = None;
        self.fs_selected = None;
        self.fs_refresh();
    }

    /// 操作后刷新：补全 file_tree 重扫 + notify
    pub(crate) fn fs_refresh(&mut self) {
        self.file_tree = scan_file_tree(&self.cwd, self.show_hidden);
    }

    // ---------- 重命名（右键菜单 → 行内弹窗） ----------

    pub(crate) fn rename_input(
        &mut self,
        cx: &mut Context<App>,
        name: &str,
    ) -> gpui::Entity<crate::views::text_input::TextInput> {
        cx.new(|cx| crate::views::text_input::TextInput {
            focus_handle: cx.focus_handle(),
            content: name.to_string().into(),
            placeholder: "新名称".into(),
            selected_range: name.len()..name.len(),
            selection_reversed: false,
            marked_range: None,
            last_layout: None,
            last_bounds: None,
            is_selecting: false,
        })
    }

    pub(crate) fn open_rename(&mut self, path: PathBuf, cx: &mut Context<App>) {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let input = self.rename_input(cx, &name);
        self.rename_state = Some(crate::views::context_menu::RenameState { path, input });
    }

    pub(crate) fn confirm_rename(&mut self, cx: &mut Context<App>) {
        let Some(st) = self.rename_state.as_ref() else {
            return;
        };
        let target = st.path.clone();
        let new_name = st.input.read(cx).text().trim().to_string();
        self.rename_state = None;
        self.fs_rename(&target, &new_name);
    }

    /// 重命名：std::fs::rename + 工作区校验 + 重名拒绝
    pub(crate) fn fs_rename(&mut self, target: &Path, new_name: &str) {
        let new_name = new_name.trim();
        if new_name.is_empty() || new_name.contains('/') || new_name == "." || new_name == ".." {
            self.push_notice("✗ 名称不能为空、包含 / 或为路径符号".to_string(), true);
            return;
        }
        let Some(dir) = target.parent() else { return };
        let dest = dir.join(new_name);
        if dest.exists() {
            self.push_notice(format!("✗ 名称已被占用：{new_name}"), true);
            return;
        }
        if self.ws_ok(&dest).is_none() || self.ws_ok(target).is_none() {
            self.push_notice("✗ 路径不在工作区内".to_string(), true);
            return;
        }
        match std::fs::rename(target, &dest) {
            Ok(_) => {
                self.push_notice(format!("✓ 已重命名 → {new_name}"), false);
                if self.fs_selected.as_ref() == Some(&target.to_path_buf()) {
                    self.fs_selected = Some(dest.clone());
                }
                // 展开状态迁移到新路径（目录自身 + 全部子孙路径），旧路径不残留
                self.expanded_dirs = self
                    .expanded_dirs
                    .iter()
                    .map(|d| {
                        if *d == target {
                            dest.clone()
                        } else if d.starts_with(target) {
                            dest.join(d.strip_prefix(target).unwrap_or(d))
                        } else {
                            d.clone()
                        }
                    })
                    .collect();
            }
            Err(e) => self.push_notice(format!("✗ 重命名失败：{e}"), true),
        }
        self.fs_refresh();
    }
}

impl App {
    // ---------- 引用文件 / 目录切换 / 选择移动 ----------

    /// 点击文件：向 Composer 输入框追加 "@path " 引用（不直接发送）
    pub(crate) fn fs_append_ref(&mut self, path: &Path, cx: &mut Context<Self>) {
        let rel = path
            .strip_prefix(&self.cwd)
            .map(|r| r.to_string_lossy().into_owned())
            .unwrap_or_else(|_| path.to_string_lossy().into_owned());
        let cur = self.input.read(cx).text();
        let sep = if cur.is_empty() || cur.ends_with(' ') {
            ""
        } else {
            " "
        };
        let new_text = format!("{cur}{sep}@{rel} ");
        self.input.update(cx, |i, cx| i.set_text(&new_text, cx));
    }

    /// 展开/折叠目录
    pub(crate) fn fs_toggle_dir(&mut self, path: &Path) {
        if !self.expanded_dirs.remove(path) {
            self.expanded_dirs.insert(path.to_path_buf());
        }
    }

    /// 上下键移动选中项（可见行序列）
    pub(crate) fn fs_move_selection(&mut self, down: bool) {
        let rows = self.fs_rows();
        if rows.is_empty() {
            return;
        }
        let cur_idx = self
            .fs_selected
            .as_ref()
            .and_then(|sel| rows.iter().position(|r| &r.path == sel))
            .unwrap_or(if down { rows.len() - 1 } else { 0 });
        let next = if down {
            (cur_idx + 1).min(rows.len() - 1)
        } else {
            cur_idx.saturating_sub(1)
        };
        self.fs_selected = Some(rows[next].path.clone());
    }
}

/// 文件浏览器整栏渲染（三栏布局的右栏，宽 240px）
pub(crate) fn render_file_explorer(app: &mut App, cx: &mut Context<App>) -> gpui::Div {
    let selected = app.fs_selected.clone();
    let armed = app.fs_delete_armed.clone();
    let clipboard: Option<SharedString> = app.fs_clipboard.as_ref().map(|(p, cut)| {
        SharedString::from(format!(
            "{} {}",
            if *cut { "✂" } else { "📄" },
            trunc(&p.to_string_lossy(), 22)
        ))
    });
    let rows = app.fs_rows();
    // “@即选中”联动：正文 @path 引用到的行加蓝色左侧标记（cwd 相对路径比对）
    let mentioned = app.mentioned.clone();
    let cwd = app.cwd.clone();

    // 工具条按钮（作用于选中项）
    let toolbar = {
        div()
            .flex()
            .gap_1()
            .child(fs_btn(
                "app",
                "＋",
                "新建文件",
                cx.listener(|this, _: &gpui::ClickEvent, _, cx| {
                    this.fs_new(false);
                    cx.notify();
                }),
            ))
            .child(fs_btn(
                "dir",
                "⛁",
                "新建目录",
                cx.listener(|this, _: &gpui::ClickEvent, _, cx| {
                    this.fs_new(true);
                    cx.notify();
                }),
            ))
            .child(fs_btn(
                "copy",
                "⧉",
                "复制（cmd-c）",
                cx.listener(|this, _: &gpui::ClickEvent, _, cx| {
                    this.fs_copy_entry(false);
                    cx.notify();
                }),
            ))
            .child(fs_btn(
                "cut",
                "✂",
                "剪切（cmd-x）",
                cx.listener(|this, _: &gpui::ClickEvent, _, cx| {
                    this.fs_copy_entry(true);
                    cx.notify();
                }),
            ))
            .child(fs_btn(
                "paste",
                "📋",
                "粘贴（cmd-v）",
                cx.listener(|this, _: &gpui::ClickEvent, _, cx| {
                    this.fs_paste();
                    cx.notify();
                }),
            ))
            .child(fs_btn(
                "del",
                &armed_label(&armed),
                "删除（二次确认）",
                cx.listener(|this, _: &gpui::ClickEvent, _, cx| {
                    this.fs_delete();
                    cx.notify();
                }),
            ))
    };

    // 文件树行
    let mut tree = div().flex().flex_col().children(rows.iter().map(|r| {
        let path = r.path.clone();
        let rc_path = path.clone();
        let sel = selected.as_ref() == Some(&path);
        let is_mentioned = path
            .strip_prefix(&cwd)
            .ok()
            .map(|rel| {
                let rel = rel.to_string_lossy().replace('\\', "/");
                mentioned.contains(&rel)
            })
            .unwrap_or(false);
        let depth_px = 8. + r.depth as f32 * 14.;
        div()
            .id(SharedString::from(format!("fs-{}", r.path.display())))
            .debug_selector(move || format!("fsrow-{}", r.name))
            .flex()
            .items_center()
            .gap_1()
            .pl(px(depth_px))
            .pr_2()
            .py_0p5()
            .rounded_sm()
            .text_size(px(11.5))
            .cursor_pointer()
            .when(sel, |d| d.bg(BG_ACTIVE))
            .when(is_mentioned, |d| d.border_l_2().border_color(BLUE))
            .hover(|s| s.bg(BG_HOVER))
            .on_click(cx.listener(move |this, _: &gpui::ClickEvent, window, cx| {
                let was_dir = path.is_dir();
                this.fs_selected = Some(path.clone());
                this.fs_delete_armed = None;
                this.explorer_focus.focus(window);
                if was_dir {
                    this.fs_toggle_dir(&path);
                } else {
                    this.fs_append_ref(&path, cx);
                }
                cx.notify();
            }))
            .on_mouse_down(
                gpui::MouseButton::Right,
                cx.listener(move |this, ev: &gpui::MouseDownEvent, window, cx| {
                    // 右键 = 选中 + 弹上下文菜单；坐标/翻折/clamp 统一在 open_context_menu
                    // （event.position 为窗口坐标，菜单挂窗口根层，菜单位置即鼠标位置）
                    this.fs_selected = Some(rc_path.clone());
                    this.fs_delete_armed = None;
                    this.open_context_menu(
                        rc_path.clone(),
                        ev.position.x.to_f64() as f32,
                        ev.position.y.to_f64() as f32,
                        {
                            let vp = window.viewport_size();
                            (vp.width.to_f64() as f32, vp.height.to_f64() as f32)
                        },
                        cx,
                    );
                    this.explorer_focus.focus(window);
                    cx.notify();
                }),
            )
            .child(div().child(file_icon(&r.name, r.is_dir, r.open)))
            .child(
                div()
                    .text_color(if sel { TEXT_1 } else { TEXT_2 })
                    .child(r.name.clone()),
            )
    }));

    tree = tree.child(div().flex_1());

    div()
        .w(px(240.))
        .flex_shrink_0()
        .flex()
        .flex_col()
        .bg(BG_SIDE)
        .border_l_1()
        .border_color(BORDER)
        .key_context("FileExplorer")
        .track_focus(&app.explorer_focus)
        .on_action(cx.listener(|this, _: &FsCopy, _, cx| {
            this.fs_copy_entry(false);
            cx.notify();
        }))
        .on_action(cx.listener(|this, _: &FsCut, _, cx| {
            this.fs_copy_entry(true);
            cx.notify();
        }))
        .on_action(cx.listener(|this, _: &FsPaste, _, cx| {
            this.fs_paste();
            cx.notify();
        }))
        .on_action(cx.listener(|this, _: &FsNewFile, _, cx| {
            this.fs_new(false);
            cx.notify();
        }))
        .on_action(cx.listener(|this, _: &FsNewDir, _, cx| {
            this.fs_new(true);
            cx.notify();
        }))
        .on_action(cx.listener(|this, _: &FsDelete, _, cx| {
            this.fs_delete();
            cx.notify();
        }))
        .on_action(cx.listener(|this, _: &FsSelUp, _, cx| {
            this.fs_move_selection(false);
            cx.notify();
        }))
        .on_action(cx.listener(|this, _: &FsSelDown, _, cx| {
            this.fs_move_selection(true);
            cx.notify();
        }))
        .on_action(cx.listener(|this, e: &FsToggleDir, _, cx| {
            let _ = e;
            this.fs_selected.clone().inspect(|p| {
                if p.is_dir() {
                    this.fs_toggle_dir(p);
                }
            });
            cx.notify();
        }))
        .child(
            div()
                .h_9()
                .flex()
                .items_center()
                .gap_2()
                .px_2()
                .border_b_1()
                .border_color(BORDER)
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(TEXT_3)
                        .child("工作区文件"),
                )
                .children(clipboard.map(|c| div().text_size(px(9.5)).text_color(BLUE).child(c)))
                .child(div().flex_1())
                .child(
                    div()
                        .id("btn-show-hidden")
                        .px_1p5()
                        .py_0p5()
                        .rounded_sm()
                        .text_size(px(10.5))
                        .tooltip(tooltip_view("切换隐藏文件显示"))
                        .text_color(if app.show_hidden { ACCENT } else { TEXT_3 })
                        .cursor_pointer()
                        .hover(|s| s.bg(BG_HOVER))
                        .on_click(cx.listener(|this, _: &gpui::ClickEvent, _, cx| {
                            this.show_hidden = !this.show_hidden;
                            // @ 补全的 file_tree 同步尊重 show_hidden
                            this.file_tree = scan_file_tree(&this.cwd, this.show_hidden);
                            cx.notify();
                        }))
                        .child(if app.show_hidden {
                            "● 隐藏"
                        } else {
                            "◌ 隐藏"
                        }),
                )
                .child(toolbar),
        )
        .child(
            div()
                .id("fs-tree")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .child(tree),
        )
}

/// hover 文字提示视图（gpui 0.2 tooltip API：InteractiveElement::tooltip 构建任意视图，
/// 无内置 Tooltip 组件，自绘最小视图）
struct TextTooltip(SharedString);

impl gpui::Render for TextTooltip {
    fn render(&mut self, _: &mut gpui::Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .rounded_sm()
            .border_1()
            .border_color(BORDER_STRONG)
            .bg(BG_ACTIVE)
            .px_2()
            .py_1()
            .text_size(px(11.))
            .text_color(TEXT_1)
            .child(self.0.clone())
    }
}

fn tooltip_view(tip: &'static str) -> impl Fn(&mut gpui::Window, &mut gpui::App) -> gpui::AnyView {
    move |_, cx| cx.new(|_| TextTooltip(tip.into())).into()
}

/// 工具条小按钮（icon + hover 文字提示）
fn fs_btn(
    id: &str,
    icon: &str,
    tip: &'static str,
    on_click: impl Fn(&gpui::ClickEvent, &mut gpui::Window, &mut gpui::App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(SharedString::from(format!("fs-btn-{id}")))
        .debug_selector(move || format!("fsbtn-{}", id))
        .px_1p5()
        .py_0p5()
        .rounded_sm()
        .text_size(px(11.))
        .text_color(TEXT_2)
        .cursor_pointer()
        .hover(|s| s.bg(BG_HOVER))
        .tooltip(tooltip_view(tip))
        .on_click(on_click)
        .child(icon.to_string())
}

/// 删除按钮：武装态显示确认字样
fn armed_label(armed: &Option<PathBuf>) -> String {
    if armed.is_some() {
        "确认?".to_string()
    } else {
        "🗑".to_string()
    }
}

// ---------- 右键菜单 / 重命名弹窗渲染 ----------

/// 重命名弹窗（居中小卡：TextInput 预填 + 确认/取消）。
/// 文件/会话两类重命名共用（状态互斥，同一挂载点渲染）；确认按钮按目标分发。
pub(crate) fn render_rename_modal(
    app: &mut App,
    cx: &mut Context<App>,
) -> Option<gpui::AnyElement> {
    // 会话重命名优先（两状态互斥，不会同时存在）
    let is_session = app.session_rename.is_some();
    let input = if is_session {
        app.session_rename.as_ref()?.input.clone()
    } else {
        app.rename_state.as_ref()?.input.clone()
    };
    let overlay = div()
        .absolute()
        .size_full()
        .top_0()
        .left_0()
        .bg(gpui::hsla(0., 0., 0., 0.5))
        .flex()
        .items_center()
        .justify_center()
        .child(
            div()
                .w(px(360.))
                .rounded_xl()
                .border_1()
                .border_color(BORDER_STRONG)
                .bg(BG_PANEL)
                .p_4()
                .flex()
                .flex_col()
                .gap_3()
                .child(div().text_size(px(13.)).child("✏️ 重命名"))
                .child(
                    div()
                        .rounded_md()
                        .border_1()
                        .border_color(BORDER_STRONG)
                        .bg(rgb(0x222228))
                        .overflow_hidden()
                        .child(input),
                )
                .child(
                    div()
                        .flex()
                        .justify_end()
                        .gap_2()
                        .child(
                            div()
                                .id("btn-rename-cancel")
                                .px_3()
                                .py_1p5()
                                .rounded_md()
                                .border_1()
                                .border_color(BORDER_STRONG)
                                .text_size(px(12.))
                                .text_color(TEXT_2)
                                .cursor_pointer()
                                .hover(|s| s.bg(BG_HOVER))
                                .on_click(cx.listener(|this, _: &gpui::ClickEvent, _, cx| {
                                    this.session_rename = None;
                                    this.rename_state = None;
                                    cx.notify();
                                }))
                                .child("取消"),
                        )
                        .child(
                            div()
                                .id("btn-rename-confirm")
                                .debug_selector(|| "rename-confirm".to_string())
                                .px_3()
                                .py_1p5()
                                .rounded_md()
                                .bg(ACCENT)
                                .text_size(px(12.))
                                .text_color(rgb(0xffffff))
                                .cursor_pointer()
                                .hover(|s| s.bg(BG_HOVER))
                                .on_click(cx.listener(move |this, _: &gpui::ClickEvent, _, cx| {
                                    // 按目标分发：会话标题 → confirm_session_rename；文件名 → confirm_rename
                                    if is_session {
                                        this.confirm_session_rename(cx);
                                    } else {
                                        this.confirm_rename(cx);
                                    }
                                    cx.notify();
                                }))
                                .child("确认"),
                        ),
                ),
        );
    Some(overlay.into_any_element())
}

// ---------- 文件树扫描（App 扫 cwd：深度 3，跳重目录，上限 600；@ 补全 respect show_hidden） ----------

pub(crate) fn scan_file_tree(cwd: &Path, show_hidden: bool) -> Vec<String> {
    let mut out = vec![];
    walk_dir(cwd, cwd, 0, show_hidden, &mut out);
    out
}

fn walk_dir(
    base: &std::path::Path,
    dir: &std::path::Path,
    depth: usize,
    show_hidden: bool,
    out: &mut Vec<String>,
) {
    if depth >= 3 || out.len() >= 600 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        if out.len() >= 600 {
            return;
        }
        let name = e.file_name().to_string_lossy().into_owned();
        if matches!(name.as_str(), "target" | "node_modules")
            || (!show_hidden && name.starts_with('.'))
        {
            continue;
        }
        let p = e.path();
        if p.is_dir() {
            out.push(format!(
                "{}/",
                e.path().strip_prefix(base).unwrap_or(&p).to_string_lossy()
            ));
            walk_dir(base, &p, depth + 1, show_hidden, out);
        } else if let Ok(rel) = p.strip_prefix(base) {
            out.push(rel.to_string_lossy().into_owned());
        }
    }
}
