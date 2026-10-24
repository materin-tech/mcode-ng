// MCode — AI coding agent
// Copyright (c) 2026 Materin-tech. All rights reserved.
// SPDX-License-Identifier: MIT

//! 斜杠指令目录（CLI 与 Desktop 共用的唯一事实源）。
//!
//! 每条 [`CommandSpec`] 携带 `usage`（用法）与 `desc`（作用），两端的
//! /help 输出与输入补全提示都从 [`commands()`] 渲染——改这里，两端同步。

/// 指令分组（/help 按组渲染）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandGroup {
    /// 对话与会话生命周期
    Conversation,
    /// 上下文与用量
    Context,
    /// 配置切换
    Config,
    /// 代理能力（skills/agents/hooks/MCP）
    Agent,
    /// 诊断与状态
    Diagnostics,
}

impl CommandGroup {
    /// /help 的组标题
    pub fn title(self) -> &'static str {
        match self {
            CommandGroup::Conversation => "对话与会话",
            CommandGroup::Context => "上下文与用量",
            CommandGroup::Config => "配置",
            CommandGroup::Agent => "代理能力",
            CommandGroup::Diagnostics => "诊断与状态",
        }
    }
}

/// 单条斜杠指令的规格：名称 + 别名 + 用法 + 作用 + 分组
#[derive(Debug, Clone, Copy)]
pub struct CommandSpec {
    /// 指令名（不含前导 `/`）
    pub name: &'static str,
    /// 别名（同样不含 `/`）
    pub aliases: &'static [&'static str],
    /// 用法（含参数占位，如 "/compact [instructions]"）
    pub usage: &'static str,
    /// 作用（一句话，/help 与补全提示共用）
    pub desc: &'static str,
    /// 分组
    pub group: CommandGroup,
}

/// 全量指令目录。两端实现差异（如 Desktop 无 /quit 之外的特殊键位）由
/// 各自的 dispatch 决定；目录本身保持两端一致。
pub const COMMANDS: &[CommandSpec] = &[
    // ---------- 对话与会话 ----------
    CommandSpec {
        name: "help",
        aliases: &["?"],
        usage: "/help [command]",
        desc: "显示全部指令或单个指令的用法与作用",
        group: CommandGroup::Conversation,
    },
    CommandSpec {
        name: "clear",
        aliases: &[],
        usage: "/clear",
        desc: "清空当前会话历史，开始新对话（会话文件保留）",
        group: CommandGroup::Conversation,
    },
    CommandSpec {
        name: "compact",
        aliases: &[],
        usage: "/compact [instructions]",
        desc: "压缩历史为摘要腾出上下文，可选指示摘要侧重点",
        group: CommandGroup::Conversation,
    },
    CommandSpec {
        name: "copy",
        aliases: &[],
        usage: "/copy [last|all]",
        desc: "复制最后一条回复（默认）或全部对话到剪贴板",
        group: CommandGroup::Conversation,
    },
    CommandSpec {
        name: "export",
        aliases: &[],
        usage: "/export [file]",
        desc: "导出会话为 Markdown 文件，缺省写到 mcode-export.md",
        group: CommandGroup::Conversation,
    },
    CommandSpec {
        name: "goal",
        aliases: &[],
        usage: "/goal <objective>",
        desc: "持续目标模式：循环执行直到判官宣判完成（上限 12 轮，Ctrl-C 退出）",
        group: CommandGroup::Conversation,
    },
    CommandSpec {
        name: "rename",
        aliases: &[],
        usage: "/rename <title>",
        desc: "重命名当前会话（落盘 sidecar 标题）",
        group: CommandGroup::Conversation,
    },
    CommandSpec {
        name: "resume",
        aliases: &[],
        usage: "/resume [id]",
        desc: "恢复历史会话；无参数 = 交互式选择最近会话",
        group: CommandGroup::Conversation,
    },
    CommandSpec {
        name: "rewind",
        aliases: &[],
        usage: "/rewind",
        desc: "回退会话到上一个检查点（双击 Esc 的指令入口）",
        group: CommandGroup::Conversation,
    },
    CommandSpec {
        name: "save",
        aliases: &[],
        usage: "/save",
        desc: "立即落盘当前会话",
        group: CommandGroup::Conversation,
    },
    CommandSpec {
        name: "quit",
        aliases: &["exit"],
        usage: "/quit",
        desc: "退出 mcode（会话已自动落盘）",
        group: CommandGroup::Conversation,
    },
    // ---------- 上下文与用量 ----------
    CommandSpec {
        name: "context",
        aliases: &[],
        usage: "/context",
        desc: "显示上下文窗口用量分解（系统提示/工具/消息/余量）",
        group: CommandGroup::Context,
    },
    CommandSpec {
        name: "cost",
        aliases: &[],
        usage: "/cost",
        desc: "当前会话累计成本估算",
        group: CommandGroup::Context,
    },
    CommandSpec {
        name: "diff",
        aliases: &[],
        usage: "/diff",
        desc: "显示工作区 git diff --stat 变更概要",
        group: CommandGroup::Context,
    },
    CommandSpec {
        name: "files",
        aliases: &[],
        usage: "/files",
        desc: "列出本会话触碰过的文件（来自工具调用目标）",
        group: CommandGroup::Context,
    },
    CommandSpec {
        name: "token",
        aliases: &[],
        usage: "/token",
        desc: "当前会话 token 用量（输入/输出/缓存读）",
        group: CommandGroup::Context,
    },
    // ---------- 配置 ----------
    CommandSpec {
        name: "effort",
        aliases: &[],
        usage: "/effort [low|medium|high]",
        desc: "查看或设置推理努力级别",
        group: CommandGroup::Config,
    },
    CommandSpec {
        name: "memory",
        aliases: &[],
        usage: "/memory",
        desc: "用 $EDITOR 打开 MCODE.md 记忆文件",
        group: CommandGroup::Config,
    },
    CommandSpec {
        name: "mcp",
        aliases: &[],
        usage: "/mcp",
        desc: "列出 MCP servers 与连接状态",
        group: CommandGroup::Config,
    },
    CommandSpec {
        name: "mode",
        aliases: &[],
        usage: "/mode [manual|auto|plan|passon]",
        desc: "查看或切换权限模式（缺省 = 循环切换）",
        group: CommandGroup::Config,
    },
    CommandSpec {
        name: "model",
        aliases: &[],
        usage: "/model [name]",
        desc: "查看或切换模型（缺省 = 交互选择）",
        group: CommandGroup::Config,
    },
    CommandSpec {
        name: "provider",
        aliases: &[],
        usage: "/provider [name]",
        desc: "查看或切换 API provider",
        group: CommandGroup::Config,
    },
    // ---------- 代理能力 ----------
    CommandSpec {
        name: "agents",
        aliases: &[],
        usage: "/agents",
        desc: "列出可用 subagents（内置/用户/项目）",
        group: CommandGroup::Agent,
    },
    CommandSpec {
        name: "hooks",
        aliases: &[],
        usage: "/hooks",
        desc: "列出已配置的 hooks（事件/匹配器/命令）",
        group: CommandGroup::Agent,
    },
    CommandSpec {
        name: "init",
        aliases: &[],
        usage: "/init",
        desc: "让模型分析代码库并生成 MCODE.md 项目说明",
        group: CommandGroup::Agent,
    },
    CommandSpec {
        name: "skills",
        aliases: &[],
        usage: "/skills",
        desc: "列出可用 skills（可用 /<skill名> 直接调用）",
        group: CommandGroup::Agent,
    },
    // ---------- 诊断与状态 ----------
    CommandSpec {
        name: "doctor",
        aliases: &[],
        usage: "/doctor",
        desc: "体检：配置/网络/权限/依赖自检并给出修复建议",
        group: CommandGroup::Diagnostics,
    },
    CommandSpec {
        name: "status",
        aliases: &[],
        usage: "/status",
        desc: "显示会话/模型/provider/权限模式等当前状态",
        group: CommandGroup::Diagnostics,
    },
];

