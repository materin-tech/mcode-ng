// MCode_Desktop — AI coding agent desktop app
// Copyright (c) 2026 Materin-tech. All rights reserved.
// SPDX-License-Identifier: MIT

//! Windows 资源嵌入：图标 + 版本信息（仅 windows 目标构建时生效，macOS 走 icns）。

fn main() {
    if std::env::var("CARGO_CFG_WINDOWS").is_ok() {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("../../assets/brand/icon.ico");
        res.set("FileDescription", "MCode Desktop — AI coding agent");
        res.set("ProductName", "MCode Desktop");
        if let Err(e) = res.compile() {
            // 图标嵌入失败不阻断构建（exe 退化为无图标）
            println!("cargo:warning=winresource 嵌入失败: {e}");
        }
    }
    println!("cargo:rerun-if-changed=../../assets/brand/icon.ico");
}
