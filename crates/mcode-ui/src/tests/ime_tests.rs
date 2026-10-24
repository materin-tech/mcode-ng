// MCode_Desktop — AI coding agent desktop app
// Copyright (c) 2026 Materin-tech. All rights reserved.
// SPDX-License-Identifier: MIT

//! IME 组词守卫与消息区滚动跟随的状态机测试：
//! - 组词期（marked text 未上屏）退格/回车必须交给输入法，不得删已上屏内容、不得误发送
//! - 上翻暂停跟随的三个恢复路径（新建/清空/切换会话）都要把跟随重置回贴底

use crate::views::text_input::{self as ti, TextInput};
use gpui::{EntityInputHandler, TestAppContext};

fn make_input_window(cx: &mut TestAppContext) -> gpui::WindowHandle<TextInput> {
    cx.add_window(|_, cx| TextInput {
        focus_handle: cx.focus_handle(),
        content: "".into(),
        placeholder: "测试".into(),
        selected_range: 0..0,
        selection_reversed: false,
        marked_range: None,
        last_layout: None,
        last_bounds: None,
        is_selecting: false,
    })
}

/// 中文输入法组词期：退格/回车交给输入法，不删已上屏内容、不当发送；
/// 上屏（unmark）后恢复正常行为。
#[gpui::test]
fn ime_composition_guards_backspace_and_submit(cx: &mut TestAppContext) {
    let window = make_input_window(cx);
    window
        .update(cx, |input, window, cx| {
            input.set_text("已上屏", cx);

            // 模拟 IME 组词：光标处挂 marked text（未上屏）
            input.replace_and_mark_text_in_range(None, "ni", None, window, cx);
            assert_eq!(input.text(), "已上屏ni", "组词文本应临时拼入内容");
            assert!(input.marked_range.is_some(), "组词状态未置位");

            // 组词期：退格/回车属于输入法，应用侧守卫必须全部拦截
            input.backspace(&ti::Backspace, window, cx);
            assert_eq!(input.text(), "已上屏ni", "组词期退格不得删已上屏内容");
            input.submit(&ti::Submit, window, cx);
            assert_eq!(input.text(), "已上屏ni", "组词期回车不得当发送清空");

            // 外部整体重设内容（补全应用/历史召回）必须连带清组词态：
            // 残留会让 gpui 后端按"组词中"把按键全导向 IME，输入框假死
            input.set_text("重设", cx);
            assert!(input.marked_range.is_none(), "set_text 后组词状态不得残留");

            // 上屏（unmark 后守卫解除）→ 重建已知状态，验证退格/回车恢复正常
            input.unmark_text(window, cx);
            assert!(input.marked_range.is_none(), "上屏后组词状态应清除");
            input.set_text("abc", cx);
            input.backspace(&ti::Backspace, window, cx);
            assert_eq!(input.text(), "ab", "上屏后退格应恢复正常删除");
            input.submit(&ti::Submit, window, cx);
            assert_eq!(input.text(), "", "非组词期回车发送后应清空");
        })
        .unwrap();
}

/// MS 拼音逐次组词更新（每次带相对组词串的显式 caret 选区）下，selected_range
/// 必须塌缩在组词尾部且不越界：越界非塌缩区间会让 prepaint 走高亮分支画方块
/// 而非光标条（"光标停在字中间"），还埋下切片越界隐患。
#[gpui::test]
fn ime_composition_updates_keep_selection_collapsed(cx: &mut TestAppContext) {
    let window = make_input_window(cx);
    window
        .update(cx, |input, window, cx| {
            input.set_text("已上屏", cx); // utf8 len 9

            // 组词第 1 帧："n"，IME 给 caret = 组词串内 utf16 1..1
            input.replace_and_mark_text_in_range(None, "n", Some(1..1), window, cx);
            assert_eq!(input.text(), "已上屏n");
            assert_eq!(input.selected_range, 10..10);

            // 组词第 2 帧：替换旧 marked（utf16 3..4）为 "ni"，caret 2..2。
            // 修复前：选区换算误加 range.end → selected 被推成 11..12（越界且不塌缩）
            input.replace_and_mark_text_in_range(Some(3..4), "ni", Some(2..2), window, cx);
            assert_eq!(input.text(), "已上屏ni");
            assert_eq!(input.selected_range, 11..11, "组词选区应塌缩在组词尾部");
            assert_eq!(input.marked_range, Some(9..11), "marked 应落在新组词串上");
            assert_eq!(
                input.marked_text_range(window, cx),
                Some(3..5),
                "键盘闸门读到的 utf16 组词区间应有效"
            );
        })
        .unwrap();
}

/// 残留失效 marked（entity 与真实 IME 脱钩）必须自动降级解闸：gpui Windows
/// 后端以 marked_text_range() 作"组词中"键盘闸门，废组词态不解闸会把方向键/
/// 空格全数导向已不存在的组词会话（表现为按键没效果）。点击输入框同理要清。
#[gpui::test]
fn ime_stale_marked_gate_self_heals(cx: &mut TestAppContext) {
    let window = make_input_window(cx);
    window
        .update(cx, |input, window, cx| {
            input.set_text("abc", cx);
            // 模拟脱钩残留：越界 marked
            input.marked_range = Some(99..102);
            assert_eq!(
                input.marked_text_range(window, cx),
                None,
                "越界残留不得再当组词态（闸门应解）"
            );
            input.backspace(&ti::Backspace, window, cx);
            assert_eq!(input.text(), "ab", "解闸后退格应恢复正常删除");

            // 点击输入框即结束组词：entity 侧同步清 marked，防闸门滞留
            input.replace_and_mark_text_in_range(None, "de", None, window, cx);
            assert!(input.marked_range.is_some(), "前置：组词态应已挂上");
            input.on_mouse_down(
                &gpui::MouseDownEvent {
                    button: gpui::MouseButton::Left,
                    position: gpui::point(gpui::px(0.), gpui::px(0.)),
                    modifiers: Default::default(),
                    click_count: 1,
                    first_mouse: true,
                },
                window,
                cx,
            );
            assert!(input.marked_range.is_none(), "点击后组词态应清除");
        })
        .unwrap();
}

/// 上翻暂停跟随的三个恢复路径（新建/清空/切换会话）都要把跟随重置回贴底。
#[gpui::test]
fn scroll_follow_resets_on_new_chat_clear_and_switch(cx: &mut TestAppContext) {
    let c = crate::tests::interaction_tests::setup(cx, vec![]);
    c.app.update(cx, |a, cx| {
        // 1) 新建会话
        a.chat_follow = false;
        a.new_chat(cx);
        assert!(a.chat_follow, "新建会话即恢复滚动跟随");

        // 2) 切换/重载会话（/clear 会结束会话，放最后验证）
        let id = a.current_session_id().expect("new_chat 后应有新会话");
        a.chat_follow = false;
        a.resume_session(&id, cx);
        assert!(a.chat_follow, "切换会话应恢复滚动跟随");

        // 3) /clear 命令
        a.chat_follow = false;
        assert!(a.handle_command("/clear", cx));
        assert!(a.chat_follow, "/clear 应恢复滚动跟随");
    });
    // 清理 new_chat 落盘的会话（store.dir() 即 bucket 本身）
    if let Ok(store) = mcode_core::sessions::SessionStore::for_project(&c.tmp) {
        let _ = std::fs::remove_dir_all(store.dir());
    }
}
