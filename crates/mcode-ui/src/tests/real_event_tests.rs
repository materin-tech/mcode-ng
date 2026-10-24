//! 真实事件级测试 — gpui TestContext 真窗口 + simulate_mouse_down/up 完整分发：
//! 覆盖"状态机直调绿但真机失败"的事件路由断点（遮罩先吃 down → 菜单卸载 → on_click 落空）。

use crate::app::App;
use gpui::{MouseButton, TestAppContext, point};
use std::path::PathBuf;

/// 带真窗口的测试环境：tempdir cwd + 预置文件（首帧即渲染行）
fn windowed_setup(
    cx: &mut TestAppContext,
    files: &[&str],
    dirs: &[&str],
) -> (gpui::Entity<App>, PathBuf, gpui::VisualTestContext) {
    let tmp = crate::tests::interaction_tests::make_temp_dir();
    for f in files {
        std::fs::write(tmp.join(f), "data").unwrap();
    }
    for d in dirs {
        std::fs::create_dir_all(tmp.join(d)).unwrap();
    }
    let window = cx.add_window(|_, cx| {
        let mut a = App::new_for_test(cx);
        a.cfg = crate::tests::interaction_tests::test_config();
        a.cwd = tmp.clone();
        a.file_tree = crate::views::file_explorer::scan_file_tree(&a.cwd, a.show_hidden);
        a.refresh_sessions();
        a
    });
    let view = window.root(cx).unwrap();
    let visual = gpui::VisualTestContext::from_window(window.into(), cx);
    (view, tmp, visual)
}

fn click(cx: &mut gpui::VisualTestContext, pos: gpui::Point<gpui::Pixels>, button: MouseButton) {
    cx.simulate_mouse_down(pos, button, gpui::Modifiers::none());
    cx.simulate_mouse_up(pos, button, gpui::Modifiers::none());
}

fn center(b: gpui::Bounds<gpui::Pixels>) -> gpui::Point<gpui::Pixels> {
    point(
        b.origin.x + b.size.width / 2.,
        b.origin.y + b.size.height / 2.,
    )
}

fn cleanup(tmp: &PathBuf) {
    let _ = std::fs::remove_dir_all(tmp);
}

// ---------- 用例 30：真实事件右键 → 菜单 → 删除两段式 → 文件消失 ----------
#[gpui::test]
async fn case30_real_events_delete_via_context_menu(cx: &mut TestAppContext) {
    let (view, tmp, mut cx) = windowed_setup(cx, &["victim.txt"], &[]);
    // ① 右键文件行（坐标来自元素真实 bounds）
    let row = cx.debug_bounds("fsrow-victim.txt").expect("行元素未渲染");
    let click_pos = center(row);
    click(&mut cx, click_pos, MouseButton::Right);
    cx.run_until_parked();
    // 菜单打开且坐标 = 右键点（视口内 ±2px；贴右缘时按翻折规则左移且完整入窗）
    let vp_w = 1920.; // 测试窗口 = Bounds::maximized → 1920×1080
    view.update(&mut cx, |a, _| {
        let m = a.context_menu.as_ref().expect("右键后菜单未打开");
        let cx_f = click_pos.x.to_f64() as f32;
        let cy_f = click_pos.y.to_f64() as f32;
        if cx_f + 170. <= vp_w - 8. {
            assert!(
                (m.x - cx_f).abs() <= 2. && (m.y - cy_f).abs() <= 2.,
                "菜单坐标 != 右键点"
            );
        } else {
            // 翻折：菜单完整入窗
            assert!(
                m.x + 170. <= vp_w - 8. + 2. && m.x <= cx_f + 2.,
                "翻折后仍出窗或右移: x={}",
                m.x
            );
        }
    });
    // ② 左键点"删除"菜单项
    let del = cx
        .debug_bounds("menuitem-🗑 删除")
        .expect("删除菜单项未渲染");
    click(&mut cx, center(del), MouseButton::Left);
    cx.run_until_parked();
    // ③ 菜单仍打开 + 武装（真实路由下两段式的第一段）
    view.update(&mut cx, |a, _| {
        let m = a
            .context_menu
            .as_ref()
            .expect("第一次点击后菜单被关闭（真实路由断点）");
        assert!(m.delete_armed, "武装未置位");
    });
    let confirm = cx
        .debug_bounds("menuitem-🗑 确认删除？")
        .expect("确认删除项未渲染");
    click(&mut cx, center(confirm), MouseButton::Left);
    cx.run_until_parked();
    // ④ 文件已删除 + 菜单关闭
    view.update(&mut cx, |a, _| {
        assert!(a.context_menu.is_none(), "删除完成后菜单未关闭");
    });
    assert!(
        !tmp.join("victim.txt").exists(),
        "文件未删除（真实事件下删除无效）"
    );
    cleanup(&tmp);
}

