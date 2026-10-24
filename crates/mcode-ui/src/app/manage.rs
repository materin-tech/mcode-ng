// MCode Desktop — AI coding agent desktop app
// Copyright (c) 2026 Materin-tech. All rights reserved.
// SPDX-License-Identifier: MIT

//! 管理操作 — 设置/扩展页对话框状态与 CRUD（消费 extension_manage 管理原语，
//! 与 CLI manage 逻辑同一套底座）

use crate::app::extension_manage::{
    create_agent, create_skill, install_agent_from_file, install_skill_from_dir, remove_agent,
    remove_skill,
};
use crate::app::runtime::{load_agent_registry, load_skill_registry};
use crate::app::{App, Dialog, InstallKind, RemoveAction};
use crate::views::text_input::TextInput;
use gpui::Entity;
use mcode_core::config::Provider;
use mcode_core::transcript::NoticeKind;

impl App {
    // ---------- 扩展页缓存 ----------

    /// 刷新技能/agent 清单（进入扩展视图或增删后调用；重扫全部来源目录）
    pub fn refresh_extensions(&mut self) {
        self.ext_skills = load_skill_registry(&self.cwd)
            .all()
            .into_iter()
            .cloned()
            .collect();
        self.ext_agents = load_agent_registry(&self.cwd)
            .all()
            .into_iter()
            .cloned()
            .collect();
    }

    // ---------- 对话框打开 ----------

    pub fn close_dialog(&mut self) {
        self.dialog = None;
    }

    fn prefill(e: &Entity<TextInput>, v: &str, cx: &mut gpui::Context<Self>) {
        e.update(cx, |t, _| t.content = v.to_string().into());
    }

    pub fn open_provider_form(&mut self, editing: Option<&Provider>, cx: &mut gpui::Context<Self>) {
        let (n, u, k, m, mml) = editing
            .map(|p| {
                (
                    p.name.clone(),
                    p.base_url.clone(),
                    p.api_key_env.clone(),
                    p.models.join(","),
                    p.multimodal_models.join(","),
                )
            })
            .unwrap_or_default();
        let name = Self::make_input(cx, "供应商名称，如 glm");
        let url = Self::make_input(cx, "https://…/v1");
        let key = Self::make_input(cx, "API key 环境变量名");
        let models = Self::make_input(cx, "模型列表，逗号分隔");
        let mm = Self::make_input(cx, "多模态模型，逗号分隔（可留空）");
        Self::prefill(&name, &n, cx);
        Self::prefill(&url, &u, cx);
        Self::prefill(&key, &k, cx);
        Self::prefill(&models, &m, cx);
        Self::prefill(&mm, &mml, cx);
        self.dialog = Some(Dialog::ProviderForm {
            editing: editing.map(|p| p.name.clone()),
            name,
            url,
            key,
            models,
            mm,
            protocol: editing.map(|p| p.protocol).unwrap_or_default(),
        });
    }
}

impl App {
    pub fn open_agent_create(&mut self, cx: &mut gpui::Context<Self>) {
        let name = Self::make_input(cx, "agent 名称，如 doc-writer");
        let desc = Self::make_input(cx, "一句话描述（主模型据此决定何时派发）");
        let prompt = Self::make_input(cx, "system prompt（正文）");
        self.dialog = Some(Dialog::AgentCreate { name, desc, prompt });
    }

    pub fn open_skill_create(&mut self, cx: &mut gpui::Context<Self>) {
        let name = Self::make_input(cx, "技能名称，如 code-explain");
        let desc = Self::make_input(cx, "一句话描述（常驻上下文的唯一线索）");
        self.dialog = Some(Dialog::SkillCreate { name, desc });
    }

    pub fn open_install(&mut self, kind: InstallKind, cx: &mut gpui::Context<Self>) {
        let path = match kind {
            InstallKind::Skill => Self::make_input(cx, "/本地目录（须含 SKILL.md）"),
            InstallKind::Agent => Self::make_input(cx, "/本地路径/agent.md"),
        };
        self.dialog = Some(Dialog::Install { kind, path });
    }

    pub fn open_mcp_form(&mut self, cx: &mut gpui::Context<Self>) {
        let name = Self::make_input(cx, "server 名称，如 fs");
        let command = Self::make_input(
            cx,
            "command 及参数，如 npx -y @modelcontextprotocol/server-fs",
        );
        self.dialog = Some(Dialog::McpForm { name, command });
    }

