//! 右键上下文菜单 — 状态（CtxMenu/CtxAction + SessMenu/SessAction）、动作执行
//! （run_context_action / run_session_action，渲染层与测试共用）、
//! 浮层渲染（统一挂在窗口根层 = render_app 顶层，保证盖住一切内容；菜单左上角 = 右键点位置，
//! 按视口边缘翻折 + 兜底 clamp）。

use gpui::{
    Context, InteractiveElement, SharedString, StatefulInteractiveElement, div, prelude::*, px,
};
use std::path::{Path, PathBuf};

use crate::app::App;
use crate::views::theme::*;

/// 右键上下文菜单状态（渲染为绝对定位浮层）
pub(crate) struct CtxMenu {
    pub path: PathBuf,
    pub is_dir: bool,
    /// 删除两段式确认（菜单内"确认删除？"红色确认态）
    pub delete_armed: bool,
    pub x: f32,
    pub y: f32,
}

/// 右键菜单动作（渲染层与测试共用 run_context_action）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CtxAction {
    Rename,
    Reveal,
    Delete,
    Copy,
    Cut,
    PasteInto,
    NewFile,
    NewDir,
}

/// 行内重命名弹窗（TextInput 预填当前名）
pub(crate) struct RenameState {
    pub path: PathBuf,
    pub input: gpui::Entity<crate::views::text_input::TextInput>,
}

/// 会话右键菜单状态（渲染为绝对定位浮层，与文件右键菜单互斥）
pub(crate) struct SessMenu {
    pub id: String,
    /// 删除两段式确认（菜单内"确认删除？"红色确认态）
    pub delete_armed: bool,
    pub x: f32,
    pub y: f32,
}

/// 会话右键菜单动作（渲染层与测试共用 run_session_action）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessAction {
    Rename,
    Delete,
}

/// 会话重命名弹窗（TextInput 预填当前标题；与文件重命名弹窗共用渲染挂载点）
pub(crate) struct SessRename {
    pub id: String,
    pub input: gpui::Entity<crate::views::text_input::TextInput>,
}

/// 菜单尺寸（会话菜单固定两项：重命名 / 删除）
const SESS_MENU_W: f32 = 170.;
const SESS_MENU_ITEM_H: f32 = 30.;

/// 菜单定位：左上角 = 右键点，按视口边缘翻折 + 兜底 clamp（文件/会话菜单共用）。
/// viewport 由右键 listener 用 window.viewport_size() 传入；测试传合成尺寸。
pub(crate) fn menu_position(
    x: f32,
    y: f32,
    menu_w: f32,
    menu_h: f32,
    viewport: (f32, f32),
) -> (f32, f32) {
    let margin = 8.;
    let mut mx = x;
    let mut my = y;
    // 右缘/下缘向内翻折
    if mx + menu_w > viewport.0 - margin {
        mx = x - menu_w - 4.;
    }
    if my + menu_h > viewport.1 - margin {
        my = y - menu_h - 4.;
    }
    let mx = mx.clamp(margin, (viewport.0 - menu_w - margin).max(margin));
    let my = my.clamp(margin, (viewport.1 - menu_h - margin).max(margin));
    (mx, my)
}

/// 窗口视口尺寸（右键 listener 取坐标用；测试传合成尺寸的对照入口）
pub(crate) fn window_viewport(window: &gpui::Window) -> (f32, f32) {
    let vp = window.viewport_size();
    (vp.width.to_f64() as f32, vp.height.to_f64() as f32)
}

impl App {
    // ---------- 右键菜单（坐标 clamp + 动作执行，供渲染层与测试直调） ----------

    /// 打开右键菜单：菜单左上角 = 右键点位置，定位细节在 menu_position。
    pub(crate) fn open_context_menu(
        &mut self,
        path: PathBuf,
        x: f32,
        y: f32,
        viewport: (f32, f32),
        _cx: &mut Context<Self>,
    ) {
        // 菜单与重命名弹窗互斥（用户实测：菜单打开时改名弹窗残留漏出）
        self.rename_state = None;
        let item_h = SESS_MENU_ITEM_H;
        let menu_h = if path.is_dir() {
            8. * item_h + 12.
        } else {
            5. * item_h + 12.
        };
        let (mx, my) = menu_position(x, y, 170., menu_h, viewport);
        let is_dir = path.is_dir();
        self.context_menu = Some(crate::views::context_menu::CtxMenu {
            path,
            is_dir,
            delete_armed: false,
            x: mx,
            y: my,
        });
    }

    // ---------- 会话右键菜单（重命名 / 删除，供渲染层与测试直调） ----------

