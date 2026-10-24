// MCode — AI coding agent
// Copyright (c) 2026 Materin-tech. All rights reserved.
// SPDX-License-Identifier: MIT

//! 斜杠命令的运行时落地：state 分派出 [`crate::tui::state::RuntimeCmd`]
//! 与若干 `Local` 名字后，这里做真正的 IO/会话操作。
//! 纯 IO 函数显式收参（不摸全局），单测直调。

use anyhow::Result;
use mcode_core::message::{Conversation, Role};
use std::io::Write;
use std::path::Path;

/// /export：会话活跃路径 → Markdown。返回写入的消息条数。
pub fn export_markdown(conv: &Conversation, path: &Path) -> Result<usize> {
    let mut out = String::from("# MCode 会话导出\n\n");
    let mut n = 0;
    for m in conv.active_messages() {
        if m.synthetic {
            continue;
        }
        let text = m.text();
        if text.trim().is_empty() {
            continue;
        }
        match m.role {
            Role::User => out.push_str(&format!("## 用户\n\n{text}\n\n")),
            Role::Assistant => out.push_str(&format!("## 助手\n\n{text}\n\n")),
        }
        n += 1;
    }
    std::fs::write(path, out)?;
    Ok(n)
}

/// /copy：文本进系统剪贴板。依次尝试 pbcopy（macOS）/ clip（Windows）/
/// wl-copy / xclip——全失败才报错（不引第三方剪贴板依赖）。
pub fn copy_to_clipboard(text: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    let candidates: &[&str] = &["pbcopy"];
    #[cfg(target_os = "windows")]
    let candidates: &[&str] = &["clip"];
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let candidates: &[&str] = &["wl-copy", "xclip", "xsel"];

    for bin in candidates {
        let mut cmd = std::process::Command::new(bin);
        // xclip/xsel 需要参数才写剪贴板；pbcopy/clip 直接吃 stdin
        if *bin == "xclip" {
            cmd.args(["-selection", "clipboard"]);
        } else if *bin == "xsel" {
            cmd.arg("--clipboard").arg("--input");
        }
        match cmd
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(mut child) => {
                if let Some(mut sin) = child.stdin.take() {
                    let _ = sin.write_all(text.as_bytes());
                }
                let _ = child.wait();
                return Ok(());
            }
            Err(_) => continue,
        }
    }
    anyhow::bail!("未找到可用的剪贴板命令（pbcopy/clip/wl-copy/xclip）")
}

