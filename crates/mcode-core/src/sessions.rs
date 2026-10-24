//! sessions —— 会话持久化：parent 链 JSONL、resume、文件快照、保留策略。
//!
//! transcript 布局：
//! - 每会话一个 JSONL：`~/.mcode/projects/<sanitized-cwd>/<session-id>.jsonl`；
//! - 每行一条记录，`type` 内部 tagged：
//!   `meta`（会话元信息，首行）→ `message`（父链消息）→ `snapshot`
//!   （文件快照，rewind 恢复数据源）；
//! - 解析宽容：坏行/未知 type 跳过不报错（协议前向兼容——旧版本文件
//!   里出现新记录类型不该炸掉 resume）；
//! - 文件快照在**写入前**捕获（路径 → 写前内容），rewind 到某检查点时
//!   按追加序的**逆序**恢复（后写的先还原），None 表示该文件原本不存在
//!   （rewind 时删除）。

use crate::message::{Conversation, Message, Role};
use crate::util::mcode_dir;
use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// 每项目保留的会话数上限（超出按文件 mtime 淘汰最旧）。
pub const MAX_SESSIONS_PER_PROJECT: usize = 50;
/// 会话标题截断长度（resume 列表展示用）。
const TITLE_MAX_CHARS: usize = 60;

// ---------------------------------------------------------------------------
// 记录类型
// ---------------------------------------------------------------------------

/// 会话元信息（JSONL 首行）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub session_id: String,
    /// 启动会话时的工作区（绝对路径字符串）。
    pub cwd: String,
    /// Unix 毫秒。
    pub created_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// 单文件快照：某检查点（用户消息）开启的回合里，写入前该文件的旧内容。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileSnapshot {
    /// 快照归属的检查点（开启该回合的 user 消息 id）。
    pub checkpoint_id: String,
    pub path: PathBuf,
    /// 写入前的内容；`None` = 该文件原本不存在（rewind → 删除）。
    pub prior_content: Option<String>,
}

/// JSONL 行（内部 tagged；宽容解析——未知 type 落入 Unknown 被跳过）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Line {
    Meta(SessionMeta),
    Message(Message),
    Snapshot(FileSnapshot),
    /// 活跃叶指针（rewind 时追加）：resume 语义 = 「最后一条 Leaf 记录」。
    /// `None` = 叶已清空（rewind 到空会话）；**没有** Leaf 记录 = 叶取
    /// 最后一条主链消息（append-only 会话的自然语义）。两态必须区分。
    Leaf {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        leaf_id: Option<String>,
    },
    /// 未知/未来版本记录：解析成功但语义未知，加载时忽略。
    #[serde(other)]
    Unknown,
}

// ---------------------------------------------------------------------------
// SessionStore：目录组织 + start/list/load/prune
// ---------------------------------------------------------------------------

/// resume 列表条目（标题/条数从文件内容派生，避免维护易失的计数）。
#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub session_id: String,
    pub path: PathBuf,
    pub created_ms: i64,
    /// 文件 mtime（Unix 毫秒）——最近活跃时间。
    pub updated_ms: i64,
    /// 首条用户消息文本（截断）；无则 None。
    pub title: Option<String>,
    pub message_count: usize,
}

/// 会话存储（按项目目录分桶）。
#[derive(Clone)]
pub struct SessionStore {
    dir: PathBuf,
}

impl SessionStore {
    /// 生产入口：`~/.mcode/projects/<sanitized-cwd>/`。
    pub fn for_project(cwd: &Path) -> Result<Self> {
        let base = mcode_dir().context("无法定位 ~/.mcode（HOME 未设置）")?;
        Ok(Self {
            dir: base.join("projects").join(sanitize_dir_name(cwd)),
        })
    }