    /// 打开会话右键菜单（与文件菜单/两类重命名弹窗互斥）
    pub(crate) fn open_session_menu(
        &mut self,
        id: String,
        x: f32,
        y: f32,
        viewport: (f32, f32),
        _cx: &mut Context<Self>,
    ) {
        self.context_menu = None;
        self.rename_state = None;
        self.session_rename = None;
        let (mx, my) = menu_position(x, y, SESS_MENU_W, 2. * SESS_MENU_ITEM_H + 12., viewport);
        self.session_menu = Some(crate::views::context_menu::SessMenu {
            id,
            delete_armed: false,
            x: mx,
            y: my,
        });
    }

    /// 会话菜单动作执行；返回 true = 关闭菜单。
    /// Delete 两段式同文件菜单：第一次点仅武装（菜单内"确认删除？"），第二次点执行。
    pub(crate) fn run_session_action(
        &mut self,
        action: crate::views::context_menu::SessAction,
        id: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        use crate::views::context_menu::SessAction;
        match action {
            SessAction::Rename => {
                self.open_session_rename(id, cx);
                true
            }
            SessAction::Delete => {
                let armed = self
                    .session_menu
                    .as_ref()
                    .is_some_and(|m| m.id == id && m.delete_armed);
                if !armed {
                    if let Some(m) = self.session_menu.as_mut().filter(|m| m.id == id) {
                        m.delete_armed = true;
                    }
                    return false;
                }
                self.delete_session(id, cx);
                true
            }
        }
    }

    /// 删除会话：jsonl + 标题 sidecar 落盘删除；删的是当前会话则重建 harness（新会话）。
    pub(crate) fn delete_session(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(store) = self.store.clone() else {
            self.push_notice("✗ 会话存储未初始化".to_string(), true);
            cx.notify();
            return;
        };
        match crate::app::extension_manage::delete_session(&store, id) {
            Ok(_) => {
                if self.current_session_id().as_deref() == Some(id) {
                    self.rebuild_harness(cx);
                }
                self.refresh_sessions();
                self.push_notice("✓ 会话已删除".to_string(), false);
            }
            Err(e) => self.push_notice(format!("✗ 删除会话失败：{e}"), true),
        }
        cx.notify();
    }

    /// 当前活动会话 id（harness 会话句柄路径派生）
    pub(crate) fn current_session_id(&self) -> Option<String> {
        self.harness
            .as_ref()
            .map(|h| crate::app::extension_manage::session_id_of(&h.session))
    }

    /// 打开会话重命名弹窗（TextInput 预填当前标题；全新会话无标题缓存 → 默认名）
    pub(crate) fn open_session_rename(&mut self, id: &str, cx: &mut Context<Self>) {
        let title = self
            .session_title_of(id)
            .unwrap_or_else(|| "未命名会话".to_string());
        let input = self.rename_input(cx, &title);
        self.session_rename = Some(crate::views::context_menu::SessRename {
            id: id.to_string(),
            input,
        });
    }

    /// 会话标题（自定义 sidecar 标题优先，其次列表缓存）
    fn session_title_of(&self, id: &str) -> Option<String> {
        if let Some(s) = self.store.as_ref()
            && let Some(t) = crate::app::extension_manage::custom_session_title(s, id)
        {
            return Some(t);
        }
        self.sessions
            .iter()
            .find(|m| m.session_id == id)
            .and_then(|m| m.title.clone())
    }

    /// 确认重命名：sidecar 落盘 + 列表刷新（列表标题派生自 sidecar）
    pub(crate) fn confirm_session_rename(&mut self, cx: &mut Context<Self>) {
        let Some(st) = self.session_rename.as_ref() else {
            return;
        };
        let id = st.id.clone();
        let title = st.input.read(cx).text().trim().to_string();
        self.session_rename = None;
        // 空标题拒绝：弹窗已关，sidecar 标题保持
        if title.is_empty() {
            self.push_notice("✗ 标题不能为空".to_string(), true);
            cx.notify();
            return;
        }
        match self.store.as_ref() {
            Some(store) => {
                crate::app::extension_manage::rename_session(store, &id, Some(&title));
                self.refresh_sessions();
                self.push_notice(format!("✓ 已重命名 → {title}"), false);
            }
            None => self.push_notice("✗ 会话存储未初始化".to_string(), true),
        }
        cx.notify();
    }

