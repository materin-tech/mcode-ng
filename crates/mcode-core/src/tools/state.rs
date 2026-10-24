//! 会话级工具状态：read-before-write 追踪 / bash 会话 / todo 列表。
//!
//! read-before-write 安全机制：edit_file / write_file 覆盖已有
//! 文件前，必须先用 read_file 读过，且读之后文件没被外部改动
//! （mtime/size 指纹校验）——防止模型基于过期内容盲写。

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

// ---------------------------------------------------------------------------
// FileStateTracker — read-before-write
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct FileStamp {
    /// 读取时的内容指纹（蓝本：模型眼里的文件内容）
    content: String,
    mtime: Option<SystemTime>,
    size: u64,
}

/// 追踪「本会话读过哪些文件、读到的是什么」。
#[derive(Default)]
pub struct FileStateTracker {
    stamps: BTreeMap<PathBuf, FileStamp>,
}

impl FileStateTracker {
    pub fn record_read(&mut self, path: &Path, content: &str) {
        self.stamps.insert(
            path.to_path_buf(),
            FileStamp {
                content: content.to_string(),
                mtime: fs_mtime(path),
                size: std::fs::metadata(path).map(|m| m.len()).unwrap_or(0),
            },
        );
    }

    pub fn forget(&mut self, path: &Path) {
        self.stamps.remove(path);
    }

    /// 文件是否已读过（绝对路径或相对 cwd 解析后的路径）。
    pub fn has_read(&self, path: &Path) -> bool {
        self.stamps.contains_key(path)
    }

    /// 模型上次见到的内容（edit_file 的旧内容蓝本核对用）。
    pub fn last_seen(&self, path: &Path) -> Option<&str> {
        self.stamps.get(path).map(|s| s.content.as_str())
    }

    /// 写前校验：
    /// - 新文件（不存在）→ 放行；
    /// - 已读过且指纹未变 → 放行；
    /// - 存在但没读过 → 拒绝（先 read_file）；
    /// - 读过但外部改过（mtime/size 变了）→ 拒绝（重新 read）。
    pub fn ensure_writable(&self, path: &Path) -> Result<()> {
        if !path.exists() {
            return Ok(());
        }
        let Some(stamp) = self.stamps.get(path) else {
            anyhow::bail!(
                "文件 {} 存在但尚未读取。先调用 read_file 查看内容，再执行写入（防止盲写覆盖）",
                path.display()
            );
        };
        let cur_mtime = fs_mtime(path);
        let cur_size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        if stamp.mtime != cur_mtime || stamp.size != cur_size {
            anyhow::bail!(
                "文件 {} 在读取后被外部修改过，请重新 read_file 再写入",
                path.display()
            );
        }
        Ok(())
    }

    /// 写成功后把新内容登记为最新指纹（连续 edit 不需要重复 read）。
    pub fn record_write(&mut self, path: &Path, new_content: &str) {
        self.record_read(path, new_content);
    }
}

fn fs_mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok().and_then(|m| m.modified().ok())
}

// ---------------------------------------------------------------------------
// BashSession — cwd 持久化 + 后台任务
// ---------------------------------------------------------------------------

/// 一个后台 bash 任务的句柄。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackgroundTask {
    pub id: String,
    pub command: String,
    /// 原始输出文件（追加写）
    pub output_path: PathBuf,
    pub started_ms: u64,
    #[serde(skip)]
    pub status: TaskStatus,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    #[default]
    Running,
    Completed {
        code: i32,
    },
    Failed {
        reason: String,
    },
    Killed,
}

/// bash 工具的会话状态：跨调用保持 cwd（`cd` 生效），后台任务表可查询。
#[derive(Default)]
pub struct BashSession {
    /// 当前工作目录（cd 持久化；None = 跟随 ToolContext.cwd）
    pub cwd: Option<PathBuf>,
    tasks: BTreeMap<String, BackgroundTask>,
    seq: u64,
}

