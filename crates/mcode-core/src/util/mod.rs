//! 通用工具：数据目录、项目哈希、用量台账、跨平台杂项。
//!
//! 与旧版的关键差异：
//! - 项目目录哈希改为 **base64url(cwd)**，与 TS CLI `hashCwd` 逐字节对齐——
//!   旧 Rust core 用的 FNV-1a 与 TS 互不识别（同机装两版会话互不可见），
//!   本版修复后可直接 resume TS CLI / 旧 CLI 创建的历史会话。
//! - 无 HOME 环境不再 panic（旧版 `.expect("无法获取 HOME")` 会在
//!   Windows 服务态 / 容器裸环境下直接崩），目录一律返回 `Option`，
//!   由 config/sessions 层翻译成可读错误。

pub mod file_refs;
pub mod frontmatter;
pub mod paths;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use std::collections::BTreeMap;
use std::path::PathBuf;

/// 用户主目录。无 HOME 时返回 `None`（调用方给可读错误）。
pub fn home_dir() -> Option<PathBuf> {
    dirs::home_dir()
}

/// 数据根目录：`~/.mcode/`。TS CLI / 旧 CLI / Desktop 三方共享同一根。
pub fn mcode_dir() -> Option<PathBuf> {
    home_dir().map(|h| h.join(".mcode"))
}

/// 全局配置：`~/.mcode/config.json`（camelCase schema，三方互通）。
pub fn config_path() -> Option<PathBuf> {
    mcode_dir().map(|d| d.join("config.json"))
}

/// 桌面扩展配置：`~/.mcode/desktop.json`（权限规则 / MCP / hooks）。
pub fn desktop_config_path() -> Option<PathBuf> {
    mcode_dir().map(|d| d.join("desktop.json"))
}

/// cwd → 项目目录名（base64url 无 padding，对齐 TS `Buffer.from(cwd).toString('base64url')`）。
pub fn hash_cwd(cwd: &std::path::Path) -> String {
    URL_SAFE_NO_PAD.encode(cwd.to_string_lossy().as_bytes())
}

/// 项目目录名 → cwd（decode 失败说明不是本工具创建的目录，返回 `None`）。
pub fn unhash_cwd(name: &str) -> Option<String> {
    let bytes = URL_SAFE_NO_PAD.decode(name).ok()?;
    String::from_utf8(bytes).ok()
}

/// `~/.mcode/projects/{hash}` — 单项目根（会话与文件快照都在其下）。
pub fn project_dir(cwd: &std::path::Path) -> Option<PathBuf> {
    mcode_dir().map(|d| d.join("projects").join(hash_cwd(cwd)))
}

/// `~/.mcode/projects/{hash}/sessions` — 会话 JSONL 目录。
pub fn sessions_dir(cwd: &std::path::Path) -> Option<PathBuf> {
    project_dir(cwd).map(|d| d.join("sessions"))
}

/// 当前毫秒时间戳（会话消息时间戳 / 状态栏耗时展示共用）。
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// token 用量台账（对齐 TS util/token-usage.ts 的按模型分组累计）
// ---------------------------------------------------------------------------

/// 单模型用量快照。
#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TokenUsage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

impl TokenUsage {
    /// 从协议归一化的 [`crate::message::Usage`] 构造（cache_creation → cache_write）。
    pub fn from_message_usage(u: &crate::message::Usage) -> Self {
        TokenUsage {
            input: u.input_tokens,
            output: u.output_tokens,
            cache_read: u.cache_read_input_tokens,
            cache_write: u.cache_creation_input_tokens,
        }
    }
}

/// 跨模型累计台账。
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct TokenLedger {
    /// model name → usage
    pub by_model: BTreeMap<String, TokenUsage>,
}

impl TokenLedger {
    pub fn record(&mut self, model: &str, u: TokenUsage) {
        let e = self.by_model.entry(model.to_string()).or_default();
        e.input += u.input;
        e.output += u.output;
        e.cache_read += u.cache_read;
        e.cache_write += u.cache_write;
    }

    pub fn total(&self) -> TokenUsage {
        self.by_model
            .values()
            .fold(TokenUsage::default(), |a, u| TokenUsage {
                input: a.input + u.input,
                output: a.output + u.output,
                cache_read: a.cache_read + u.cache_read,
                cache_write: a.cache_write + u.cache_write,
            })
    }
}

/// Windows：隐藏子进程的控制台窗口（CREATE_NO_WINDOW = 0x0800_0000）。
/// GUI 子系统进程（Desktop / 未来 Windows 服务态）spawn console 程序时
/// 不再弹窗；stdio 走管道不受影响。非 Windows 无操作。
pub fn hide_console_window(cmd: &mut tokio::process::Command) -> &mut tokio::process::Command {
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    #[cfg(not(windows))]
    {
        let _ = cmd;
    }
    cmd
}

/// 同步 [`hide_console_window`]：std::process::Command 版（同步工具路径用）。
pub fn hide_console_window_sync(cmd: &mut std::process::Command) -> &mut std::process::Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    #[cfg(not(windows))]
    {
        let _ = cmd;
    }
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cwd_hash_roundtrips_and_matches_node_base64url() {
        let cwd = std::path::Path::new("/Users/qi/dev/mcode-ctx");
        let h = hash_cwd(cwd);
        // Node: Buffer.from('/Users/qi/dev/mcode-ctx').toString('base64url')
        assert_eq!(h, "L1VzZXJzL3FpL2Rldi9tY29kZS1jdHg");
        assert_eq!(unhash_cwd(&h).as_deref(), Some("/Users/qi/dev/mcode-ctx"));
        // 含 CJK 的路径同样 roundtrip
        let zh = std::path::Path::new("/Users/qi/项目/测试");
        assert_eq!(
            unhash_cwd(&hash_cwd(zh)).as_deref(),
            Some("/Users/qi/项目/测试")
        );
        // 非法目录名 decode 失败
        assert!(unhash_cwd("not-base64url!!").is_none());
    }

    #[test]
    fn base64url_escapes_path_specials() {
        // '+' '/' 会出现在标准 base64 中，base64url 必须转成 '-'
        let h = hash_cwd(std::path::Path::new("aa/../bb?"));
        assert!(!h.contains('+') && !h.contains('/'), "base64url 不得含 + /");
        assert!(!h.contains('='), "Node base64url 无 padding");
    }

    #[test]
    fn ledger_groups_by_model() {
        let mut l = TokenLedger::default();
        l.record(
            "glm-4.7",
            TokenUsage {
                input: 100,
                output: 10,
                ..Default::default()
            },
        );
        l.record(
            "glm-4.7",
            TokenUsage {
                input: 50,
                output: 5,
                ..Default::default()
            },
        );
        l.record(
            "glm-4.7-air",
            TokenUsage {
                input: 7,
                ..Default::default()
            },
        );
        assert_eq!(l.by_model["glm-4.7"].input, 150);
        assert_eq!(l.total().output, 15);
        assert_eq!(l.total().input, 157);
    }

    #[test]
    fn usage_maps_cache_fields() {
        let u = crate::message::Usage {
            input_tokens: 10,
            output_tokens: 3,
            cache_read_input_tokens: 100,
            cache_creation_input_tokens: 55,
        };
        let t = TokenUsage::from_message_usage(&u);
        assert_eq!(t.cache_read, 100);
        assert_eq!(t.cache_write, 55);
    }
}