    /// 菜单动作执行（渲染层与测试共用）；返回 true = 关闭菜单。
    /// Delete 两段式：第一次点仅武装（菜单内显示"确认删除？"，菜单保持打开），第二次点执行。
    pub(crate) fn run_context_action(
        &mut self,
        action: crate::views::context_menu::CtxAction,
        path: &std::path::Path,
        cx: &mut Context<Self>,
    ) -> bool {
        use crate::views::context_menu::CtxAction;
        // 菜单动作的作用目标 = 右键路径（fs_* 系列读 fs_selected，先对齐）
        self.fs_selected = Some(path.to_path_buf());
        match action {
            CtxAction::Rename => {
                self.open_rename(path.to_path_buf(), cx);
                true
            }
            CtxAction::Delete => {
                let armed = self.fs_delete_armed.as_ref() == Some(&path.to_path_buf());
                if !armed {
                    // 武装：fs_delete_armed 置位 + 菜单内确认态，菜单保持打开
                    self.fs_delete_armed = Some(path.to_path_buf());
                    if let Some(m) = self.context_menu.as_mut() {
                        m.delete_armed = true;
                    }
                    false
                } else {
                    self.fs_delete();
                    true
                }
            }
            CtxAction::Reveal => {
                let mut cmd = build_reveal_command(path);
                if let Err(e) = cmd.spawn() {
                    self.push_notice(format!("⚠ 无法打开文件管理器: {e}"), true);
                }
                true
            }
            CtxAction::Copy => {
                self.fs_copy_entry(false);
                true
            }
            CtxAction::Cut => {
                self.fs_copy_entry(true);
                true
            }
            CtxAction::PasteInto => {
                // 粘贴目标 = 右键的目录本身
                if path.is_dir() {
                    self.fs_selected = Some(path.to_path_buf());
                }
                self.fs_paste();
                true
            }
            CtxAction::NewFile => {
                self.fs_selected = Some(path.to_path_buf());
                self.fs_new(false);
                true
            }
            CtxAction::NewDir => {
                self.fs_selected = Some(path.to_path_buf());
                self.fs_new(true);
                true
            }
        }
    }
}

/// 右键菜单浮层（全屏遮罩捕获点击关闭 + 绝对定位菜单面板，挂在窗口根层）
pub(crate) fn render_context_menu(
    app: &mut App,
    cx: &mut Context<App>,
) -> Option<gpui::AnyElement> {
    let menu = app.context_menu.as_ref()?;
    let path = menu.path.clone();
    let x = px(menu.x);
    let y = px(menu.y);

    // 删除两段式：第一次点后本菜单内显示红色"确认删除？"
    let del_label = if menu.delete_armed {
        "🗑 确认删除？"
    } else {
        "🗑 删除"
    };
    let reveal_label: &str = if cfg!(target_os = "windows") {
        "🔎 在 Explorer 中显示"
    } else {
        "🔎 在 Finder 中显示"
    };

    let menu_panel = div()
        .min_w(px(170.))
        .absolute()
        .left(x)
        .top(y)
        // 真实事件路由修复：面板吞掉 bubble 的 mouse_down，阻止遮罩先关菜单
        // （否则菜单在 down→up 之间卸载，on_click 的 down+up 配对落空 → 菜单项点击无效）
        // 真实事件路由修复：面板吞掉 bubble 的 mouse_down，阻止遮罩先关菜单
        // （否则菜单在 down→up 之间卸载，on_click 的 down+up 配对落空 → 菜单项点击无效——
        //   已由 case30 真实事件测试先复现红、后转绿验证）
        .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_mouse_down(gpui::MouseButton::Right, |_, _, cx| cx.stop_propagation())
        .rounded_md()
        .border_1()
        .border_color(BORDER_STRONG)
        .bg(BG_PANEL)
        .py_1()
        .flex()
        .flex_col()
        .shadow_lg()
        .child(action_item("✏️ 重命名", CtxAction::Rename, &path, cx))
        .child(
            action_item(del_label, CtxAction::Delete, &path, cx).when(menu.delete_armed, |d| {
                d.text_color(RED).font_weight(gpui::FontWeight::BOLD)
            }),
        )
        .child(action_item(reveal_label, CtxAction::Reveal, &path, cx))
        .child(action_item("⧉ 复制", CtxAction::Copy, &path, cx))
        .child(action_item("✂ 剪切", CtxAction::Cut, &path, cx));

    let menu_panel = if menu.is_dir {
        menu_panel
            .child(action_item(
                "📋 粘贴到此处",
                CtxAction::PasteInto,
                &path,
                cx,
            ))
            .child(action_item("＋ 新建文件", CtxAction::NewFile, &path, cx))
            .child(action_item("＋ 新建目录", CtxAction::NewDir, &path, cx))
    } else {
        menu_panel
    };

    let overlay = div()
        .id("ctx-menu-overlay")
        .absolute()
        .size_full()
        .top_0()
        .left_0()
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(|this, _: &gpui::MouseDownEvent, _, cx| {
                // 遮罩点击：复位删除武装态并关闭菜单
                this.fs_delete_armed = None;
                this.context_menu = None;
                cx.notify();
            }),
        )
        .child(menu_panel);

    Some(overlay.into_any_element())
}