// ---------- 用例 31：真实事件重命名链（菜单 → 弹窗 → 确认按钮） ----------
#[gpui::test]
async fn case31_real_events_rename_via_menu(cx: &mut TestAppContext) {
    let (view, tmp, mut cx) = windowed_setup(cx, &["before.txt"], &[]);
    let row = cx.debug_bounds("fsrow-before.txt").expect("行元素未渲染");
    click(&mut cx, center(row), MouseButton::Right);
    cx.run_until_parked();
    let item = cx
        .debug_bounds("menuitem-✏️ 重命名")
        .expect("重命名菜单项未渲染");
    click(&mut cx, center(item), MouseButton::Left);
    cx.run_until_parked();
    // 弹窗打开，输入框预填
    view.update(&mut cx, |a, cx| {
        let st = a.rename_state.as_ref().expect("重命名弹窗未出现");
        assert_eq!(st.input.read(cx).text(), "before.txt", "输入框未预填当前名");
        st.input.update(cx, |i, cx| i.set_text("after.txt", cx));
    });
    let confirm = cx.debug_bounds("rename-confirm").expect("确认按钮未渲染");
    click(&mut cx, center(confirm), MouseButton::Left);
    cx.run_until_parked();
    view.update(&mut cx, |a, _| {
        assert!(a.rename_state.is_none(), "确认后弹窗未关闭");
    });
    assert!(
        !tmp.join("before.txt").exists() && tmp.join("after.txt").exists(),
        "重命名未生效"
    );
    cleanup(&tmp);
}

// ---------- 用例 32：真实事件复制（工具条） → 粘贴（右键菜单） 链路 ----------
#[gpui::test]
async fn case32_real_events_copy_toolbar_paste_menu(cx: &mut TestAppContext) {
    let (view, tmp, mut cx) = windowed_setup(cx, &["src.txt"], &["dst_dir"]);
    // ① 选中文件行（左键，会展开目录/引用——文件则仅选中）→ 工具条复制按钮
    let row = cx.debug_bounds("fsrow-src.txt").expect("行元素未渲染");
    click(&mut cx, center(row), MouseButton::Left);
    cx.run_until_parked();
    let copy_btn = cx.debug_bounds("fsbtn-copy").expect("复制按钮未渲染");
    click(&mut cx, center(copy_btn), MouseButton::Left);
    cx.run_until_parked();
    view.update(&mut cx, |a, _| {
        assert!(a.fs_clipboard.is_some(), "复制未入剪贴板");
    });
    // ② 右键目录行 → 菜单 → 粘贴到此处
    let drow = cx.debug_bounds("fsrow-dst_dir").expect("目录行未渲染");
    click(&mut cx, center(drow), MouseButton::Right);
    cx.run_until_parked();
    let paste = cx
        .debug_bounds("menuitem-📋 粘贴到此处")
        .expect("粘贴菜单项未渲染");
    click(&mut cx, center(paste), MouseButton::Left);
    cx.run_until_parked();
    // ③ 目标副本存在（首贴无后缀）
    assert!(
        tmp.join("dst_dir").join("src.txt").exists(),
        "粘贴副本未出现"
    );
    assert_eq!(
        std::fs::read(tmp.join("dst_dir").join("src.txt")).unwrap(),
        b"data"
    );
    cleanup(&tmp);
}