pub const MAX_BACKGROUND_TASKS: usize = 32;

/// 后台任务日志文件的全局序号（连同 pid 保证跨会话/跨进程路径唯一）。
static BG_LOG_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

impl BashSession {
    /// 本条命令的执行目录（显式 cwd 参数 > 会话持久 cwd > 上下文 cwd）。
    pub fn effective_cwd(&self, ctx_cwd: &Path, arg_cwd: Option<&str>) -> Result<PathBuf> {
        match arg_cwd {
            Some(rel) if !rel.trim().is_empty() => {
                let joined = self
                    .cwd
                    .clone()
                    .unwrap_or_else(|| ctx_cwd.to_path_buf())
                    .join(rel);
                Ok(joined)
            }
            _ => Ok(self.cwd.clone().unwrap_or_else(|| ctx_cwd.to_path_buf())),
        }
    }

    /// 命令执行成功后同步会话 cwd。
    ///
    /// 子进程的 `cd` 无法改父进程目录，因此持久化靠解析命令的 `cd <dir>`
    /// 前缀（主流 CLI 的通行近似）：`cd x && rest` / `cd x; rest` / `cd x`。
    /// `None`（无 cd 前缀）→ cwd 不变；`Some(dir)` → 会话 cwd = 当前持久
    /// cwd ∪ dir（退出工作区根时记 None）。
    pub fn sync_cwd_after_command(&mut self, command: &str, base: &Path, ctx_cwd: &Path, ok: bool) {
        if !ok {
            return;
        }
        let Some(dir) = extract_cd_prefix(command) else {
            return;
        };
        let target = base.join(&dir);
        let canonical = target.canonicalize().unwrap_or(target);
        let ctx_canonical = ctx_cwd
            .canonicalize()
            .unwrap_or_else(|_| ctx_cwd.to_path_buf());
        self.cwd = if canonical == ctx_canonical {
            None // 回到工作区根，视为复位
        } else {
            Some(canonical)
        };
    }

    pub fn spawn_task(&mut self, command: &str) -> Result<BackgroundTask> {
        if self.tasks.len() >= MAX_BACKGROUND_TASKS {
            anyhow::bail!("后台任务已达上限 {MAX_BACKGROUND_TASKS}，请先 bash_output 收割或 kill");
        }
        self.seq += 1;
        let id = format!("bg_{}", self.seq);
        // 日志路径必须进程唯一：会话内 seq 只保证本会话不重号，
        // 两个窗口/两个进程的同号任务共用一条路径会互相截断/删除。
        let n = BG_LOG_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let output_path =
            std::env::temp_dir().join(format!("mcode-bg-{}-{n}-{id}.log", std::process::id()));
        std::fs::write(&output_path, b"")
            .with_context(|| format!("创建 {} 失败", output_path.display()))?;
        let task = BackgroundTask {
            id: id.clone(),
            command: command.to_string(),
            output_path,
            started_ms: crate::util::now_ms(),
            status: TaskStatus::Running,
        };
        self.tasks.insert(id, task.clone());
        Ok(task)
    }

    pub fn task(&self, id: &str) -> Option<&BackgroundTask> {
        self.tasks.get(id)
    }

    pub fn task_mut(&mut self, id: &str) -> Option<&mut BackgroundTask> {
        self.tasks.get_mut(id)
    }

    pub fn finish_task(&mut self, id: &str, status: TaskStatus) {
        if let Some(t) = self.tasks.get_mut(id) {
            t.status = status;
        }
    }

    /// 收割已结束任务并删除表项（bash_output 语义：读到完整输出 + 退出码）。
    pub fn reap_task(&mut self, id: &str) -> Option<BackgroundTask> {
        self.tasks.remove(id)
    }

    /// 有输出增量但仍在跑的任务（bash_output 不收割）。
    pub fn running_ids(&self) -> Vec<String> {
        self.tasks
            .iter()
            .filter(|(_, t)| t.status == TaskStatus::Running)
            .map(|(id, _)| id.clone())
            .collect()
    }
}