/// /diff：`git diff --stat` 概要。None = 非 git 仓库或 git 不可用。
pub fn git_diff_stat(cwd: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["diff", "--stat"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Some(text).filter(|t| !t.is_empty())
}

/// /doctor 单项体检结果。
pub struct DoctorCheck {
    pub name: &'static str,
    pub ok: bool,
    pub note: String,
}

/// /doctor：配置/provider/密钥/模型/会话存储/MCP 逐项自检。
/// 密钥只报告「已设置/未设置」，绝不打印值。
pub fn run_doctor(
    config: &mcode_core::config::Config,
    desktop: &mcode_core::config::DesktopConfig,
    cwd: &Path,
    mcp_report: &[String],
) -> Vec<DoctorCheck> {
    let mut out = vec![];

    match mcode_core::config::Config::load() {
        Ok(_) => out.push(DoctorCheck {
            name: "配置文件",
            ok: true,
            note: "~/.mcode/config.json 可读".into(),
        }),
        Err(e) => out.push(DoctorCheck {
            name: "配置文件",
            ok: false,
            note: format!("加载失败：{e:#}"),
        }),
    }

    match config.active_provider() {
        Some(p) => {
            let key = p.resolve_api_key();
            out.push(DoctorCheck {
                name: "Provider",
                ok: true,
                note: format!(
                    "{}（{}）· 密钥{}",
                    p.name,
                    p.base_url,
                    if key.is_some_and(|k| !k.is_empty()) {
                        "已设置"
                    } else {
                        "未设置（检查 api_key_env 对应环境变量）"
                    }
                ),
            });
        }
        None => out.push(DoctorCheck {
            name: "Provider",
            ok: false,
            note: "未配置 active_provider；/provider <name> 切换或编辑 providers.json".into(),
        }),
    }

    let model = config.active_model();
    out.push(DoctorCheck {
        name: "模型",
        ok: model.is_some(),
        note: model.unwrap_or_else(|| "未配置 active_model".into()),
    });

    match SessionStoreProbe::for_project(cwd) {
        Ok(dir) => {
            let probe = dir.join(".doctor-probe");
            let ok = std::fs::write(&probe, b"ok").is_ok();
            let _ = std::fs::remove_file(&probe);
            out.push(DoctorCheck {
                name: "会话存储",
                ok,
                note: format!(
                    "{} {}",
                    dir.display(),
                    if ok {
                        "可写"
                    } else {
                        "不可写（检查权限）"
                    }
                ),
            });
        }
        Err(e) => out.push(DoctorCheck {
            name: "会话存储",
            ok: false,
            note: format!("目录创建失败：{e}"),
        }),
    }

    out.push(DoctorCheck {
        name: "MCP",
        ok: mcp_report.is_empty(),
        note: if mcp_report.is_empty() {
            format!("{} 个 server 全部就绪", desktop.mcp_servers.len())
        } else {
            mcp_report.join("；")
        },
    });

    out
}

/// 会话存储探针（把 SessionStore::for_project 的 Result 借给 doctor；不落会话文件）。
struct SessionStoreProbe;

impl SessionStoreProbe {
    fn for_project(cwd: &Path) -> Result<std::path::PathBuf> {
        let store = mcode_core::sessions::SessionStore::for_project(cwd)?;
        std::fs::create_dir_all(store.dir())?;
        Ok(store.dir().to_path_buf())
    }
}

/// /resume 无参：会话清单行（id 前缀 + 标题 + 条数）。
pub fn resume_list_lines(store: &mcode_core::sessions::SessionStore) -> Vec<String> {
    let list = store.list();
    if list.is_empty() {
        return vec!["（本项目暂无历史会话）".into()];
    }
    list.iter()
        .map(|s| {
            let id8: String = s.session_id.chars().take(12).collect();
            let title = s.title.as_deref().unwrap_or("（无标题）");
            format!(
                "  {id8}  「{}」  {} 条",
                title.chars().take(40).collect::<String>(),
                s.message_count
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcode_core::message::Message;

    #[test]
    fn export_writes_markdown_of_active_path() {
        let mut conv = Conversation::new();
        conv.push(Message::user_text("第一问"));
        conv.push(Message::assistant_text("第一答"));
        let id2 = conv.push(Message::user_text("第二问"));
        conv.rewind_before(&id2); // 活跃路径只剩前两条

        let tmp = std::env::temp_dir().join(format!("mcode-export-{}.md", std::process::id()));
        let n = export_markdown(&conv, &tmp).unwrap();
        assert_eq!(n, 2);
        let text = std::fs::read_to_string(&tmp).unwrap();
        assert!(text.contains("## 用户\n\n第一问"));
        assert!(text.contains("## 助手\n\n第一答"));
        assert!(!text.contains("第二问"), "被回退的消息不导出");
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn doctor_reports_missing_provider_and_unset_key() {
        let cfg = mcode_core::config::Config::default();
        let desktop = mcode_core::config::DesktopConfig::default();
        let checks = run_doctor(&cfg, &desktop, std::path::Path::new("/tmp"), &[]);
        // default Config 至少给出结构化结论，不 panic；密钥未设置必须显式提示
        let provider = checks.iter().find(|c| c.name == "Provider").unwrap();
        assert!(
            provider.note.contains("未配置") || provider.note.contains("未设置"),
            "{}",
            provider.note
        );
        assert!(!checks.iter().any(|c| c.note.contains("sk-")), "不泄漏密钥");
    }

    #[test]
    fn git_diff_stat_none_outside_repo() {
        // 新建临时目录不是 git 仓库 → None（不 panic、不误报）
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = std::env::temp_dir().join(format!("mcode-diff-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        assert!(git_diff_stat(&tmp).is_none());
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
