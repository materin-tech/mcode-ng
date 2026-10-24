//! `/` 与 `@` 补全：触发检测、候选过滤、接受插入（CC 式菜单数据源）。
//!
//! `list_dir` 沿用旧实现（单层列举、目录在前、跳过依赖目录与点文件）。

use std::path::Path;

use super::input::InputBuffer;

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

/// 候选展示上限（CC 菜单也是有限列表）。
pub const MAX_CANDIDATES: usize = 20;

/// 菜单同时可见行数。
pub const MAX_VISIBLE: usize = 8;

/// 补全候选。`insert` 是接受后拼接在触发字符之后的文本（当前层粒度：
/// 目录带尾 `/`）；`display` 是弹窗里的展示名。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionCandidate {
    pub insert: String,
    pub display: String,
    pub is_dir: bool,
    /// 斜杠命令的说明（菜单右侧 dim 列）
    pub description: String,
}

impl CompletionCandidate {
    fn file(insert: String, display: String, is_dir: bool) -> Self {
        CompletionCandidate {
            insert,
            display,
            is_dir,
            description: String::new(),
        }
    }
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
        out.push(CompletionCandidate::file(
            format!("{d}/"),
            format!("{d}/"),
            true,
        ));
    }
    for f in files {
        out.push(CompletionCandidate::file(f.clone(), f, false));
    }
    out.truncate(MAX_ENTRIES);
    out
}

/// 打开中的补全状态。
#[derive(Debug, Clone)]
pub struct CompletionState {
    /// 触发 token 的字节起点（接受时替换 input[at..cursor]）
    pub at: usize,
    /// '/' = 命令，'@' = 文件
    pub trigger: char,
    /// '@' 时已确认的目录前缀
    pub dir_part: String,
    pub candidates: Vec<CompletionCandidate>,
    pub selected: usize,
}

impl CompletionState {
    /// 按当前输入与光标刷新补全；不触发则 None。
    pub fn refresh(
        input: &InputBuffer,
        slash_commands: &[(String, String)],
        root: &Path,
    ) -> Option<Self> {
        let cursor = input.cursor;
        let text = input.text();
        if cursor > text.len() {
            return None;
        }
        let token_start = text[..cursor]
            .rfind(char::is_whitespace)
            .map(|i| i + 1)
            .unwrap_or(0);
        let token = &text[token_start..cursor];
        if let Some(rest) = token.strip_prefix('/') {
            if token_start != 0 {
                return None; // 斜杠命令只认首词
            }
            let query = rest.to_lowercase();
            let candidates: Vec<_> = slash_commands
                .iter()
                .filter(|(name, _)| name.to_lowercase().starts_with(&query))
                .take(MAX_CANDIDATES)
                .map(|(name, desc)| CompletionCandidate {
                    insert: format!("{name} "),
                    display: name.clone(),
                    is_dir: false,
                    description: desc.clone(),
                })
                .collect();
            if candidates.is_empty() {
                return None;
            }
            return Some(CompletionState {
                at: token_start,
                trigger: '/',
                dir_part: String::new(),
                candidates,
                selected: 0,
            });
        }
        if let Some(rest) = token.strip_prefix('@') {
            let (dir_part, prefix) = match rest.rfind('/') {
                Some(i) => (&rest[..=i], &rest[i + 1..]),
                None => ("", rest),
            };
            let candidates: Vec<_> = list_dir(root, dir_part)
                .into_iter()
                .filter(|c| c.display.starts_with(prefix))
                .take(MAX_CANDIDATES)
                .collect();
            if candidates.is_empty() {
                return None;
            }
            return Some(CompletionState {
                at: token_start,
                trigger: '@',
                dir_part: dir_part.to_string(),
                candidates,
                selected: 0,
            });
        }
        None
    }

    pub fn up(&mut self) {
        self.selected = (self.selected + self.candidates.len() - 1) % self.candidates.len();
    }

    pub fn down(&mut self) {
        self.selected = (self.selected + 1) % self.candidates.len();
    }

    /// 接受当前候选：改写 input 并返回是否保持打开（目录继续下钻）。
    pub fn accept(&self, input: &mut InputBuffer) -> bool {
        let Some(c) = self.candidates.get(self.selected) else {
            return false;
        };
        let cursor = input.cursor;
        let mut replacement = String::new();
        replacement.push(self.trigger);
        if self.trigger == '@' {
            replacement.push_str(&self.dir_part);
        }
        replacement.push_str(&c.insert);
        // replace_range 只动 [at, cursor)，光标后的文本原位保留
        input.buf.replace_range(self.at..cursor, &replacement);
        input.cursor = self.at + replacement.len();
        input.hist_pos = None;
        // 目录候选：保持打开继续下钻；文件/命令：闭合
        c.is_dir
    }

