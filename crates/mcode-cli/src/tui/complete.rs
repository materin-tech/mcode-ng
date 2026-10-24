//! @ 文件补全的数据源：递归枚举工作区文件（相对路径）。
//!
//! 对齐 CC 的 `@path` 体验：跳过依赖/构建产物目录，限深限量防大仓库卡顿；
//! 匹配在 state 层做（可测），这里只负责收集。

use std::path::Path;

/// 跳过的目录名（依赖/构建/版本控制）。
const SKIP_DIRS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    "node_modules",
    "target",
    "dist",
    "build",
    "__pycache__",
    ".venv",
    "venv",
    ".mcode",
    ".idea",
    ".vscode",
    ".next",
];

/// 最大收集条数与递归深度（防超大仓库拖慢 UI）。
const MAX_FILES: usize = 2000;
const MAX_DEPTH: usize = 8;

/// 递归枚举 `root` 下的文件，返回相对路径（`/` 分隔，排序确定序）。
pub fn collect_workspace_files(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    walk(root, root, 0, &mut out);
    out.sort();
    out
}

fn walk(root: &Path, dir: &Path, depth: usize, out: &mut Vec<String>) {
    if depth > MAX_DEPTH || out.len() >= MAX_FILES {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = rd.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let Ok(ft) = e.file_type() else { continue };
        let name = e.file_name().to_string_lossy().to_string();
        let path = e.path();
        if ft.is_dir() {
            if SKIP_DIRS.contains(&name.as_str()) {
                continue;
            }
            walk(root, &path, depth + 1, out);
        } else if ft.is_file() {
            if out.len() >= MAX_FILES {
                return;
            }
            if let Ok(rel) = path.strip_prefix(root) {
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_tree(root: &Path) {
        for p in ["src/main.rs", "src/lib.rs", "README.md", "docs/guide.md"] {
            let f = root.join(p);
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, "x").unwrap();
        }
        // 应跳过的目录
        let skip = root.join("node_modules/pkg/index.js");
        std::fs::create_dir_all(skip.parent().unwrap()).unwrap();
        std::fs::write(skip, "x").unwrap();
        let skip2 = root.join("target/debug/a.out");
        std::fs::create_dir_all(skip2.parent().unwrap()).unwrap();
        std::fs::write(skip2, "x").unwrap();
    }

    #[test]
    fn collects_relative_paths_skipping_build_dirs() {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = std::env::temp_dir().join(format!("mcode-complete-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        make_tree(&tmp);

        let files = collect_workspace_files(&tmp);
        assert_eq!(
            files,
            vec![
                "README.md".to_string(),
                "docs/guide.md".to_string(),
                "src/lib.rs".to_string(),
                "src/main.rs".to_string(),
            ],
            "{files:?}"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn missing_root_yields_empty() {
        assert!(collect_workspace_files(Path::new("/nonexistent-mcode-zz")).is_empty());
    }
}
