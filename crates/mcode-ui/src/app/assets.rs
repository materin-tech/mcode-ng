// MCode Desktop — AI coding agent desktop app
// Copyright (c) 2026 Materin-tech. All rights reserved.
// SPDX-License-Identifier: MIT

//! 桌面端自带的基础设施：打包资产物化、MCODE.md 记忆文档路径与 /init。
//! （新 core 只保留通用面；这些桌面分发语义留在 ui crate。）

use std::path::PathBuf;

/// 项目级 MCODE.md 记忆文档搜索 + 全局（最近优先：cwd → 根，对齐 CLI mdoc.ts）
pub fn mcode_md_paths(cwd: &std::path::Path) -> Vec<PathBuf> {
    let mut out = vec![];
    let mut cur = Some(cwd.to_path_buf());
    while let Some(d) = cur {
        out.push(d.join("MCODE.md"));
        cur = d.parent().map(|p| p.to_path_buf());
    }
    if let Some(dir) = mcode_core::util::mcode_dir() {
        out.push(dir.join("MCODE.md"));
    }
    out
}

/// /init 模板（逐字对齐 TS memory/mdoc.ts 的 TEMPLATE）
pub const MCODE_MD_TEMPLATE: &str = "# MCODE.md

Project-level instructions for MCode. Loaded into the system prompt on every session.

## Build & test

<!-- e.g. npm run build / npm test -->

## Conventions

<!-- code style, naming, directory layout -->

## Notes

<!-- anything the agent should always know -->
";

/// 生成 MCODE.md（已存在则不覆盖，返回 (路径, 是否新建)）
pub fn init_mcode_doc(cwd: &std::path::Path) -> std::io::Result<(PathBuf, bool)> {
    let path = cwd.join("MCODE.md");
    if path.exists() {
        return Ok((path, false));
    }
    std::fs::write(&path, MCODE_MD_TEMPLATE)?;
    Ok((path, true))
}

/// 打包内资产的运行时定位：macOS .app Resources → 可执行文件旁 assets/ → 仓库源码路径（cargo run 兜底）
pub fn assets_source_dir() -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        // .app/Contents/MacOS/exe → .app/Contents/Resources/assets
        if let Some(res) = exe
            .parent()
            .and_then(|p| p.parent())
            .map(|p| p.join("Resources/assets"))
            && res.is_dir()
        {
            return Some(res);
        }
        if let Some(adj) = exe.parent().map(|p| p.join("assets"))
            && adj.is_dir()
        {
            return Some(adj);
        }
    }
    // cargo run / 开发态：仓库根 assets
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../assets");
    if repo.is_dir() {
        return Some(repo);
    }
    None
}

/// 内置资产一次性物化到 ~/.mcode/assets（已存在则跳过，删除后重新落地可强制更新）
pub fn materialize_bundled_assets() -> usize {
    let Some(src) = assets_source_dir() else {
        return 0;
    };
    let Some(dst_root) = mcode_core::util::mcode_dir() else {
        return 0;
    };
    let dst_root = dst_root.join("assets");
    let mut n = 0;
    for tier in ["ecc-core", "pua-core"] {
        let from = src.join(tier);
        let to = dst_root.join(tier);
        if !from.is_dir() || to.is_dir() {
            continue;
        }
        if copy_tree(&from, &to).is_ok() {
            n += 1;
        }
    }
    n
}

/// 递归复制目录（物化与 skill 安装共用）
pub fn copy_tree(from: &std::path::Path, to: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let t = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &t)?;
        } else {
            std::fs::copy(entry.path(), &t)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_mcode_doc_creates_then_skips() {
        let dir = std::env::temp_dir().join(format!("mcode_ui_init_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (p1, created1) = init_mcode_doc(&dir).unwrap();
        assert!(created1);
        assert!(p1.ends_with("MCODE.md"));
        let content1 = std::fs::read_to_string(&p1).unwrap();
        assert!(content1.starts_with("# MCODE.md\n"));
        // 二次调用不覆盖
        std::fs::write(&p1, "user content").unwrap();
        let (_, created2) = init_mcode_doc(&dir).unwrap();
        assert!(!created2);
        assert_eq!(std::fs::read_to_string(&p1).unwrap(), "user content");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn md_paths_walk_upward_then_global() {
        let dir = std::env::temp_dir().join(format!("mcode_ui_md_{}", std::process::id()));
        std::fs::create_dir_all(dir.join("a/b")).unwrap();
        let paths = mcode_md_paths(&dir.join("a/b"));
        assert!(paths[0].ends_with("a/b/MCODE.md"));
        assert!(paths[1].ends_with("a/MCODE.md"));
        // 全局 ~/.mcode/MCODE.md 收尾
        assert!(paths.last().unwrap().ends_with(".mcode/MCODE.md"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
