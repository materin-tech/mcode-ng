//! Markdown frontmatter 解析（agents / skills / commands 共用）。
//!
//! 手写 YAML-lite：`---` 包裹的 `key: value` 行集合。覆盖配置文件的
//! 全部实际用到的形态——标量、带引号标量、逗号分隔列表。不引入完整
//! YAML 依赖（KISS：schema 形态我们自己定义，不需要 YAML 的表达力）。

/// 拆 frontmatter：`---` 开头的文件 → (键值对, 正文)。
/// 无 frontmatter → None（正文即整个文件，调用方自行处理）。
pub fn split(src: &str) -> Option<(Vec<(String, String)>, String)> {
    let trimmed = src.trim_start();
    let rest = trimmed.strip_prefix("---")?;
    let mut fm: Vec<(String, String)> = Vec::new();
    let mut body = String::new();
    let mut in_fm = true;
    for line in rest.lines() {
        if in_fm {
            let t = line.trim();
            if t == "---" {
                in_fm = false;
                continue;
            }
            if t.is_empty() || t.starts_with('#') {
                continue;
            }
            // 正文里出现无法解析的行（frontmatter 从未闭合就遇到非 KV 行）
            // → 视为无 frontmatter 文件
            let (k, v) = t.split_once(':')?;
            fm.push((k.trim().to_string(), trim_value(v)));
        } else {
            body.push_str(line);
            body.push('\n');
        }
    }
    if in_fm {
        // `---` 未闭合：整个文件当正文
        return None;
    }
    Some((fm, body))
}

/// 值清洗：去首尾空白与成对引号。
pub fn trim_value(v: &str) -> String {
    let v = v.trim();
    let v = v
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| v.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
        .unwrap_or(v);
    v.trim().to_string()
}

/// 键值对里取键（大小写不敏感）。
pub fn get<'a>(fm: &'a [(String, String)], key: &str) -> Option<&'a str> {
    fm.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v)| v.as_str())
}

/// 键值对里取逗号分隔列表（空片段剔除）。
pub fn get_list(fm: &[(String, String)], key: &str) -> Option<Vec<String>> {
    get(fm, key).map(|v| {
        v.split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_well_formed_frontmatter() {
        let (fm, body) = split("---\nname: a\ndescription: \"带引号\"\n---\n正文").unwrap();
        assert_eq!(get(&fm, "name"), Some("a"));
        assert_eq!(get(&fm, "DESCRIPTION"), Some("带引号"));
        assert_eq!(body, "正文\n");
    }

    #[test]
    fn no_frontmatter_returns_none() {
        assert!(split("纯正文").is_none());
        // 未闭合
        assert!(split("---\nname: a\n没有冒号的行").is_none());
        // `---` 后没有内容
        assert!(split("").is_none());
    }

    #[test]
    fn lists_split_on_comma() {
        let (fm, _) = split("---\ntools: read_file, write_file ,\n---\nb").unwrap();
        assert_eq!(
            get_list(&fm, "tools"),
            Some(vec!["read_file".to_string(), "write_file".to_string()])
        );
    }
}