    /// 测试/自定义根目录。
    pub fn with_base(base: PathBuf) -> Self {
        Self {
            dir: base.join("projects").join("test-project"),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// 会话标题 sidecar（`<id>.title`，与 jsonl 同目录）。
    /// `Some` = 写入自定义标题；`None` = 删除 sidecar（回落首条用户消息）。
    pub fn set_title(&self, id: &str, title: Option<&str>) -> std::io::Result<()> {
        let sidecar = self.dir().join(format!("{id}.title"));
        match title {
            Some(t) => std::fs::write(sidecar, t.trim()),
            None => {
                let _ = std::fs::remove_file(sidecar);
                Ok(())
            }
        }
    }

    /// 追加打开会话文件（start/resume 共用；create 兼顾新文件首开）。
    fn open_append(&self, path: PathBuf) -> Result<SessionHandle> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("打开会话 {} 失败", path.display()))?;
        Ok(SessionHandle {
            path,
            file: Mutex::new(file),
        })
    }

    /// 开启新会话：建目录、写 meta 行，返回追加句柄。
    pub fn start(&self, cwd: &Path, model: Option<String>) -> Result<(SessionHandle, SessionMeta)> {
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("创建会话目录 {} 失败", self.dir.display()))?;
        let meta = SessionMeta {
            session_id: uuid::Uuid::new_v4().simple().to_string(),
            cwd: cwd.display().to_string(),
            created_ms: chrono::Utc::now().timestamp_millis(),
            model,
        };
        let handle = self.open_append(self.dir.join(format!("{}.jsonl", meta.session_id)))?;
        handle.append(&Line::Meta(meta.clone()))?;
        Ok((handle, meta))
    }

    /// 列出本项目会话（最近活跃在前），并执行保留策略（超出上限淘汰最旧）。
    pub fn list(&self) -> Vec<SessionSummary> {
        let mut summaries: Vec<SessionSummary> = std::fs::read_dir(&self.dir)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
                    .filter_map(|e| summarize(&e.path()))
                    .collect()
            })
            .unwrap_or_default();
        summaries.sort_by_key(|s| std::cmp::Reverse(s.updated_ms));
        // 保留策略：超出上限淘汰最旧（删除失败不致命）
        for s in summaries.iter().skip(MAX_SESSIONS_PER_PROJECT) {
            let _ = std::fs::remove_file(&s.path);
        }
        summaries.truncate(MAX_SESSIONS_PER_PROJECT);
        summaries
    }

    /// 按 id 加载会话：重建父链会话 + meta。坏行/未知行宽容跳过。
    /// 叶 = 最后一条 Leaf 记录（rewind 过的会话；None=清空）；没有 Leaf
    /// 记录则取最后一条主链消息（append-only 默认语义）。
    pub fn load(&self, session_id: &str) -> Result<(Conversation, Option<SessionMeta>)> {
        let path = self.dir.join(format!("{session_id}.jsonl"));
        let lines = read_lines(&path)?;
        let mut meta = None;
        let mut messages = Vec::new();
        let mut leaf_rec: Option<Option<String>> = None;
        for line in lines {
            match line {
                Line::Meta(m) => meta = Some(m),
                Line::Message(m) => messages.push(m),
                Line::Leaf { leaf_id: l } => leaf_rec = Some(l),
                Line::Snapshot(_) | Line::Unknown => {}
            }
        }
        let conv = match leaf_rec {
            // 无记录 → append-only 默认语义
            None => Conversation::from_messages(messages, None),
            // 显式清空（rewind 到空会话）
            Some(None) => Conversation {
                messages,
                leaf_id: None,
            },
            // 显式叶：指向的消息不存在（截断/损坏）时回落默认语义
            Some(Some(l)) if messages.iter().any(|m| m.id == l) => {
                Conversation::from_messages(messages, Some(l))
            }
            Some(Some(_)) => Conversation::from_messages(messages, None),
        };
        Ok((conv, meta))
    }

    /// 打开既有会话继续追加（resume 场景）。
    pub fn resume(&self, session_id: &str) -> Result<SessionHandle> {
        let path = self.dir.join(format!("{session_id}.jsonl"));
        anyhow::ensure!(
            path.exists(),
            "会话 {session_id} 不存在：{}",
            path.display()
        );
        self.open_append(path)
    }
}

/// 追加句柄：JSONL 逐行落盘（行缓冲 flush，进程崩溃最多丢最后一行）。
pub struct SessionHandle {
    path: PathBuf,
    file: Mutex<File>,
}