/// 提取命令的 `cd <dir>` 前缀：`cd x && rest` / `cd x; rest` / 裸 `cd x`。
/// 返回 Some(dir)；无 cd 前缀或 cd 目标为空返回 None。
pub fn extract_cd_prefix(command: &str) -> Option<String> {
    let t = command.trim_start();
    let rest = t.strip_prefix("cd ").or_else(|| t.strip_prefix("cd\t"))?;
    let end = rest
        .find("&&")
        .or_else(|| rest.find(';'))
        .unwrap_or(rest.len());
    let raw = rest[..end].trim();
    let quoted = (raw.starts_with('"') && raw.ends_with('"') && raw.len() >= 2)
        || (raw.starts_with('\'') && raw.ends_with('\'') && raw.len() >= 2);
    let dir = if quoted {
        raw[1..raw.len() - 1].to_string()
    } else {
        raw.to_string()
    };
    if dir.is_empty() || dir == "-" || (!quoted && dir.contains(char::is_whitespace)) {
        // `cd` 无参（回 HOME）/ `cd -` / 未加引号的多级表达式不跟踪
        None
    } else {
        Some(dir)
    }
}

// ---------------------------------------------------------------------------
// TodoState — TodoWrite 的存储端
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TodoItemStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TodoItem {
    pub content: String,
    pub status: TodoItemStatus,
    pub active_form: String,
}

/// TodoWrite 状态（/todos UI 与 statusline 数据源）。
#[derive(Default)]
pub struct TodoState {
    items: Vec<TodoItem>,
    /// 会话内已提醒过"只有一个 in_progress"的次数（防刷屏）
    pub warned_multi_active: bool,
}

impl TodoState {
    pub fn items(&self) -> &[TodoItem] {
        &self.items
    }

    /// 整表替换（TodoWrite 语义）。校验：content 非空。
    pub fn replace_all(&mut self, items: Vec<TodoItem>) -> Result<()> {
        for item in &items {
            anyhow::ensure!(!item.content.trim().is_empty(), "todo 内容不能为空");
        }
        self.items = items;
        Ok(())
    }

    /// 进度摘要（statusline 用）：3/7。
    pub fn progress(&self) -> (usize, usize) {
        let done = self
            .items
            .iter()
            .filter(|i| i.status == TodoItemStatus::Completed)
            .count();
        (done, self.items.len())
    }

