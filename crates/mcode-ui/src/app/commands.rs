// MCode_Desktop — AI coding agent desktop app
// Copyright (c) 2026 Materin-tech. All rights reserved.
// SPDX-License-Identifier: MIT

//! slash 指令目录 — 唯一事实源在 `mcode_core::commands`（CLI 与 Desktop 共用）。
//! Desktop 侧只保留补全过滤与技能候选；指令目录/别名/分组随 core 表自动同步。

/// `/` 补全过滤：对齐 CLI 补全面板（大小写不敏感、name 前缀匹配，core 表为源）
pub fn filter_commands(query: &str) -> Vec<&'static mcode_core::commands::CommandSpec> {
    let q = query.to_lowercase();
    mcode_core::commands::COMMANDS
        .iter()
        .filter(|c| c.name.to_lowercase().starts_with(&q))
        .collect()
}

/// `/` 面板的技能候选：已加载技能按 name 过滤，展示形态与 CLI 一致
///（`技能：<描述前 60 字>`）。
pub fn slash_skill_candidates(
    skills: &[mcode_core::skills::SkillDefinition],
    query: &str,
) -> Vec<crate::app::completion::Candidate> {
    let q = query.to_lowercase();
    skills
        .iter()
        .filter(|s| s.name.to_lowercase().contains(&q))
        .map(|s| {
            let d = s.description.trim();
            let head: String = d.chars().take(60).collect();
            crate::app::completion::Candidate {
                name: s.name.clone(),
                desc: format!("技能：{head}"),
                is_dir: false,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcode_core::skills::SkillDefinition;
    use std::path::PathBuf;

    fn skill(name: &str, desc: &str) -> SkillDefinition {
        SkillDefinition {
            name: name.into(),
            description: desc.into(),
            path: PathBuf::new(),
            source_dir: PathBuf::new(),
        }
    }

    #[test]
    fn slash_skill_candidates_filter_and_format() {
        let skills = vec![
            skill("pua-loop", "以完成信号收尾的自主循环"),
            skill("gateguard", "路径门卫"),
        ];
        let hits = slash_skill_candidates(&skills, "pua");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "pua-loop");
        assert!(hits[0].desc.starts_with("技能："));
        assert!(hits[0].desc.contains("自主循环"));
        // 大小写不敏感 + 无命中为空
        assert_eq!(slash_skill_candidates(&skills, "PUA").len(), 1);
        assert!(slash_skill_candidates(&skills, "nope").is_empty());
    }

    #[test]
    fn command_table_sourced_from_core() {
        // 目录 = core 表（唯一事实源），Desktop 不再持有副本
        assert!(
            mcode_core::commands::COMMANDS.len() >= 28,
            "core 指令目录应 ≥ 28 条，实际 {}",
            mcode_core::commands::COMMANDS.len()
        );
        assert!(mcode_core::commands::lookup("token").is_some());
        assert!(mcode_core::commands::lookup("?").is_some());
        // 别名归一：? → help
        assert_eq!(
            mcode_core::commands::lookup("?").map(|c| c.name),
            Some("help")
        );
    }

    #[test]
    fn filter_matches_cli_prefix_behavior() {
        // CLI 口径：name 前缀匹配（非 contains）
        let names: Vec<_> = filter_commands("to").into_iter().map(|c| c.name).collect();
        assert!(names.contains(&"token"));
        assert!(!names.contains(&"todos"));
        let all: Vec<_> = filter_commands("").into_iter().map(|c| c.name).collect();
        assert!(all.contains(&"help"));
        assert!(filter_commands("不存在的指令").is_empty());
    }
}