impl SessionHandle {
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 追加一条记录并 flush。
    pub fn append(&self, line: &Line) -> Result<()> {
        let mut json = serde_json::to_string(line)?;
        json.push('\n');
        let mut f = self.file.lock().expect("会话文件锁");
        f.write_all(json.as_bytes())
            .and_then(|_| f.flush())
            .with_context(|| format!("写会话 {} 失败", self.path.display()))
    }

    pub fn append_message(&self, m: &Message) {
        if let Err(e) = self.append(&Line::Message(m.clone())) {
            // 落盘失败不阻断对话（降级为无持久化），但要留痕
            eprintln!("mcode: 会话写入失败: {e}");
        }
    }

    pub fn append_snapshot(&self, s: &FileSnapshot) {
        if let Err(e) = self.append(&Line::Snapshot(s.clone())) {
            eprintln!("mcode: 会话快照写入失败: {e}");
        }
    }

    /// 记录活跃叶指针（rewind 后调用；resume 时据此恢复分支位置）。
    /// `None` = 叶清空（rewind 到空会话）。
    pub fn append_leaf(&self, leaf_id: Option<&str>) {
        let line = Line::Leaf {
            leaf_id: leaf_id.map(str::to_string),
        };
        if let Err(e) = self.append(&line) {
            eprintln!("mcode: 会话叶指针写入失败: {e}");
        }
    }
}

// ---------------------------------------------------------------------------
// 快照恢复（rewind 数据源）
// ---------------------------------------------------------------------------

/// 读整个 JSONL（宽容：坏行跳过）。
pub fn read_lines(path: &Path) -> Result<Vec<Line>> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("读会话 {} 失败", path.display()))?;
    Ok(raw
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<Line>(l).ok())
        .collect())
}

/// 取 `checkpoint_id` 之后追加的所有快照，**逆序**（rewind 恢复顺序：
/// 后写的先还原）。
pub fn snapshots_after(lines: &[Line], checkpoint_id: &str) -> Vec<FileSnapshot> {
    let start = lines
        .iter()
        .position(|l| matches!(l, Line::Message(m) if m.id == checkpoint_id));
    let Some(start) = start else {
        return vec![];
    };
    lines[start + 1..]
        .iter()
        .filter_map(|l| match l {
            Line::Snapshot(s) => Some(s.clone()),
            _ => None,
        })
        .rev()
        .collect()
}

/// 应用快照列表：写回旧内容；`prior_content=None` → 删除文件。
/// 返回成功恢复的文件数。按传入顺序执行（调用方传逆序列表）。
pub fn apply_snapshots(snaps: &[FileSnapshot]) -> usize {
    let mut restored = 0;
    for s in snaps {
        let ok = match &s.prior_content {
            Some(content) => std::fs::write(&s.path, content).is_ok(),
            // 文件已不在（幂等）：视为恢复成功
            None => match std::fs::remove_file(&s.path) {
                Ok(()) => true,
                Err(e) => e.kind() == std::io::ErrorKind::NotFound,
            },
        };
        if ok {
            restored += 1;
        }
    }
    restored
}

// ---------------------------------------------------------------------------
// 内部：目录名消毒 / 摘要 / 标题派生
// ---------------------------------------------------------------------------

