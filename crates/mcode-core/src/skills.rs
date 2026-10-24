//! skills：`<name>/SKILL.md` 技能定义的加载、注册与 `skill` 工具。
//!
//! 技能体系核心语义：
//! - 目录格式 `<skills-dir>/<name>/SKILL.md`（不支持裸 .md 文件），
//!   技能名 = 目录名（frontmatter name 只是校验/展示）；
//! - 来源分层：`~/.mcode/skills`（用户）→ `.mcode/skills`（项目覆盖）→
//!   插件技能（由 plugins 层把目录汇入 [`SkillRegistry::load_dirs`]）；
//! - `skill` 工具让模型主动调用：执行 = 把 SKILL.md 正文装进对话，
//!   让模型按说明书行动；
//! - `/名称` 的 slash 触发由交互层复用 [`SkillRegistry`] 实现（TUI 里程碑）。

use crate::tools::{Tool, ToolContext, ToolDefinition, ToolResult};
use anyhow::Result;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// 一个已加载技能。
#[derive(Debug, Clone)]
pub struct SkillDefinition {
    /// 技能名（= 目录名；slash 命令名与工具入参）。
    pub name: String,
    /// 干什么用（frontmatter description；技能清单展示给模型选型）。
    pub description: String,
    /// SKILL.md 路径（执行时读正文）。
    pub path: PathBuf,
    /// 来源目录（`/` 前缀补全与去重展示用）。
    pub source_dir: PathBuf,
}

/// 从 SKILL.md 内容解析技能。技能名 = 目录名（frontmatter name 不参与寻址）。
pub fn parse_skill_markdown(src: &str, dir_name: &str) -> SkillDefinition {
    let (fm, _body) = crate::util::frontmatter::split(src).unwrap_or_default();
    SkillDefinition {
        name: dir_name.to_string(),
        description: crate::util::frontmatter::get(&fm, "description")
            .unwrap_or_default()
            .to_string(),
        path: PathBuf::new(),
        source_dir: PathBuf::new(),
    }
}

/// 技能注册表。
#[derive(Debug, Clone, Default)]
pub struct SkillRegistry {
    skills: BTreeMap<String, SkillDefinition>,
}

/// 内置资产技能层：桌面端启动物化到 `~/.mcode/assets` 的 ecc-core/pua-core，
/// CLI 与桌面端同源加载（目录不存在时 `load_dirs` 自然跳过）。
pub fn bundled_dirs() -> Vec<PathBuf> {
    bundled_dirs_at(crate::util::mcode_dir().as_deref())
}

/// [`bundled_dirs`] 的可注入形态（测试与自定义 HOME 用）。
pub fn bundled_dirs_at(home: Option<&Path>) -> Vec<PathBuf> {
    let Some(h) = home else {
        return Vec::new();
    };
    vec![
        h.join("assets").join("ecc-core").join("skills"),
        h.join("assets").join("pua-core"),
    ]
}

