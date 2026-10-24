// MCode_Desktop — AI coding agent desktop app
// Copyright (c) 2026 Materin-tech. All rights reserved.
// SPDX-License-Identifier: MIT

//! 主题 — 颜色语义 1:1 对齐 desktop-prototype/css/app.css 设计令牌
//! rgb() 在 gpui 0.2 非 const fn，故用结构体字面量保持 const

use gpui::Rgba;

pub const BG_WINDOW: Rgba = Rgba {
    r: 17. / 255.,
    g: 17. / 255.,
    b: 20. / 255.,
    a: 1.,
};
pub const BG_SIDE: Rgba = Rgba {
    r: 23. / 255.,
    g: 23. / 255.,
    b: 27. / 255.,
    a: 1.,
};
pub const BG_MAIN: Rgba = Rgba {
    r: 28. / 255.,
    g: 28. / 255.,
    b: 33. / 255.,
    a: 1.,
};
pub const BG_PANEL: Rgba = Rgba {
    r: 34. / 255.,
    g: 34. / 255.,
    b: 40. / 255.,
    a: 1.,
};
pub const BG_HOVER: Rgba = Rgba {
    r: 42. / 255.,
    g: 42. / 255.,
    b: 49. / 255.,
    a: 1.,
};
pub const BG_ACTIVE: Rgba = Rgba {
    r: 50. / 255.,
    g: 50. / 255.,
    b: 59. / 255.,
    a: 1.,
};
pub const BORDER: Rgba = Rgba {
    r: 46. / 255.,
    g: 46. / 255.,
    b: 54. / 255.,
    a: 1.,
};
pub const BORDER_STRONG: Rgba = Rgba {
    r: 61. / 255.,
    g: 61. / 255.,
    b: 71. / 255.,
    a: 1.,
};
pub const TEXT_1: Rgba = Rgba {
    r: 232. / 255.,
    g: 232. / 255.,
    b: 236. / 255.,
    a: 1.,
};
pub const TEXT_2: Rgba = Rgba {
    r: 168. / 255.,
    g: 168. / 255.,
    b: 179. / 255.,
    a: 1.,
};
pub const TEXT_3: Rgba = Rgba {
    r: 109. / 255.,
    g: 109. / 255.,
    b: 120. / 255.,
    a: 1.,
};
pub const ACCENT: Rgba = Rgba {
    r: 217. / 255.,
    g: 119. / 255.,
    b: 87. / 255.,
    a: 1.,
};
pub const BLUE: Rgba = Rgba {
    r: 91. / 255.,
    g: 155. / 255.,
    b: 213. / 255.,
    a: 1.,
};
pub const GREEN: Rgba = Rgba {
    r: 111. / 255.,
    g: 191. / 255.,
    b: 115. / 255.,
    a: 1.,
};
pub const YELLOW: Rgba = Rgba {
    r: 217. / 255.,
    g: 161. / 255.,
    b: 68. / 255.,
    a: 1.,
};
pub const RED: Rgba = Rgba {
    r: 224. / 255.,
    g: 85. / 255.,
    b: 85. / 255.,
    a: 1.,
};
pub const PURPLE: Rgba = Rgba {
    r: 160. / 255.,
    g: 119. / 255.,
    b: 212. / 255.,
    a: 1.,
};

/// 权限模式语义色（红/黄/蓝/紫，与原型与 CLI 状态栏一致）
pub fn mode_color(m: mcode_core::permission::Mode) -> Rgba {
    use mcode_core::permission::Mode::*;
    match m {
        Manual => YELLOW,
        Auto => BLUE,
        Plan => PURPLE,
        Passon => RED,
    }
}