    pub fn open_confirm_remove(&mut self, action: RemoveAction) {
        self.dialog = Some(Dialog::ConfirmRemove { action });
    }

    // ---------- 对话框提交 ----------

    fn csv(s: &str) -> Vec<String> {
        s.split(',')
            .map(|x| x.trim().to_string())
            .filter(|x| !x.is_empty())
            .collect()
    }

    /// 供应商表单保存：新增走 add，编辑走 remove(旧)+update/add + active 保持
    pub fn dialog_save_provider(&mut self, cx: &mut gpui::Context<Self>) {
        let Some(Dialog::ProviderForm {
            editing,
            name,
            url,
            key,
            models,
            mm,
            protocol,
        }) = &self.dialog
        else {
            return;
        };
        let name = name.read(cx).text().trim().to_string();
        let url = url.read(cx).text().trim().to_string();
        let key = key.read(cx).text().trim().to_string();
        let models = Self::csv(&models.read(cx).text());
        let mm = Self::csv(&mm.read(cx).text());
        if name.is_empty() || url.is_empty() || models.is_empty() {
            self.push_notice("名称 / baseURL / 模型列表不能为空", NoticeKind::Error);
            return;
        }
        let p = Provider {
            name: name.clone(),
            base_url: url,
            api_key_env: key,
            models,
            multimodal_models: mm,
            protocol: *protocol,
        };
        if let Some(old) = editing {
            let was_active = self.cfg.active_provider.as_deref() == Some(old.as_str());
            if *old != name {
                self.cfg.remove_provider(old).ok();
            }
            self.cfg
                .update_provider(p.clone())
                .or_else(|_| self.cfg.add_provider(p))
                .ok();
            if was_active {
                self.cfg.set_active_provider(&name).ok();
            }
        } else if let Err(e) = self.cfg.add_provider(p) {
            self.push_notice(format!("{e:#}"), NoticeKind::Error);
            return;
        }
        self.dialog = None;
        self.save_cfg_and_rebuild(cx);
        self.push_notice(format!("供应商 `{name}` 已保存"), NoticeKind::Success);
    }
}

impl App {
    pub fn dialog_agent_create(&mut self, cx: &mut gpui::Context<Self>) {
        let Some(Dialog::AgentCreate { name, desc, prompt }) = &self.dialog else {
            return;
        };
        let name = name.read(cx).text().trim().to_string();
        let desc = desc.read(cx).text().trim().to_string();
        let prompt = prompt.read(cx).text().trim().to_string();
        if name.is_empty() || desc.is_empty() || prompt.is_empty() {
            self.push_notice("名称 / 描述 / system prompt 不能为空", NoticeKind::Error);
            return;
        }
        match create_agent(&name, &desc, &prompt, None, None) {
            Ok(p) => {
                self.dialog = None;
                self.refresh_extensions();
                self.push_notice(
                    format!("agent `{name}` 已创建：{}", p.display()),
                    NoticeKind::Success,
                );
            }
            Err(e) => self.push_notice(format!("{e:#}"), NoticeKind::Error),
        }
    }

    pub fn dialog_skill_create(&mut self, cx: &mut gpui::Context<Self>) {
        let Some(Dialog::SkillCreate { name, desc }) = &self.dialog else {
            return;
        };
        let name = name.read(cx).text().trim().to_string();
        let desc = desc.read(cx).text().trim().to_string();
        if name.is_empty() || desc.is_empty() {
            self.push_notice("名称 / 描述不能为空", NoticeKind::Error);
            return;
        }
        let body = format!("# {name}\n\n待补充：逐步披露的完整指令内容。");
        match create_skill(&name, &desc, &body, Some(&self.cwd)) {
            Ok(p) => {
                self.dialog = None;
                self.refresh_extensions();
                self.push_notice(
                    format!("技能 `{name}` 已创建（项目级）：{}", p.display()),
                    NoticeKind::Success,
                );
            }
            Err(e) => self.push_notice(format!("{e:#}"), NoticeKind::Error),
        }
    }