    /// 可见窗口起点（选中项尽量贴近窗口尾部，超出才滚动）。
    fn window_start(&self) -> usize {
        let len = self.candidates.len();
        if len <= MAX_VISIBLE {
            0
        } else {
            self.selected
                .saturating_sub(MAX_VISIBLE - 1)
                .min(len - MAX_VISIBLE)
        }
    }

    /// 可见窗口（围绕 selected）。
    pub fn visible(&self) -> &[CompletionCandidate] {
        let len = self.candidates.len();
        let start = self.window_start();
        &self.candidates[start..(start + MAX_VISIBLE).min(len)]
    }

    /// 可见窗口首元素在 candidates 里的下标（菜单选中标记用）。
    pub fn visible_first_offset(&self) -> usize {
        self.window_start()
    }
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
        for p in ["node_modules/pkg/index.js", "target/debug/a.out", ".hidden"] {
            let f = tmp.join(p);
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, "x").unwrap();
        }
        tmp
    }

    fn input_at(text: &str) -> InputBuffer {
        let mut b = InputBuffer::new();
        b.set_text(text);
        b
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
        assert!(list_dir(&root, "../").is_empty());
        assert!(list_dir(&root, "src/../../").is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_dir_yields_empty() {
        assert!(list_dir(Path::new("/nonexistent-mcode-zz"), "").is_empty());
    }

    #[test]
    fn slash_completion_filters_by_prefix_and_accepts() {
        let root = make_tree();
        let slash = vec![
            ("help".to_string(), "帮助".to_string()),
            ("history".to_string(), "历史".to_string()),
            ("model".to_string(), "模型".to_string()),
        ];
        let mut b = input_at("/h");
        b.cursor = 2;
        let st = CompletionState::refresh(&b, &slash, &root).unwrap();
        assert_eq!(st.trigger, '/');
        assert_eq!(st.candidates.len(), 2, "help/history 命中 /h");
        let keep = st.accept(&mut b);
        assert!(!keep, "命令接受后闭合");
        assert_eq!(b.text(), "/help ");
        assert_eq!(b.cursor, "/help ".len());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn slash_only_triggers_as_first_word() {
        let root = make_tree();
        let slash = vec![("help".to_string(), String::new())];
        let input = input_at("请用 /he");
        assert!(
            CompletionState::refresh(&input, &slash, &root).is_none(),
            "非首词 / 不触发命令补全"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn at_completion_drills_directories_and_accepts_files() {
        let root = make_tree();
        let mut b = input_at("@sr");
        b.cursor = b.buf.len();
        let st = CompletionState::refresh(&b, &[], &root).unwrap();
        assert_eq!(st.trigger, '@');
        assert_eq!(st.candidates.len(), 1);
        assert_eq!(st.candidates[0].display, "src/");
        assert!(st.candidates[0].is_dir);
        let keep = st.accept(&mut b);
        assert!(keep, "目录接受后保持打开下钻");
        assert_eq!(b.text(), "@src/");
        assert_eq!(b.cursor, b.buf.len());

        // 下钻到文件
        let st = CompletionState::refresh(&b, &[], &root).unwrap();
        assert_eq!(st.dir_part, "src/");
        assert_eq!(
            st.candidates
                .iter()
                .map(|c| c.display.as_str())
                .collect::<Vec<_>>(),
            vec!["lib.rs", "main.rs"]
        );
        let mut st = st;
        st.down();
        st.accept(&mut b);
        assert_eq!(b.text(), "@src/main.rs");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn no_trigger_yields_none() {
        let root = make_tree();
        let b = input_at("普通文本");
        assert!(CompletionState::refresh(&b, &[], &root).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn visible_window_follows_selection() {
        let mut st = CompletionState {
            at: 0,
            trigger: '/',
            dir_part: String::new(),
            candidates: (0..12)
                .map(|i| CompletionCandidate {
                    insert: format!("c{i}"),
                    display: format!("c{i}"),
                    is_dir: false,
                    description: String::new(),
                })
                .collect(),
            selected: 0,
        };
        assert_eq!(st.visible().len(), MAX_VISIBLE);
        assert_eq!(st.visible()[0].display, "c0");
        for _ in 0..7 {
            st.down();
        }
        assert_eq!(st.visible().last().unwrap().display, "c7", "窗口跟随选中项");
        st.up();
        assert!(st.visible().iter().any(|c| c.display == "c6"));
    }
}