impl SkillRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, def: SkillDefinition) {
        self.skills.insert(def.name.clone(), def);
    }

    pub fn get(&self, name: &str) -> Option<&SkillDefinition> {
        self.skills.get(name)
    }

    pub fn names(&self) -> Vec<String> {
        self.skills.keys().cloned().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    pub fn all(&self) -> Vec<&SkillDefinition> {
        self.skills.values().collect()
    }

    /// skill 工具描述里的选型清单：`- name: description`。
    pub fn list_for_prompt(&self) -> String {
        if self.skills.is_empty() {
            return "（当前没有可用技能）".into();
        }
        self.skills
            .values()
            .map(|s| format!("- {}: {}", s.name, s.description))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// 扫描目录列表加载技能（`<dir>/<name>/SKILL.md`；后者覆盖前者同名）。
    /// 目录不存在 → 跳过；缺 SKILL.md 的目录跳过；坏文件宽容（不拖垮整体）。
    /// 扫描目录列表加载技能（`<dir>/<name>/SKILL.md`；后者覆盖前者同名）。
    /// 目录不存在 → 跳过；缺 SKILL.md 的目录跳过；坏文件宽容（不拖垮整体）。
    pub fn load_dirs(dirs: &[&Path]) -> Self {
        let mut reg = Self::new();
        for dir in dirs {
            let Ok(rd) = std::fs::read_dir(dir) else {
                continue;
            };
            let mut entries: Vec<_> = rd.filter_map(|e| e.ok()).map(|e| e.path()).collect();
            entries.sort();
            for entry in entries {
                if !entry.is_dir() {
                    continue; // 裸文件不支持
                }
                let skill_file = entry.join("SKILL.md");
                let Ok(src) = std::fs::read_to_string(&skill_file) else {
                    continue;
                };
                let dir_name = entry
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("skill")
                    .to_string();
                let mut def = parse_skill_markdown(&src, &dir_name);
                def.path = skill_file;
                def.source_dir = dir.to_path_buf();
                reg.register(def);
            }
        }
        reg
    }
}

// ---------------------------------------------------------------------------
// skill 工具
// ---------------------------------------------------------------------------

/// `skill`：让模型主动调用已安装技能（把 SKILL.md 正文装进对话）。
pub struct SkillTool {
    skills: std::sync::RwLock<SkillRegistry>,
}

impl SkillTool {
    pub fn new(registry: SkillRegistry) -> Self {
        SkillTool {
            skills: std::sync::RwLock::new(registry),
        }
    }

    /// 热刷新（插件/技能目录变化时由交互层调用）。
    pub fn replace_registry(&self, registry: SkillRegistry) {
        if let Ok(mut r) = self.skills.write() {
            *r = registry;
        }
    }

    fn current(&self) -> Option<SkillRegistry> {
        self.skills.read().ok().map(|r| r.clone())
    }
}

#[async_trait::async_trait]
impl Tool for SkillTool {
    fn def(&self) -> ToolDefinition {
        let list = self
            .current()
            .map(|r| r.list_for_prompt())
            .unwrap_or_else(|| "（当前没有可用技能）".into());
        ToolDefinition {
            name: "skill".into(),
            description: format!(
                "调用一个已安装技能：把该技能的操作手册装进对话并遵循执行。\
                可用技能：\n{list}\n\
                用户消息里的 /名称 也指向这些技能。"
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "技能名" },
                    "args": { "type": "string", "description": "传给技能的参数（可选）" }
                },
                "required": ["name"]
            }),
            is_read_only: true,
        }
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> Result<ToolResult> {
        let name = input["name"].as_str().unwrap_or_default().trim();
        if name.is_empty() {
            return Ok(ToolResult::err("skill: name 不能为空"));
        }
        let args = input["args"].as_str().unwrap_or_default().trim();
        let Some(skill) = self.current().and_then(|r| r.get(name).cloned()) else {
            let names = self
                .current()
                .map(|r| r.names().join(", "))
                .unwrap_or_default();
            return Ok(ToolResult::err(format!(
                "skill: 未知技能 {name}。可用: {names}"
            )));
        };
        let body = std::fs::read_to_string(&skill.path).unwrap_or_default();
        if body.trim().is_empty() {
            return Ok(ToolResult::err(format!(
                "skill: 技能 {} 的 SKILL.md 为空或不可读",
                skill.name
            )));
        }
        let args_note = if args.is_empty() {
            String::new()
        } else {
            format!("\n\n用户参数：{args}")
        };
        Ok(ToolResult::ok(format!(
            "已装载技能 {}。按以下手册执行：\n\n---\n{}---{args_note}",
            skill.name, body
        )))
    }
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_dirs_point_at_materialized_tiers() {
        let dirs = bundled_dirs_at(Some(Path::new("/home/u")));
        assert_eq!(dirs.len(), 2);
        assert!(dirs[0].ends_with("assets/ecc-core/skills"));
        assert!(dirs[1].ends_with("assets/pua-core"));
        // 无 HOME（极端环境）→ 空，load_dirs 侧自然退化为三层
        assert!(bundled_dirs_at(None).is_empty());
    }

    /// 唯一临时目录。
    fn tmp_dir(tag: &str) -> PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("mcode-skills-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 建一个技能目录（返回技能目录路径）。
    fn make_skill(base: &Path, name: &str, frontmatter: &str, body: &str) -> PathBuf {
        let dir = base.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\n{frontmatter}\n---\n{body}"),
        )
        .unwrap();
        dir
    }

    #[test]
    fn parses_skill_frontmatter_with_dir_name_priority() {
        let base = tmp_dir("parse");
        // frontmatter name 与目录名不一致 → 目录名优先
        make_skill(
            &base,
            "review-pr",
            "name: 别名\ndescription: 审查 PR",
            "步骤…",
        );
        let reg = SkillRegistry::load_dirs(&[&base]);
        let s = reg.get("review-pr").expect("目录名优先");
        assert_eq!(s.description, "审查 PR");
        assert!(s.path.ends_with("SKILL.md"));
    }

    #[test]
    fn project_overrides_user_and_bare_files_skipped() {
        let user = tmp_dir("user");
        let project = tmp_dir("project");
        make_skill(&user, "deploy", "description: 用户版部署", "user body");
        make_skill(
            &project,
            "deploy",
            "description: 项目版部署",
            "project body",
        );
        // 裸文件不支持
        std::fs::write(user.join("loose.md"), "---\ndescription: x\n---\nb").unwrap();
        // 缺 SKILL.md 的目录跳过
        std::fs::create_dir_all(user.join("empty")).unwrap();

        let reg = SkillRegistry::load_dirs(&[&user, &project]);
        let d = reg.get("deploy").expect("项目覆盖用户");
        assert_eq!(d.description, "项目版部署");
        assert!(d.path.starts_with(&project));
        assert!(reg.get("loose").is_none(), "裸 .md 文件不支持");
        assert!(reg.get("empty").is_none(), "缺 SKILL.md 跳过");
        assert_eq!(reg.all().len(), 1);
    }

    #[test]
    fn prompt_list_renders_entries() {
        let base = tmp_dir("list");
        make_skill(&base, "a", "description: 技能A", "b");
        let reg = SkillRegistry::load_dirs(&[&base]);
        let list = reg.list_for_prompt();
        assert!(list.contains("- a: 技能A"), "{list}");
        assert!(SkillRegistry::new()
            .list_for_prompt()
            .contains("没有可用技能"));
    }

    #[tokio::test]
    async fn skill_tool_loads_manual_into_result() {
        let base = tmp_dir("tool");
        make_skill(&base, "greet", "description: 打招呼", "见到人说你好");
        let tool = SkillTool::new(SkillRegistry::load_dirs(&[&base]));
        let (ctx, _tx) = ToolContext::for_tests(std::env::temp_dir());

        let r = tool.execute(json!({"name": "greet"}), &ctx).await.unwrap();
        assert!(r.success, "{}", r.output);
        assert!(r.output.contains("见到人说你好"), "{}", r.output);
        assert!(r.output.contains("已装载技能 greet"));

        // 带参数
        let r = tool
            .execute(json!({"name": "greet", "args": "对小明"}), &ctx)
            .await
            .unwrap();
        assert!(r.output.contains("用户参数：对小明"));

        // 未知技能
        let r = tool.execute(json!({"name": "nope"}), &ctx).await.unwrap();
        assert!(!r.success);
        assert!(r.output.contains("未知技能 nope"));
    }
}