/// 按名称或别名查指令（入参不含前导 `/`，大小写不敏感）
pub fn lookup(name: &str) -> Option<&'static CommandSpec> {
    let n = name.trim().trim_start_matches('/').to_ascii_lowercase();
    COMMANDS.iter().find(|c| {
        c.name.eq_ignore_ascii_case(&n) || c.aliases.iter().any(|a| a.eq_ignore_ascii_case(&n))
    })
}

/// 按分组聚合的有序迭代（/help 渲染用）
pub fn groups_in_order() -> [(CommandGroup, Vec<&'static CommandSpec>); 5] {
    let mut conv = vec![];
    let mut ctx = vec![];
    let mut cfg = vec![];
    let mut agent = vec![];
    let mut diag = vec![];
    for c in COMMANDS {
        match c.group {
            CommandGroup::Conversation => conv.push(c),
            CommandGroup::Context => ctx.push(c),
            CommandGroup::Config => cfg.push(c),
            CommandGroup::Agent => agent.push(c),
            CommandGroup::Diagnostics => diag.push(c),
        }
    }
    [
        (CommandGroup::Conversation, conv),
        (CommandGroup::Context, ctx),
        (CommandGroup::Config, cfg),
        (CommandGroup::Agent, agent),
        (CommandGroup::Diagnostics, diag),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_supports_names_and_aliases() {
        assert_eq!(lookup("model").map(|c| c.name), Some("model"));
        assert_eq!(lookup("/MODEL").map(|c| c.name), Some("model"));
        assert_eq!(lookup("exit").map(|c| c.name), Some("quit"));
        assert_eq!(lookup("?").map(|c| c.name), Some("help"));
        assert!(lookup("nonexistent").is_none());
    }

    #[test]
    fn every_spec_has_usage_and_desc() {
        for c in COMMANDS {
            assert!(c.usage.starts_with('/'), "{} usage 必须以 / 开头", c.name);
            assert!(!c.desc.is_empty(), "{} 缺作用描述", c.name);
            // usage 的指令头必须与 name 一致（防复制粘贴漂移）
            let head = c.usage[1..].split([' ', '[', '<']).next().unwrap_or("");
            assert_eq!(head, c.name, "{} usage 头与 name 不一致", c.name);
        }
    }

    #[test]
    fn help_renders_usage_and_desc_per_command() {
        // 用户要求：每个指令的提示都带用法+作用
        let mut text = String::new();
        for (group, cmds) in groups_in_order() {
            text.push_str(&format!("[{}]\n", group.title()));
            for c in cmds {
                text.push_str(&format!("{} — {}\n", c.usage, c.desc));
            }
        }
        assert!(text.contains("/model [name] — 查看或切换模型"));
        assert!(text.contains("/goal <objective>"));
        assert!(text.contains("显示全部指令或单个指令的用法与作用"));
    }
}