// ---------- 用例 33：真实事件删除非空目录（用户 v0.1.4 实测失败场景） ----------
#[gpui::test]
async fn case33_real_events_delete_directory(cx: &mut TestAppContext) {
    let (view, tmp, mut cx) = windowed_setup(cx, &[], &["victim_dir"]);
    // 非空：嵌套子目录 + 文件
    std::fs::create_dir_all(tmp.join("victim_dir").join("nested")).unwrap();
    std::fs::write(tmp.join("victim_dir").join("nested").join("f.txt"), "x").unwrap();
    std::fs::write(tmp.join("victim_dir").join("top.txt"), "y").unwrap();
    cx.run_until_parked();
    // ① 先左键展开目录（对齐真实用户操作：浏览过目录内容后再删除）
    let row = cx.debug_bounds("fsrow-victim_dir").expect("目录行未渲染");
    click(&mut cx, center(row), MouseButton::Left);
    cx.run_until_parked();
    view.update(&mut cx, |a, _| {
        assert!(
            a.expanded_dirs.iter().any(|d| d.ends_with("victim_dir")),
            "前置：目录未展开"
        );
    });
    // ② 右键目录行
    let row = cx.debug_bounds("fsrow-victim_dir").expect("目录行未渲染");
    click(&mut cx, center(row), MouseButton::Right);
    cx.run_until_parked();
    view.update(&mut cx, |a, _| {
        let m = a.context_menu.as_ref().expect("目录菜单未打开");
        assert!(m.is_dir, "菜单未按目录渲染（应有 7 项）");
    });
    // ② 两段式删除
    let del = cx.debug_bounds("menuitem-🗑 删除").expect("删除项未渲染");
    click(&mut cx, center(del), MouseButton::Left);
    cx.run_until_parked();
    view.update(&mut cx, |a, _| {
        let m = a.context_menu.as_ref().expect("第一次点击后菜单被关闭");
        assert!(m.delete_armed, "目录删除未武装");
    });
    let confirm = cx
        .debug_bounds("menuitem-🗑 确认删除？")
        .expect("确认删除项未渲染");
    click(&mut cx, center(confirm), MouseButton::Left);
    cx.run_until_parked();
    // ③ 目录从文件系统消失（含嵌套内容）
    assert!(
        !tmp.join("victim_dir").exists(),
        "目录未删除（真实事件下目录删除无效——复现用户 v0.1.4 所见）"
    );
    // ④ expanded_dirs 无残留 + 树已刷新（目录不在行集内）
    view.update(&mut cx, |a, _| {
        assert!(
            !a.expanded_dirs.iter().any(|d| d.ends_with("victim_dir")),
            "expanded_dirs 残留已删目录"
        );
        let rows = a.fs_rows();
        assert!(
            !rows.iter().any(|r| r.path.ends_with("victim_dir")),
            "树未刷新，目录仍在行集"
        );
    });
    cleanup(&tmp);
}

// ---------- 用例 34：真实事件目录重命名（含展开状态保持） ----------
#[gpui::test]
async fn case34_real_events_rename_directory(cx: &mut TestAppContext) {
    let (view, tmp, mut cx) = windowed_setup(cx, &[], &["old_dir"]);
    std::fs::write(tmp.join("old_dir").join("inner.txt"), "i").unwrap();
    cx.run_until_parked();
    // 先左键展开目录（模拟用户操作过展开状态）
    let row = cx.debug_bounds("fsrow-old_dir").expect("目录行未渲染");
    click(&mut cx, center(row), MouseButton::Left);
    cx.run_until_parked();
    view.update(&mut cx, |a, _| {
        assert!(
            a.expanded_dirs.iter().any(|d| d.ends_with("old_dir")),
            "目录未展开"
        );
    });
    // 右键 → 重命名 → 弹窗 → 确认
    click(&mut cx, center(row), MouseButton::Right);
    cx.run_until_parked();
    let item = cx
        .debug_bounds("menuitem-✏️ 重命名")
        .expect("重命名项未渲染");
    click(&mut cx, center(item), MouseButton::Left);
    cx.run_until_parked();
    view.update(&mut cx, |a, cx| {
        let st = a.rename_state.as_ref().expect("重命名弹窗未出现");
        assert_eq!(st.input.read(cx).text(), "old_dir");
        st.input.update(cx, |i, cx| i.set_text("new_dir", cx));
    });
    let confirm = cx.debug_bounds("rename-confirm").expect("确认按钮未渲染");
    click(&mut cx, center(confirm), MouseButton::Left);
    cx.run_until_parked();
    assert!(
        !tmp.join("old_dir").exists() && tmp.join("new_dir").join("inner.txt").exists(),
        "目录重命名未生效"
    );
    // expanded_dirs 已迁移到新路径
    view.update(&mut cx, |a, _| {
        assert!(
            a.expanded_dirs.iter().any(|d| d.ends_with("new_dir")),
            "展开状态未迁移到新路径"
        );
        assert!(
            !a.expanded_dirs.iter().any(|d| d.ends_with("old_dir")),
            "旧路径残留 expanded_dirs"
        );
    });
    cleanup(&tmp);
}
