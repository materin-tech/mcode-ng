// MCode Desktop — AI coding agent desktop app
// Copyright (c) 2026 Materin-tech. All rights reserved.
// SPDX-License-Identifier: MIT

//! 扩展管理 CRUD（skill / agent 的创建、安装、删除）+ 会话重命名/删除/导出。
//!
//! 新 core 只做「目录发现」（SkillRegistry / AgentRegistry::load_dirs）；
//! 写操作是桌面端设置页的职责，落盘格式与 core 发现格式一致：
//! skill = `<base>/<name>/SKILL.md`，agent = `<base>/<name>.md`。
//! 会话重命名用 sidecar `<session_id>.title`（core 的 jsonl Line 无 title 记录）。

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// skill CRUD
// ---------------------------------------------------------------------------

pub fn validate_skill_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    anyhow::ensure!(ok, "非法 skill 名 `{name}`（仅限字母数字-_，≤64 字符）");
    Ok(())
}

fn skills_base(project_dir: Option<&Path>) -> PathBuf {
    match project_dir {
        Some(p) => p.join(".mcode").join("skills"),
        None => mcode_core::util::mcode_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("skills"),
    }
}

pub fn create_skill(
    name: &str,
    description: &str,
    body: &str,
    project_dir: Option<&Path>,
) -> Result<PathBuf> {
    validate_skill_name(name)?;
    anyhow::ensure!(
        !description.trim().is_empty(),
        "description 不能为空（它是常驻上下文的唯一线索）"
    );
    let dir = skills_base(project_dir).join(name);
    anyhow::ensure!(!dir.exists(), "skill `{name}` 已存在于 {}", dir.display());
    std::fs::create_dir_all(&dir)?;
    let md = format!(
        "---\nname: {name}\ndescription: {}\n---\n{}\n",
        description.trim(),
        body.trim()
    );
    std::fs::write(dir.join("SKILL.md"), md)?;
    Ok(dir)
}

pub fn install_skill_from_dir(src: &Path, project_dir: Option<&Path>) -> Result<PathBuf> {
    let src_md = src.join("SKILL.md");
    anyhow::ensure!(src_md.is_file(), "源目录缺少 SKILL.md：{}", src.display());
    let text = std::fs::read_to_string(&src_md)?;
    let name = frontmatter_skill_name(&text)
        .with_context(|| format!("SKILL.md frontmatter 解析失败：{}", src.display()))?;
    validate_skill_name(&name)?;
    let dir = skills_base(project_dir).join(&name);
    anyhow::ensure!(!dir.exists(), "skill `{name}` 已存在于 {}", dir.display());
    super::assets::copy_tree(src, &dir)
        .with_context(|| format!("拷贝 {} → {} 失败", src.display(), dir.display()))?;
    Ok(dir)
}

pub fn remove_skill(name: &str, project_dir: Option<&Path>) -> Result<bool> {
    validate_skill_name(name)?;
    let dir = skills_base(project_dir).join(name);
    if !dir.is_dir() {
        return Ok(false);
    }
    std::fs::remove_dir_all(&dir)?;
    Ok(true)
}

/// SKILL.md frontmatter 里的 name（没有 name 键时回退目录名由调用方处理）
fn frontmatter_skill_name(text: &str) -> Result<String> {
    let (fm, _) = mcode_core::util::frontmatter::split(text.trim_start())
        .context("缺少 `---` 包裹的 frontmatter")?;
    let name = mcode_core::util::frontmatter::get(&fm, "name")
        .context("frontmatter 缺少 name 键")?
        .to_string();
    Ok(name)
}

// ---------------------------------------------------------------------------
// agent CRUD
// ---------------------------------------------------------------------------

pub fn validate_agent_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    anyhow::ensure!(ok, "非法 agent 名 `{name}`（仅限字母数字-_，≤64 字符）");
    Ok(())
}

fn agents_base(project_dir: Option<&Path>) -> PathBuf {
    match project_dir {
        Some(p) => p.join(".mcode").join("agents"),
        None => mcode_core::util::mcode_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("agents"),
    }
}

pub fn create_agent(
    name: &str,
    description: &str,
    system_prompt: &str,
    tools: Option<Vec<String>>,
    project_dir: Option<&Path>,
) -> Result<PathBuf> {
    validate_agent_name(name)?;
    anyhow::ensure!(
        !description.trim().is_empty(),
        "description 不能为空（主模型据此决定何时派发）"
    );
    anyhow::ensure!(
        !system_prompt.trim().is_empty(),
        "system_prompt（正文）不能为空"
    );
    let dir = agents_base(project_dir);
    let path = dir.join(format!("{name}.md"));
    anyhow::ensure!(!path.exists(), "agent `{name}` 已存在于 {}", path.display());
    std::fs::create_dir_all(&dir)?;
    let tools_line = tools
        .map(|t| format!("tools: {}\n", t.join(", ")))
        .unwrap_or_default();
    let md = format!(
        "---\nname: {name}\ndescription: {}\n{tools_line}---\n{}\n",
        description.trim(),
        system_prompt.trim()
    );
    std::fs::write(&path, md)?;
    Ok(path)
}

