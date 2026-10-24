//! plugins：`.mcode-plugin/plugin.json` 插件清单的加载与目录组合。
//!
//! 插件体系核心语义（清单字段兼容既有 PluginManifestSchema）：
//! - 插件 = 一个含 `.mcode-plugin/plugin.json` 的目录；
//! - `name` 必填且 kebab-case（小写字母/数字，`-` 分隔，无空格）；
//!   非法清单的插件跳过，不拖垮整体加载；
//! - 插件可贡献技能与子代理：约定 `skills/`、`agents/` 子目录，
//!   由 [`PluginRegistry::skill_dirs`] / [`PluginRegistry::agent_dirs`]
//!   汇出，交给 [`crate::skills::SkillRegistry`] /
//!   [`crate::subagents::AgentRegistry`] 的 load_dirs 组合；
//! - 来源分层：`~/.mcode/plugins`（用户）→ `.mcode/plugins`（项目覆盖）。

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// 清单里的 author 字段：允许纯字符串或 `{name, email, url}` 详情。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Author {
    /// `"张三"`
    Name(String),
    /// `{"name": "...", "email": "...", "url": "..."}`
    Detail {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        email: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
    },
}

impl Author {
    /// 展示用文本（`name` / `name <email>`）。
    pub fn display(&self) -> String {
        match self {
            Author::Name(n) => n.clone(),
            Author::Detail { name, email, .. } => match email {
                Some(e) => format!("{name} <{e}>"),
                None => name.clone(),
            },
        }
    }
}

/// `.mcode-plugin/plugin.json` 清单。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginManifest {
    /// 插件名：必填，kebab-case。
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<Author>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub homepage: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keywords: Vec<String>,
    /// 依赖的其他插件名（安装期检查；加载期不做解析）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dependencies: Vec<String>,
}

/// kebab-case：非空、小写字母数字、`-` 分隔（首尾段非空）。
pub fn is_kebab_case(name: &str) -> bool {
    !name.is_empty()
        && name.split('-').all(|seg| {
            !seg.is_empty()
                && seg
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        })
}

/// 一个已加载插件。
#[derive(Debug, Clone)]
pub struct Plugin {
    pub manifest: PluginManifest,
    /// 插件根目录（`<root>/.mcode-plugin/plugin.json`）。
    pub root: PathBuf,
}

impl Plugin {
    /// 清单文件路径。
    pub fn manifest_path(root: &Path) -> PathBuf {
        root.join(".mcode-plugin").join("plugin.json")
    }

    /// 读并校验清单；name 缺失或非 kebab-case → None。
    pub fn load(root: &Path) -> Option<Self> {
        let src = std::fs::read_to_string(Self::manifest_path(root)).ok()?;
        let manifest: PluginManifest = serde_json::from_str(&src).ok()?;
        if !is_kebab_case(&manifest.name) {
            return None;
        }
        Some(Plugin {
            manifest,
            root: root.to_path_buf(),
        })
    }

    fn contrib_dir(&self, kind: &str) -> Option<PathBuf> {
        let dir = self.root.join(kind);
        dir.is_dir().then_some(dir)
    }

    /// 插件技能目录（存在才返回）。
    pub fn skill_dir(&self) -> Option<PathBuf> {
        self.contrib_dir("skills")
    }

    /// 插件子代理目录（存在才返回）。
    pub fn agent_dir(&self) -> Option<PathBuf> {
        self.contrib_dir("agents")
    }
}

/// 插件注册表：按清单名索引，后加载的同名插件覆盖前者。
#[derive(Debug, Clone, Default)]
pub struct PluginRegistry {
    plugins: BTreeMap<String, Plugin>,
}

