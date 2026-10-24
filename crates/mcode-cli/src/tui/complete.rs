//! @ 文件补全的数据源：按目录层级实时列举（对齐 CC 的层级浏览体验）。
//!
//! 每次刷新只 `read_dir` 一层——目录在前（带 `/` 尾）、文件在后，跳过
//! 依赖/构建产物目录与点文件；匹配与候选状态在 state 层做（可测），
//! 这里只负责单层收集。实时列举天然动态：新建/删除的文件立即反映到候选，
//! 也不再有「全量递归 2000 条平铺」的大杂烩。

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

/// 单层最多收集条数（防异常目录拖慢 UI；候选展示层另有 20 条封顶）。
const MAX_ENTRIES: usize = 500;

/// 补全候选。`insert` 是接受后拼接在触发字符之后的文本（当前层粒度：
/// 目录带尾 `/`）；`display` 是弹窗里的展示名。state 层接受时会把
/// 已确认的目录前缀（`dir_part`）拼进 insert。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionCandidate {
    pub insert: String,
    pub display: String,
    pub is_dir: bool,
}

/// 列举 `root` 下相对目录 `rel_dir`（'/' 分隔）这一层的内容。
/// 目录在前、文件在后，各按名字排序；拒绝 `..` 与绝对分量（防逃逸，
/// 列目录不许越出工作区根）。
pub fn list_dir(root: &Path, rel_dir: &str) -> Vec<CompletionCandidate> {
    let mut dir = root.to_path_buf();
    for comp in rel_dir.split('/') {
        match comp {
            "" | "." => {}
            ".." => return Vec::new(),
            name => dir.push(name),
        }
    }
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut dirs: Vec<String> = Vec::new();
    let mut files: Vec<String> = Vec::new();
    for e in rd.filter_map(|e| e.ok()) {
        let Ok(ft) = e.file_type() else { continue };
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        if ft.is_dir() {
            if !SKIP_DIRS.contains(&name.as_str()) {
                dirs.push(name);
            }
        } else {
            files.push(name);
        }
    }
    dirs.sort();
    files.sort();
    let mut out: Vec<CompletionCandidate> = Vec::with_capacity(dirs.len() + files.len());
    for d in dirs {
        out.push(CompletionCandidate {
            insert: format!("{d}/"),
            display: format!("{d}/"),
            is_dir: true,
        });
    }
    for f in files {
        out.push(CompletionCandidate {
            insert: f.clone(),
            display: f,
            is_dir: false,
        });
    }
    out.truncate(MAX_ENTRIES);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn make_tree() -> PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = std::env::temp_dir().join(format!("mcode-complete-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        for p in ["src/main.rs", "src/lib.rs", "README.md", "docs/guide.md"] {
            let f = tmp.join(p);
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, "x").unwrap();
        }
        // 应跳过的目录与点文件
        for p in ["node_modules/pkg/index.js", "target/debug/a.out", ".hidden"] {
            let f = tmp.join(p);
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, "x").unwrap();
        }
        tmp
    }

    #[test]
    fn lists_one_level_dirs_first_skipping_junk() {
        let root = make_tree();
        let entries = list_dir(&root, "");
        let dirs: Vec<_> = entries
            .iter()
            .filter(|e| e.is_dir)
            .map(|e| e.display.clone())
            .collect();
        let files: Vec<_> = entries
            .iter()
            .filter(|e| !e.is_dir)
            .map(|e| e.display.clone())
            .collect();
        assert_eq!(dirs, vec!["docs/", "src/"], "目录在前带尾斜杠");
        assert_eq!(files, vec!["README.md"], "点文件不列");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn drills_into_subdir_and_rejects_escape() {
        let root = make_tree();
        let src: Vec<_> = list_dir(&root, "src")
            .iter()
            .map(|e| e.display.clone())
            .collect();
        assert_eq!(src, vec!["lib.rs", "main.rs"], "子层文件按名排序");

        // `..` 越出工作区：拒绝（不给列）
        assert!(list_dir(&root, "../").is_empty());
        assert!(list_dir(&root, "src/../../").is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_dir_yields_empty() {
        assert!(list_dir(Path::new("/nonexistent-mcode-zz"), "").is_empty());
    }
}