    pub fn dialog_install(&mut self, cx: &mut gpui::Context<Self>) {
        let Some(Dialog::Install { kind, path }) = &self.dialog else {
            return;
        };
        let path = path.read(cx).text().trim().to_string();
        if path.is_empty() {
            self.push_notice("路径不能为空", NoticeKind::Error);
            return;
        }
        let src = std::path::PathBuf::from(shellexpand(&path));
        let r = match kind {
            InstallKind::Skill => install_skill_from_dir(&src, Some(&self.cwd))
                .map(|p| format!("✓ 技能已安装（项目级）：{}", p.display()))
                .or_else(|e| {
                    install_skill_from_dir(&src, None)
                        .map(|p| format!("✓ 技能已安装（用户级）：{}", p.display()))
                        .map_err(|_| e)
                }),
            InstallKind::Agent => install_agent_from_file(&src, None)
                .map(|p| format!("✓ agent 已安装：{}", p.display())),
        };
        match r {
            Ok(msg) => {
                self.dialog = None;
                self.refresh_extensions();
                self.push_notice(msg, NoticeKind::Info);
            }
            Err(e) => self.push_notice(format!("{e:#}"), NoticeKind::Error),
        }
    }

    pub fn dialog_save_mcp(&mut self, cx: &mut gpui::Context<Self>) {
        let Some(Dialog::McpForm { name, command }) = &self.dialog else {
            return;
        };
        let name = name.read(cx).text().trim().to_string();
        let command = command.read(cx).text().trim().to_string();
        let mut parts = command.split_whitespace();
        let Some(cmd) = parts.next().map(|s| s.to_string()) else {
            self.push_notice("command 不能为空", NoticeKind::Error);
            return;
        };
        let mut desktop = mcode_core::config::DesktopConfig::load().unwrap_or_default();
        desktop.upsert_mcp_server(mcode_core::config::McpServerConfig {
            name: name.clone(),
            command: cmd,
            args: parts.map(|s| s.to_string()).collect(),
            env: Default::default(),
            url: None,
            headers: Default::default(),
            enabled: true,
        });
        if let Err(e) = desktop.save() {
            self.push_notice(format!("保存 desktop.json 失败：{e:#}"), NoticeKind::Error);
            return;
        }
        self.dialog = None;
        self.rebuild_harness(cx);
        self.push_notice(
            format!("MCP server `{name}` 已写入（已重建连接）"),
            NoticeKind::Success,
        );
    }

    pub fn dialog_confirm_remove(&mut self, cx: &mut gpui::Context<Self>) {
        let Some(Dialog::ConfirmRemove { action }) = self.dialog.take() else {
            return;
        };
        let msg = match action {
            RemoveAction::Provider(name) => {
                let r = self
                    .cfg
                    .remove_provider(&name)
                    .map(|_| format!("✓ 供应商 `{name}` 已移除（active 已回落）"));
                self.save_cfg_and_rebuild(cx);
                r
            }
            RemoveAction::Skill(name) => remove_skill(&name, Some(&self.cwd))
                .and_then(|hit| {
                    if hit {
                        Ok("✓ 已移除项目级技能".into())
                    } else {
                        remove_skill(&name, None).map(|_| "✓ 已移除用户级技能".into())
                    }
                })
                .inspect(|_| self.refresh_extensions()),
            RemoveAction::Agent(name) => remove_agent(&name, None).map(|hit| {
                if hit {
                    self.refresh_extensions();
                    format!("✓ agent `{name}` 已移除")
                } else {
                    format!("✗ 未找到 `{name}`")
                }
            }),
            RemoveAction::Mcp(name) => {
                let mut desktop = mcode_core::config::DesktopConfig::load().unwrap_or_default();
                let r = if desktop.remove_mcp_server(&name) {
                    desktop
                        .save()
                        .map(|_| format!("✓ MCP server `{name}` 已移除"))
                } else {
                    Err(anyhow::anyhow!("未找到 `{name}`"))
                };
                self.rebuild_harness(cx);
                r
            }
        };
        match msg {
            Ok(m) => self.push_notice(m, NoticeKind::Info),
            Err(e) => self.push_notice(format!("{e:#}"), NoticeKind::Error),
        }
    }
}

/// 展开 ~ 前缀（安装路径手输场景）
fn shellexpand(p: &str) -> String {
    if let Some(rest) = p.strip_prefix("~/")
        && let Ok(home) = std::env::var("HOME")
    {
        return format!("{home}/{rest}");
    }
    p.to_string()
}