    /// 当前 in_progress 的条目（statusline 显示"正在做什么"）。
    pub fn active(&self) -> Option<&TodoItem> {
        self.items
            .iter()
            .find(|i| i.status == TodoItemStatus::InProgress)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpfile(name: &str, content: &str) -> PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("mcode-state-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, content).unwrap();
        p
    }

    #[test]
    fn write_requires_prior_read() {
        let p = tmpfile("w1.txt", "hello");
        let mut t = FileStateTracker::default();
        // 没读过 → 拒绝
        assert!(t.ensure_writable(&p).is_err());
        // 读过 → 放行
        t.record_read(&p, "hello");
        assert!(t.ensure_writable(&p).is_ok());
        // 新文件（不存在）→ 放行
        let fresh = p.with_extension("fresh");
        assert!(t.ensure_writable(&fresh).is_ok());
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn external_modification_invalidates_stamp() {
        let p = tmpfile("w2.txt", "v1");
        let mut t = FileStateTracker::default();
        t.record_read(&p, "v1");
        // 外部改动
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&p, "v2-changed").unwrap();
        let res = t.ensure_writable(&p);
        assert!(res.is_err(), "外部修改后必须重读");
        assert!(res.unwrap_err().to_string().contains("重新 read_file"));
        // 重读后放行
        t.record_read(&p, "v2-changed");
        assert!(t.ensure_writable(&p).is_ok());
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn write_updates_stamp_so_consecutive_edits_pass() {
        let p = tmpfile("w3.txt", "v1");
        let mut t = FileStateTracker::default();
        t.record_read(&p, "v1");
        std::fs::write(&p, "v2").unwrap();
        t.record_write(&p, "v2");
        assert!(t.ensure_writable(&p).is_ok());
        assert_eq!(t.last_seen(&p), Some("v2"));
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn bash_cwd_persists_via_cd_prefix() {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut s = BashSession::default();
        let ctx = std::env::temp_dir();
        assert_eq!(s.effective_cwd(&ctx, None).unwrap(), ctx);
        // cd 前缀命令成功 → 持久 cwd 更新
        let sub = ctx.join(format!("mcode-state-cwd-{n}"));
        std::fs::create_dir_all(&sub).unwrap();
        s.sync_cwd_after_command(&format!("cd mcode-state-cwd-{n} && pwd"), &ctx, &ctx, true);
        assert_eq!(
            s.effective_cwd(&ctx, None).unwrap(),
            sub.canonicalize().unwrap()
        );
        // 相对 cwd 参数基于持久 cwd
        let rel = s.effective_cwd(&ctx, Some("src")).unwrap();
        assert!(rel.ends_with("src"));
        // 无 cd 前缀 → cwd 不变
        s.sync_cwd_after_command("pwd", &ctx, &ctx, true);
        assert!(s.cwd.is_some());
        // 失败的命令不更新
        s.sync_cwd_after_command("cd /nonexistent-xyz", &ctx, &ctx, false);
        assert!(s.cwd.is_some());
        // cd 回工作区根（.）→ 复位
        s.sync_cwd_after_command("cd .", &ctx, &ctx, true);
        assert!(s.cwd.is_none());
        let _ = std::fs::remove_dir_all(&sub);
    }

    #[test]
    fn extract_cd_prefix_cases() {
        assert_eq!(extract_cd_prefix("cd sub && pwd"), Some("sub".into()));
        assert_eq!(
            extract_cd_prefix("cd \"my dir\"; ls"),
            Some("my dir".into())
        );
        assert_eq!(extract_cd_prefix("cd"), None);
        assert_eq!(extract_cd_prefix("cd -"), None);
        assert_eq!(extract_cd_prefix("echo cd sub"), None);
        assert_eq!(extract_cd_prefix("  cd  a/b  "), Some("a/b".into()));
    }

    #[test]
    fn background_task_lifecycle() {
        let mut s = BashSession::default();
        let t = s.spawn_task("sleep 100").unwrap();
        assert!(s.task(&t.id).is_some());
        assert_eq!(s.running_ids(), vec![t.id.clone()]);
        s.finish_task(&t.id, TaskStatus::Completed { code: 0 });
        assert!(s.running_ids().is_empty());
        let reaped = s.reap_task(&t.id).unwrap();
        assert_eq!(reaped.command, "sleep 100");
        assert!(s.task(&t.id).is_none());
        assert!(reaped.output_path.exists());
        let _ = std::fs::remove_file(&reaped.output_path);
    }

    #[test]
    fn todo_progress_and_active() {
        let mut ts = TodoState::default();
        ts.replace_all(vec![
            TodoItem {
                content: "a".into(),
                status: TodoItemStatus::Completed,
                active_form: "做 a".into(),
            },
            TodoItem {
                content: "b".into(),
                status: TodoItemStatus::InProgress,
                active_form: "做 b".into(),
            },
            TodoItem {
                content: "c".into(),
                status: TodoItemStatus::Pending,
                active_form: "做 c".into(),
            },
        ])
        .unwrap();
        assert_eq!(ts.progress(), (1, 3));
        assert_eq!(ts.active().unwrap().content, "b");
        // 空内容拒绝
        assert!(ts
            .replace_all(vec![TodoItem {
                content: "  ".into(),
                status: TodoItemStatus::Pending,
                active_form: "x".into()
            }])
            .is_err());
    }
}
