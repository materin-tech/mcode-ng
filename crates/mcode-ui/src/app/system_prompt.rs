// MCode Desktop — AI coding agent desktop app
// Copyright (c) 2026 Materin-tech. All rights reserved.
// SPDX-License-Identifier: MIT

//! 系统提示词组装（对齐 CLI agent/context.ts）：身份 + 决策策略 + 规则 +
//! 语言 + 工具清单 + skill 索引 + goal + MCODE.md + subagent 协议 + PUA 注入。

use mcode_core::config::Language;
use mcode_core::skills::SkillDefinition;
use mcode_core::subagents::AgentDefinition;

use super::assets::mcode_md_paths;

fn language_directive(lang: Language) -> &'static str {
    match lang {
        Language::ZhCn => "Always respond in Simplified Chinese.",
        Language::ZhTw => "Always respond in Traditional Chinese.",
        Language::EnUs => "Always respond in English.",
        Language::JaJp => "Always respond in Japanese.",
        Language::KoKr => "Always respond in Korean.",
    }
}

pub struct SystemPromptOpts<'a> {
    pub language: Language,
    pub tool_names: Vec<String>,
    pub skills: &'a [SkillDefinition],
    /// MCODE.md 记忆文档（None = 未加载）
    pub project_doc: Option<String>,
    pub goal: Option<String>,
    pub agents: &'a [AgentDefinition],
    pub pua_directive: Option<String>,
}

/// 组装系统提示词（返回单段；多段 cache 断点由调用方决定是否拆分）。
pub fn build_system_prompt(opts: SystemPromptOpts) -> String {
    let lang = language_directive(opts.language);
    let mut lines: Vec<String> = vec![
        "You are MCode, an interactive coding agent running on the user's desktop.".into(),
        String::new(),
        "DECISION POLICY — read this before every reply:".into(),
        "1. Understand what the user is actually asking FIRST.".into(),
        "2. If the question does NOT require this workspace, answer directly from your own knowledge and call NO tools.".into(),
        "3. Use tools ONLY when the task genuinely needs this workspace: reading or editing real files, running commands, or searching code the user explicitly wants you to work on.".into(),
        "4. When a task does need files, read only what the question requires — never explore files \"just in case\".".into(),
        "5. NEVER end a turn by only listing your limitations. websearch and webfetch can reach the network; attempt the most promising approach, then present the result.".into(),
        "6. If you genuinely cannot proceed, do not dead-end: ask ONE concrete follow-up question or offer 2-3 concrete options so the user can pick.".into(),
        "7. LONG TASKS: work incrementally. Complete ONE small step per response, verify it with tools, give a brief progress note, then continue. Write file-by-file with write_file, run checks between steps. Dispatch independent subtasks to subagents to keep this conversation lean.".into(),
        "8. CLOSURE DISCIPLINE (non-negotiable): NEVER self-report completion. Before saying a task is done: (a) restate the acceptance criteria; (b) run an actual verification against the real artifact — build it, run the tests, curl the server; (c) show the evidence output in your reply; (d) if something cannot be verified, say exactly what and why.".into(),
        "9. If a previous step claimed completion without evidence, treat that claim as unverified and verify it now before building on it.".into(),
        String::new(),
        "Rules:".into(),
        "- Never invent file contents; verify with tools when file facts matter.".into(),
        "- Keep the workspace boundary: all file paths must stay inside the workspace.".into(),
        "- After edits, summarize what changed in your reply.".into(),
        "- When you finish a task, state the result plainly with evidence (files changed, commands run).".into(),
        format!("- {lang}"),
        String::new(),
        format!("Available tools: {}", opts.tool_names.join(", ")),
    ];
    if !opts.skills.is_empty() {
        lines.push(String::new());
        lines.push("Skill index:".into());
        for s in opts.skills {
            lines.push(format!("- {}: {}", s.name, s.description));
        }
        lines.push(
            "Call the skill tool with a skill name to load its full instructions when relevant."
                .into(),
        );
    }
    if let Some(goal) = opts.goal.filter(|g| !g.is_empty()) {
        lines.push(String::new());
        lines.push("ACTIVE GOAL — work directly toward this until fully achieved.".into());
        lines.push(format!("GOAL: {goal}"));
        lines.push("Decompose complex goals: write an ordered subtask list first, then execute and verify each one.".into());
    }
    if let Some(doc) = opts.project_doc.filter(|d| !d.is_empty()) {
        lines.push(String::new());
        lines.push(doc);
    }
    if !opts.agents.is_empty() {
        lines.push(String::new());
        lines.push("SUBAGENT DISPATCH PROTOCOL:".into());
        lines.push("- For complex tasks (multi-step / multi-file / cross-domain), write an ordered subtask list first, then execute and verify each one.".into());
        lines.push("- Dispatch matching subtasks to subagents via the task tool (dispatch independent subtasks in parallel).".into());
        lines.push("- Do simple tasks directly without dispatching.".into());
        lines.push(format!(
            "- Available subagents: {}",
            opts.agents
                .iter()
                .map(|a| a.agent_type.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if let Some(pua) = opts.pua_directive.filter(|p| !p.is_empty()) {
        lines.push(String::new());
        lines.push(pua);
    }
    lines.join("\n")
}

/// MCODE.md 记忆文档：项目向上搜索 + 全局，20k 字符合并预算（对齐 CLI mdoc.ts）
pub fn load_project_doc(cwd: &std::path::Path) -> Option<String> {
    let mut parts = vec![];
    let mut used = 0usize;
    const BUDGET: usize = 20_000;
    for p in mcode_md_paths(cwd) {
        if used >= BUDGET {
            break;
        }
        let Ok(text) = std::fs::read_to_string(&p) else {
            continue;
        };
        if text.trim().is_empty() {
            continue;
        }
        let take: String = text.chars().take(BUDGET - used).collect();
        used += take.len();
        parts.push(format!("--- {} ---\n{}", p.display(), take));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n\n"))
    }
}

/// PUA 收敛纪律指令：pua skill 正文（≤8k）优先，否则内置短指令。
/// pua 正文来自 skill 注册表（bundled 资产物化 + ~/.mcode/assets pua-core）。
pub fn build_pua_directive(skills: &[SkillDefinition]) -> String {
    let header = "PUA CLOSURE DISCIPLINE — always-on:\n- Never self-report completion without verification evidence (commands run + output shown).\n- If a method failed twice, stop and switch to a fundamentally different approach.\n- Facts first: label guesses as hypotheses with a verification plan.";
    let body = skills
        .iter()
        .find(|s| s.name == "pua")
        .and_then(|s| std::fs::read_to_string(s.path.join("SKILL.md")).ok())
        .map(|t| t.chars().take(8_000).collect::<String>())
        .unwrap_or_default();
    if body.is_empty() {
        header.to_string()
    } else {
        format!("{header}\n\n--- pua skill ---\n{body}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_contains_sections() {
        let opts = SystemPromptOpts {
            language: Language::default(),
            tool_names: vec!["bash".into()],
            skills: &[],
            project_doc: None,
            goal: None,
            agents: &[],
            pua_directive: None,
        };
        let p = build_system_prompt(opts);
        assert!(p.contains("DECISION POLICY"));
        assert!(p.contains("Available tools: bash"));
        assert!(p.contains("Simplified Chinese"));
    }

    #[test]
    fn pua_fallback_header_without_skill() {
        let d = build_pua_directive(&[]);
        assert!(d.contains("PUA CLOSURE DISCIPLINE"));
    }
}