/// cwd → 目录名：仅保留 `[A-Za-z0-9._-]`，其余替换为 `-`。
fn sanitize_dir_name(cwd: &Path) -> String {
    cwd.display()
        .to_string()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// 扫描单文件 → 摘要（文件损坏/不可读 → None，不拖垮列表）。
fn summarize(path: &Path) -> Option<SessionSummary> {
    let md = std::fs::metadata(path).ok()?;
    let updated_ms = {
        let t = md.modified().ok()?;
        t.duration_since(std::time::UNIX_EPOCH).ok()?.as_millis() as i64
    };
    let lines = read_lines(path).ok()?;
    let (session_id, created_ms) = lines.iter().find_map(|l| match l {
        Line::Meta(m) => Some((m.session_id.clone(), m.created_ms)),
        _ => None,
    })?;
    let session_id = if session_id.is_empty() {
        path.file_stem()?.to_string_lossy().to_string()
    } else {
        session_id
    };
    let title = lines.iter().find_map(|l| match l {
        Line::Message(m) if m.role == Role::User && !m.synthetic => {
            let t = m.text();
            (!t.trim().is_empty()).then(|| truncate_chars(&t, TITLE_MAX_CHARS))
        }
        _ => None,
    });
    let message_count = lines
        .iter()
        .filter(|l| matches!(l, Line::Message(_)))
        .count();
    Some(SessionSummary {
        session_id,
        path: path.to_path_buf(),
        created_ms,
        updated_ms,
        title,
        message_count,
    })
}

/// 按字符数截断（多字节安全），加省略号。
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{cut}…")
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::new_message_id;

    /// 唯一临时根目录（并发测试互不干扰）。
    fn tmp_base(tag: &str) -> PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("mcode-sessions-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn store(tag: &str) -> SessionStore {
        SessionStore::with_base(tmp_base(tag))
    }

    /// 对齐 runner 语义：消息先经 `Conversation::push` 挂上父链再落盘
    /// （SessionHandle 只记录，parent 由活跃会话状态决定）。
    fn record(conv: &mut Conversation, h: &SessionHandle, msg: Message) {
        conv.push(msg);
        h.append_message(conv.messages.last().unwrap());
    }

    /// sidechain 版：落盘但不动主链叶。
    fn record_sidechain(conv: &mut Conversation, h: &SessionHandle, msg: Message) {
        conv.push_sidechain(msg);
        h.append_message(conv.messages.last().unwrap());
    }

    #[test]
    fn start_append_load_roundtrip() {
        let st = store("roundtrip");
        let (h, meta) = st
            .start(Path::new("/tmp/proj"), Some("glm-5.3".into()))
            .unwrap();
        let mut conv = Conversation::new();
        record(&mut conv, &h, Message::user_text("第一问"));
        record(&mut conv, &h, Message::assistant_text("第一答"));

        let (conv, loaded) = st.load(&meta.session_id).unwrap();
        assert_eq!(loaded.unwrap().model.as_deref(), Some("glm-5.3"));
        let msgs = conv.active_messages();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].text(), "第一问");
        // 父链完整：第二条挂第一条下
        assert_eq!(msgs[1].parent_id.as_deref(), Some(msgs[0].id.as_str()));
    }

    #[test]
    fn leaf_record_controls_resume_branch() {
        let st = store("leafrec");
        let (h, meta) = st
            .start(Path::new("/tmp/proj"), Some("glm-5.3".into()))
            .unwrap();
        let mut conv = Conversation::new();
        record(&mut conv, &h, Message::user_text("一"));
        record(&mut conv, &h, Message::assistant_text("答一"));
        let a1 = conv.messages.last().unwrap().id.clone();
        record(&mut conv, &h, Message::user_text("二"));

        // rewind 到答一之前（叶=答一的 id；分支仍保留在日志）
        conv.rewind_to(&a1);
        h.append_leaf(Some(&a1));

        // resume 后活跃路径 = 一 → 答一
        let (conv2, _) = st.load(&meta.session_id).unwrap();
        let msgs = conv2.active_messages();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[1].text(), "答一");
        assert_eq!(conv2.messages.len(), 3, "旧分支日志保留");

        // rewind 到空会话：叶显式 None，与「无 Leaf 记录」区分
        h.append_leaf(None);
        let (conv3, _) = st.load(&meta.session_id).unwrap();
        assert!(conv3.active_path().is_empty());

        // resume 追加句柄语义不变（继续 append）
        let h2 = st.resume(&meta.session_id).unwrap();
        h2.append_message(&Message::assistant_text("新分支"));
        let (conv4, _) = st.load(&meta.session_id).unwrap();
        assert_eq!(conv4.messages.len(), 4);
    }

    #[tokio::test]
    async fn resume_continues_appending_and_leaf_moves_forward() {
        let st = store("resume");
        let (h, meta) = st.start(Path::new("/p"), None).unwrap();
        let mut conv = Conversation::new();
        record(&mut conv, &h, Message::user_text("q1"));
        record(&mut conv, &h, Message::assistant_text("a1"));

        let h2 = st.resume(&meta.session_id).unwrap();
        record(&mut conv, &h2, Message::user_text("q2"));
        record(&mut conv, &h2, Message::assistant_text("a2"));

        let (conv, _) = st.load(&meta.session_id).unwrap();
        let msgs = conv.active_messages();
        assert_eq!(msgs.len(), 4, "resume 追加的消息要接上父链");
        assert_eq!(msgs[3].parent_id.as_deref(), Some(msgs[2].id.as_str()));
        assert_eq!(msgs[3].text(), "a2");
    }

    #[test]
    fn sidechain_messages_excluded_from_active_path() {
        let st = store("sidechain");
        let (h, meta) = st.start(Path::new("/p"), None).unwrap();
        let mut conv = Conversation::new();
        record(&mut conv, &h, Message::user_text("主链"));
        record_sidechain(&mut conv, &h, Message::assistant_text("子代理思考"));
        record(&mut conv, &h, Message::assistant_text("主链继续"));

        let (conv, _) = st.load(&meta.session_id).unwrap();
        let msgs = conv.active_messages();
        assert_eq!(msgs.len(), 2, "sidechain 不进活跃路径");
        assert_eq!(msgs[1].text(), "主链继续");
        assert!(msgs.iter().all(|m| !m.sidechain));
    }

    #[test]
    fn list_derives_title_count_and_sorts_newest_first() {
        let st = store("list");
        let (h1, _m1) = st.start(Path::new("/p"), None).unwrap();
        let mut c1 = Conversation::new();
        record(&mut c1, &h1, Message::user_text("帮我修这个 bug"));
        record(&mut c1, &h1, Message::assistant_text("好的"));
        // 写入顺序决定 mtime：s1（最旧）→ s3（只写 meta）→ s2（最新）
        std::thread::sleep(std::time::Duration::from_millis(20));
        let (_, m3) = st.start(Path::new("/p"), None).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let (h2, m2) = st.start(Path::new("/p"), None).unwrap();
        let mut c2 = Conversation::new();
        record(&mut c2, &h2, Message::user_text("另一件事"));

        let list = st.list();
        assert_eq!(list.len(), 3);
        assert_eq!(list[0].session_id, m2.session_id, "最近活跃在前");
        assert_eq!(list[0].title.as_deref(), Some("另一件事"));
        assert_eq!(list[1].session_id, m3.session_id);
        assert_eq!(list[1].title, None, "空会话无标题");
        assert_eq!(list[1].message_count, 0);
        assert_eq!(list[2].title.as_deref(), Some("帮我修这个 bug"));
        assert_eq!(list[2].message_count, 2);
    }

    #[test]
    fn title_truncated_with_ellipsis() {
        let st = store("trunc");
        let (h, meta) = st.start(Path::new("/p"), None).unwrap();
        let long = "长".repeat(200);
        let mut conv = Conversation::new();
        record(&mut conv, &h, Message::user_text(long));

        let list = st.list();
        let t = list
            .iter()
            .find(|s| s.session_id == meta.session_id)
            .unwrap();
        let title = t.title.as_deref().unwrap();
        assert_eq!(title.chars().count(), TITLE_MAX_CHARS + 1, "截断加省略号");
        assert!(title.ends_with('…'));
    }

    #[test]
    fn retention_prunes_oldest_beyond_cap() {
        let st = store("prune");
        for i in 0..(MAX_SESSIONS_PER_PROJECT + 3) {
            let (h, _) = st.start(Path::new("/p"), None).unwrap();
            let mut conv = Conversation::new();
            record(&mut conv, &h, Message::user_text(format!("会话 {i}")));
            // mtime 毫秒粒度内可能打平，错开保证"最新"判定确定
            std::thread::sleep(std::time::Duration::from_millis(3));
        }
        let list = st.list();
        assert_eq!(
            list.len(),
            MAX_SESSIONS_PER_PROJECT,
            "超出上限的最旧会话被淘汰"
        );
        // 最新的保留
        assert_eq!(list[0].title.as_deref().unwrap(), "会话 52");
    }

    #[test]
    fn snapshots_after_returns_reverse_restore_order() {
        let st = store("snaps");
        let (h, _meta) = st.start(Path::new("/p"), None).unwrap();
        let mut conv = Conversation::new();

        let cp1 = Message::user_text("第一回合");
        let cp1_id = cp1.id.clone();
        record(&mut conv, &h, cp1);
        h.append_snapshot(&FileSnapshot {
            checkpoint_id: cp1_id.clone(),
            path: "/p/a.txt".into(),
            prior_content: Some("v0".into()),
        });
        let cp2 = Message::user_text("第二回合");
        let cp2_id = cp2.id.clone();
        record(&mut conv, &h, cp2);
        h.append_snapshot(&FileSnapshot {
            checkpoint_id: cp2_id.clone(),
            path: "/p/a.txt".into(),
            prior_content: Some("v1".into()),
        });

        let lines = read_lines(h.path()).unwrap();
        // rewind 到 cp1 → 恢复其后所有回合的快照，逆序（后写先还原：
        // 先写回 v1 再写回 v0，终态 v0）
        let after_cp1 = snapshots_after(&lines, &cp1_id);
        assert_eq!(after_cp1.len(), 2);
        assert_eq!(after_cp1[0].prior_content.as_deref(), Some("v1"));
        assert_eq!(after_cp1[1].prior_content.as_deref(), Some("v0"));

        // rewind 到 cp2 → 只撤第二回合自己的改动（cp2 的快照紧跟其消息之后）
        let after_cp2 = snapshots_after(&lines, &cp2_id);
        assert_eq!(after_cp2.len(), 1);
        assert_eq!(after_cp2[0].prior_content.as_deref(), Some("v1"));
        // checkpoint 不在文件里 → 空列表（防御）
        assert!(snapshots_after(&lines, "no-such-id").is_empty());
    }

    #[test]
    fn apply_snapshots_restores_and_deletes() {
        let dir = tmp_base("apply");
        let f_new = dir.join("new.txt");
        let f_old = dir.join("old.txt");
        std::fs::write(&f_old, "v0").unwrap();

        // 新文件快照（原不存在）→ 恢复 = 删除
        // 旧文件快照（v0）→ 恢复 = 写回 v0
        let restored = apply_snapshots(&[
            FileSnapshot {
                checkpoint_id: "cp".into(),
                path: f_old.clone(),
                prior_content: Some("v0".into()),
            },
            FileSnapshot {
                checkpoint_id: "cp".into(),
                path: f_new.clone(),
                prior_content: None,
            },
        ]);
        assert_eq!(restored, 2);
        assert_eq!(std::fs::read_to_string(&f_old).unwrap(), "v0");
        assert!(!f_new.exists(), "原不存在的文件恢复时应删除");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tolerant_of_bad_and_unknown_lines() {
        let st = store("tolerant");
        let (h, meta) = st.start(Path::new("/p"), None).unwrap();
        h.append_message(&Message::user_text("好行"));
        // 手工追加坏行 + 未知 type 行
        {
            let mut f = h.file.lock().unwrap();
            use std::io::Write;
            let _ = writeln!(f, "{{not json");
            let _ = writeln!(f, r#"{{"type":"future_record","x":1}}"#);
        }
        let (conv, _) = st.load(&meta.session_id).unwrap();
        assert_eq!(conv.active_messages().len(), 1, "坏行/未知行被跳过");
    }

    #[test]
    fn missing_session_load_errors() {
        let st = store("missing");
        assert!(st.load("no-such-session").is_err());
        assert!(st.resume("no-such-session").is_err());
    }

    #[test]
    fn sanitize_replaces_path_separators() {
        assert_eq!(
            sanitize_dir_name(Path::new("/Users/qi/yu/work dir")),
            "-Users-qi-yu-work-dir"
        );
        assert_eq!(sanitize_dir_name(Path::new("C:\\dev\\proj")), "C--dev-proj");
    }

    /// Message id 前缀正确（快照 checkpoint 关联用）。
    #[test]
    fn snapshot_checkpoint_uses_message_ids() {
        let id = new_message_id(Role::User);
        assert!(id.starts_with('u'));
        let s = FileSnapshot {
            checkpoint_id: id,
            path: PathBuf::from("/x"),
            prior_content: None,
        };
        let line = Line::Snapshot(s);
        let json = serde_json::to_string(&line).unwrap();
        assert!(json.contains(r#""type":"snapshot""#));
        let back: Line = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, Line::Snapshot(_)));
    }
}