impl PluginRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, plugin: Plugin) {
        self.plugins.insert(plugin.manifest.name.clone(), plugin);
    }

    pub fn get(&self, name: &str) -> Option<&Plugin> {
        self.plugins.get(name)
    }

    pub fn names(&self) -> Vec<String> {
        self.plugins.keys().cloned().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }

    pub fn all(&self) -> Vec<&Plugin> {
        self.plugins.values().collect()
    }

    /// 扫描目录列表加载插件（`<dir>/<plugin>/.mcode-plugin/plugin.json`）。
    /// 目录不存在 / 无清单 / 清单非法 → 跳过；同名后者覆盖。
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
                    continue;
                }
                if let Some(p) = Plugin::load(&entry) {
                    reg.register(p);
                }
            }
        }
        reg
    }

    /// 所有插件的技能目录（按插件名序；交给 `SkillRegistry::load_dirs`）。
    pub fn skill_dirs(&self) -> Vec<PathBuf> {
        self.all().iter().filter_map(|p| p.skill_dir()).collect()
    }

    /// 所有插件的子代理目录（按插件名序；交给 `AgentRegistry::load_dirs`）。
    pub fn agent_dirs(&self) -> Vec<PathBuf> {
        self.all().iter().filter_map(|p| p.agent_dir()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 唯一临时目录。
    fn tmp_dir(tag: &str) -> PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("mcode-plugins-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 建一个插件目录（返回插件根）。
    fn make_plugin(base: &Path, dir_name: &str, manifest_json: &str) -> PathBuf {
        let root = base.join(dir_name);
        std::fs::create_dir_all(root.join(".mcode-plugin")).unwrap();
        std::fs::write(
            root.join(".mcode-plugin").join("plugin.json"),
            manifest_json,
        )
        .unwrap();
        root
    }

    #[test]
    fn loads_manifest_with_author_detail_and_defaults() {
        let base = tmp_dir("load");
        make_plugin(
            &base,
            "fmt-helper",
            r#"{
                "name": "fmt-helper",
                "version": "1.2.0",
                "description": "格式化帮手",
                "author": {"name": "团队", "email": "t@example.com"},
                "repository": "https://example.com/fmt-helper",
                "keywords": ["fmt", "rust"],
                "dependencies": ["base-kit"]
            }"#,
        );
        let reg = PluginRegistry::load_dirs(&[&base]);
        let p = reg.get("fmt-helper").expect("插件已加载");
        assert_eq!(p.manifest.version.as_deref(), Some("1.2.0"));
        assert_eq!(
            p.manifest.author.as_ref().map(|a| a.display()),
            Some("团队 <t@example.com>".into())
        );
        assert_eq!(p.manifest.keywords, vec!["fmt", "rust"]);
        assert_eq!(p.manifest.dependencies, vec!["base-kit"]);
        assert!(p.manifest.license.is_none(), "缺省字段不强制");
    }

    #[test]
    fn skips_missing_or_invalid_names_and_overrides_by_order() {
        let user = tmp_dir("user");
        let project = tmp_dir("project");
        // 无清单目录
        std::fs::create_dir_all(user.join("no-manifest")).unwrap();
        // 非法 JSON
        make_plugin(&user, "bad-json", "{ not json");
        // name 缺失
        make_plugin(&user, "no-name", r#"{"version": "1.0"}"#);
        // name 非法（大写 + 空格 + 首尾连字符）
        make_plugin(&user, "bad-name", r#"{"name": "Bad Name-"}"#);
        // 合法 + 同名覆盖
        make_plugin(
            &user,
            "deploy-kit",
            r#"{"name": "deploy-kit", "version": "0.1"}"#,
        );
        make_plugin(
            &project,
            "deploy-kit",
            r#"{"name": "deploy-kit", "version": "0.2"}"#,
        );

        let reg = PluginRegistry::load_dirs(&[&user, &project]);
        assert_eq!(reg.names(), vec!["deploy-kit".to_string()]);
        assert_eq!(
            reg.get("deploy-kit").unwrap().manifest.version.as_deref(),
            Some("0.2"),
            "项目覆盖用户"
        );
    }

    #[test]
    fn kebab_case_validation() {
        assert!(is_kebab_case("a"));
        assert!(is_kebab_case("fmt-helper-2"));
        assert!(!is_kebab_case(""));
        assert!(!is_kebab_case("-lead"));
        assert!(!is_kebab_case("trail-"));
        assert!(!is_kebab_case("Bad"));
        assert!(!is_kebab_case("has space"));
        assert!(!is_kebab_case("下划线"));
    }

    #[test]
    fn skill_and_agent_dirs_only_when_present_and_ordered_by_name() {
        let base = tmp_dir("contrib");
        let a = make_plugin(&base, "aaa-kit", r#"{"name": "aaa-kit"}"#);
        let z = make_plugin(&base, "zzz-kit", r#"{"name": "zzz-kit"}"#);
        std::fs::create_dir_all(a.join("skills")).unwrap();
        std::fs::create_dir_all(z.join("agents")).unwrap();

        let reg = PluginRegistry::load_dirs(&[&base]);
        let skills = reg.skill_dirs();
        let agents = reg.agent_dirs();
        assert_eq!(skills, vec![a.join("skills")], "只有存在的目录才汇出");
        assert_eq!(agents, vec![z.join("agents")]);
    }

    #[test]
    fn plugin_skills_feed_skill_registry() {
        let base = tmp_dir("compose");
        let root = make_plugin(&base, "greet-kit", r#"{"name": "greet-kit"}"#);
        let skill_dir = root.join("skills").join("hello");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\ndescription: 插件技能\n---\n正文",
        )
        .unwrap();

        let plugins = PluginRegistry::load_dirs(&[&base]);
        let plugin_skill_dirs = plugins.skill_dirs();
        let dirs: Vec<&std::path::Path> = plugin_skill_dirs.iter().map(|p| p.as_path()).collect();
        let skills = crate::skills::SkillRegistry::load_dirs(&dirs);
        assert_eq!(
            skills.get("hello").map(|s| s.description.as_str()),
            Some("插件技能")
        );
    }
}
