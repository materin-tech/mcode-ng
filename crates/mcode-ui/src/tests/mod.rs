// MCode_Desktop — AI coding agent desktop app
// Copyright (c) 2026 Materin-tech. All rights reserved.
// SPDX-License-Identifier: MIT

//! 测试矩阵入口：状态机级（interaction_tests）+ 文件浏览器/右键菜单（fs_menu_tests）
//! + 真实事件路径（real_event_tests）。`#[gpui::test]` 驱动，CI 无头运行。

pub mod at_mention_tests;
pub mod fs_menu_tests;
pub mod ime_tests;
pub mod interaction_tests;
pub mod real_event_tests;
