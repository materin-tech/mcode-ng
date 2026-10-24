//! 路径收敛与文件性质判定——工具系统的安全地基。
//!
//! [`contain_to_workspace`] 是所有文件类工具（read/write/edit/glob/grep/@引用）
//! 的唯一入口守卫：`~` 展开、符号链接逃逸拒绝、新建文件的逐段校验，
//! 语义对齐 TS CLI guard.ts。file_refs / tools 都从这里取用，不许各写一份。

use std::path::{Component, Path, PathBuf};

/// 工作区路径收敛（防路径逃逸）：
/// - `~` / `~/x` 展开 HOME 后**不受**工作区限制（用户主目录天然可见，
///   对齐 TS CLI：@~/.ssh/config 允许读取，由权限层另行把关）；
/// - 相对路径基于 canonical 后的工作区根（规避 /var → /private/var 符号链接）；
/// - 已存在的目标 canonical 后必须仍在根内，否则拒绝；
/// - 不存在的目标（write 新文件）逐段校验，拒绝 `..` / 根前缀 / Windows 盘符。
pub fn contain_to_workspace(cwd: &Path, rel: &str) -> anyhow::Result<PathBuf> {
    let rel = rel.trim();
    let root = cwd.canonicalize()?;

    let joined: PathBuf = if rel == "~" {
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("无法获取 HOME 目录"))?
    } else if let Some(stripped) = rel.strip_prefix("~/") {
        let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("无法获取 HOME 目录"))?;
        home.join(stripped)
    } else {
        root.join(rel)
    };

    // `~` 展开的路径不走工作区收敛（HOME 在工作区外是合法输入）
    if rel == "~" || rel.starts_with("~/") {
        return Ok(joined);
    }

    if let Ok(c) = joined.canonicalize() {
        if c.starts_with(&root) {
            return Ok(c);
        }
        anyhow::bail!("路径逃逸被拒绝: {rel}");
    }
    // 目标不存在（write 新文件）：逐段校验，拒绝 `..` 与绝对路径组件
    let suffix = joined.strip_prefix(&root)?;
    let mut norm = root.clone();
    for comp in suffix.components() {
        match comp {
            Component::Normal(seg) => norm.push(seg),
            Component::CurDir => {}
            _ => anyhow::bail!("路径逃逸被拒绝: {rel}"),
        }
    }
    Ok(norm)
}

/// 二进制内容嗅探：前 8KB 含 NUL 字节即判二进制。
/// （扩展名清单只是预判，最终以内容为准——无扩展名的二进制也要拦住。）
pub fn is_probably_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8192).any(|&b| b == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mcode-paths-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d.canonicalize().unwrap()
    }

    #[test]
    fn resolves_relative_within_root() {
        let root = tmpdir("rel");
        std::fs::write(root.join("a.txt"), "x").unwrap();
        let p = contain_to_workspace(&root, "a.txt").unwrap();
        assert_eq!(p, root.join("a.txt"));
        // 不存在的新文件也放行（write 场景）
        let p = contain_to_workspace(&root, "sub/new.txt").unwrap();
        assert_eq!(p, root.join("sub/new.txt"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rejects_escape_via_dotdot() {
        let root = tmpdir("dotdot");
        assert!(contain_to_workspace(&root, "../outside").is_err());
        assert!(contain_to_workspace(&root, "a/../../b").is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rejects_escape_via_existing_symlink() {
        let root = tmpdir("sym");
        let outside = tmpdir("sym-outside");
        std::fs::write(outside.join("secret"), "s").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        // 符号链接指向根外 → canonical 后不在根内 → 拒绝
        assert!(contain_to_workspace(&root, "link/secret").is_err());
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn tilde_paths_bypass_workspace_containment() {
        let root = tmpdir("tilde");
        // HOME 下任意路径不要求在工作区内；无 HOME 环境报错而非 panic
        match dirs::home_dir() {
            Some(_) => {
                let p = contain_to_workspace(&root, "~/.mcode/config.json").unwrap();
                assert!(p.starts_with(dirs::home_dir().unwrap()));
            }
            None => {
                assert!(contain_to_workspace(&root, "~/.mcode/config.json").is_err());
            }
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn binary_sniff_by_nul() {
        assert!(!is_probably_binary(b"plain text content"));
        assert!(is_probably_binary(b"png\x00\x00data"));
        assert!(!is_probably_binary(&[b'a'; 9000])); // 无 NUL 的大文本不算二进制
    }
}