pub fn install_agent_from_file(src: &Path, project_dir: Option<&Path>) -> Result<PathBuf> {
    let text =
        std::fs::read_to_string(src).with_context(|| format!("读取失败：{}", src.display()))?;
    let default_name = src
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("agent")
        .to_string();
    let def = mcode_core::subagents::parse_agent_markdown(&text, &default_name);
    validate_agent_name(&def.agent_type)?;
    let dir = agents_base(project_dir);
    let path = dir.join(format!("{}.md", def.agent_type));
    anyhow::ensure!(
        !path.exists(),
        "agent `{}` 已存在于 {}",
        def.agent_type,
        path.display()
    );
    std::fs::create_dir_all(&dir)?;
    std::fs::copy(src, &path)?;
    Ok(path)
}

pub fn remove_agent(name: &str, project_dir: Option<&Path>) -> Result<bool> {
    validate_agent_name(name)?;
    let path = agents_base(project_dir).join(format!("{name}.md"));
    if !path.is_file() {
        return Ok(false);
    }
    std::fs::remove_file(&path)?;
    Ok(true)
}

// ---------------------------------------------------------------------------
// 会话管理：重命名（sidecar）/ 删除
// ---------------------------------------------------------------------------

/// 会话重命名：sidecar `<session_id>.title` 与 jsonl 同目录。
/// None = 恢复默认标题（删 sidecar，回落首条用户消息）。
pub fn rename_session(store: &mcode_core::sessions::SessionStore, id: &str, title: Option<&str>) {
    let sidecar = store.dir().join(format!("{id}.title"));
    match title {
        Some(t) => {
            let _ = std::fs::write(&sidecar, t.trim());
        }
        None => {
            let _ = std::fs::remove_file(&sidecar);
        }
    }
}

/// 会话自定义标题（无 sidecar → None，调用方回落 SessionSummary.title）
pub fn custom_session_title(
    store: &mcode_core::sessions::SessionStore,
    id: &str,
) -> Option<String> {
    std::fs::read_to_string(store.dir().join(format!("{id}.title")))
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
}

/// 删除会话：jsonl + sidecar 一并移除。
pub fn delete_session(store: &mcode_core::sessions::SessionStore, id: &str) -> Result<bool> {
    let path = store.dir().join(format!("{id}.jsonl"));
    if !path.is_file() {
        return Ok(false);
    }
    std::fs::remove_file(&path)?;
    let _ = std::fs::remove_file(store.dir().join(format!("{id}.title")));
    Ok(true)
}

/// 会话 id = jsonl 文件名（去扩展名）。
pub fn session_id_of(handle: &mcode_core::sessions::SessionHandle) -> String {
    handle
        .path()
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_crud_roundtrip() {
        let dir = std::env::temp_dir().join(format!("mcode_ui_skill_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = create_skill("demo-skill", "演示", "正文", Some(&dir)).unwrap();
        assert!(p.join("SKILL.md").is_file());
        assert!(create_skill("demo-skill", "重复", "", Some(&dir)).is_err());
        assert!(validate_skill_name("bad name").is_err());
        assert!(remove_skill("demo-skill", Some(&dir)).unwrap());
        assert!(!remove_skill("demo-skill", Some(&dir)).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn skill_install_requires_frontmatter_name() {
        let dir = std::env::temp_dir().join(format!("mcode_ui_skillin_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("src").join("SKILL.md"),
            "---\nname: inst\ndescription: d\n---\nb\n",
        )
        .unwrap();
        let p = install_skill_from_dir(&dir.join("src"), Some(&dir)).unwrap();
        assert!(p.ends_with("inst"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn agent_crud_roundtrip() {
        let dir = std::env::temp_dir().join(format!("mcode_ui_agent_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = create_agent("worker", "干活的", "你是一个工人", None, Some(&dir)).unwrap();
        assert!(p.is_file());
        assert!(create_agent("worker", "重复", "x", None, Some(&dir)).is_err());
        assert!(remove_agent("worker", Some(&dir)).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn session_rename_delete_via_sidecar() {
        let root = std::env::temp_dir().join(format!("mcode_ui_sess_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let cwd = root.join("proj");
        std::fs::create_dir_all(&cwd).unwrap();
        let store = mcode_core::sessions::SessionStore::with_base(root.clone());
        let (handle, _meta) = store.start(&cwd, None).unwrap();
        let id = session_id_of(&handle);
        rename_session(&store, &id, Some("我的标题"));
        assert_eq!(
            custom_session_title(&store, &id).as_deref(),
            Some("我的标题")
        );
        rename_session(&store, &id, None);
        assert!(custom_session_title(&store, &id).is_none());
        assert!(delete_session(&store, &id).unwrap());
        assert!(!delete_session(&store, &id).unwrap());
        let _ = std::fs::remove_dir_all(&root);
    }
}