/// 菜单项（min_w 全宽 + 悬停高亮；点击统一走 run_context_action，返回是否关闭菜单）
fn action_item(
    label: &str,
    action: crate::views::context_menu::CtxAction,
    path: &Path,
    cx: &mut Context<App>,
) -> gpui::Stateful<gpui::Div> {
    let p = path.to_path_buf();
    div()
        .id(SharedString::from(format!("menu-{}", label)))
        .debug_selector(move || format!("menuitem-{}", label))
        .w_full()
        .px_3()
        .py_1p5()
        .text_size(px(12.))
        .text_color(TEXT_1)
        .cursor_pointer()
        .hover(|s| s.bg(BG_HOVER))
        .on_click(cx.listener(move |this, _: &gpui::ClickEvent, _, cx| {
            let close = this.run_context_action(action, &p, cx);
            if close {
                this.context_menu = None;
            }
            cx.notify();
        }))
        .child(label.to_string())
}

/// 会话右键菜单浮层（全屏遮罩捕获点击关闭 + 绝对定位菜单面板；与文件菜单同构：
/// 面板吞掉 bubble 的 mouse_down，防止遮罩先关菜单导致 on_click 的 down+up 配对落空）
pub(crate) fn render_session_menu(
    app: &mut App,
    cx: &mut Context<App>,
) -> Option<gpui::AnyElement> {
    let menu = app.session_menu.as_ref()?;
    let id = menu.id.clone();
    let x = px(menu.x);
    let y = px(menu.y);

    // 删除两段式：第一次点后本菜单内显示红色"确认删除？"
    let del_label = if menu.delete_armed {
        "🗑 确认删除？"
    } else {
        "🗑 删除"
    };

    let menu_panel = div()
        .min_w(px(SESS_MENU_W))
        .absolute()
        .left(x)
        .top(y)
        .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_mouse_down(gpui::MouseButton::Right, |_, _, cx| cx.stop_propagation())
        .rounded_md()
        .border_1()
        .border_color(BORDER_STRONG)
        .bg(BG_PANEL)
        .py_1()
        .flex()
        .flex_col()
        .shadow_lg()
        .child(sess_item(
            "✏️ 重命名",
            crate::views::context_menu::SessAction::Rename,
            &id,
            cx,
        ))
        .child(
            sess_item(
                del_label,
                crate::views::context_menu::SessAction::Delete,
                &id,
                cx,
            )
            .when(menu.delete_armed, |d| {
                d.text_color(RED).font_weight(gpui::FontWeight::BOLD)
            }),
        );

    let overlay = div()
        .id("sess-menu-overlay")
        .absolute()
        .size_full()
        .top_0()
        .left_0()
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(|this, _: &gpui::MouseDownEvent, _, cx| {
                this.session_menu = None;
                cx.notify();
            }),
        )
        .child(menu_panel);

    Some(overlay.into_any_element())
}

/// 会话菜单项（点击统一走 run_session_action，返回是否关闭菜单）
fn sess_item(
    label: &str,
    action: crate::views::context_menu::SessAction,
    id: &str,
    cx: &mut Context<App>,
) -> gpui::Stateful<gpui::Div> {
    let id = id.to_string();
    div()
        .id(SharedString::from(format!("sess-menu-{}", label)))
        .debug_selector(move || format!("sessmenuitem-{}", action_debug_name(action)))
        .w_full()
        .px_3()
        .py_1p5()
        .text_size(px(12.))
        .text_color(TEXT_1)
        .cursor_pointer()
        .hover(|s| s.bg(BG_HOVER))
        .on_click(cx.listener(move |this, _: &gpui::ClickEvent, _, cx| {
            let close = this.run_session_action(action, &id, cx);
            if close {
                this.session_menu = None;
            }
            cx.notify();
        }))
        .child(label.to_string())
}

fn action_debug_name(action: crate::views::context_menu::SessAction) -> &'static str {
    match action {
        crate::views::context_menu::SessAction::Rename => "rename",
        crate::views::context_menu::SessAction::Delete => "delete",
    }
}

/// 在系统文件管理器中定位条目：macOS `open -R`（Finder 反显），Windows `explorer /select,`
pub(crate) fn build_reveal_command(path: &Path) -> std::process::Command {
    if cfg!(target_os = "windows") {
        let mut c = std::process::Command::new("explorer");
        c.arg(format!("/select,{}", path.display()));
        c
    } else {
        let mut c = std::process::Command::new("open");
        c.arg("-R").arg(path);
        c
    }
}
