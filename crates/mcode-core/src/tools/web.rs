//! 网络工具：web_fetch / web_search。
//!
//! 对齐 TS 版跨区域策略：搜索走 DuckDuckGo HTML，被限流/拦截时回落
//! Bing CN——纯 HTTP 抓取，无 API key 依赖（自建网关用户零配置可用）。

use super::{DisplayBlock, Tool, ToolContext, ToolDefinition, ToolResult};
use anyhow::Result;
use serde_json::{json, Value};

const MAX_FETCH_CHARS: usize = 20_000;
const MAX_RESULTS: usize = 8;
const UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0 Safari/537.36";

fn http_client(timeout_secs: u64) -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(UA)
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .build()?)
}

// ---------------------------------------------------------------------------
// web_fetch
// ---------------------------------------------------------------------------

pub struct WebFetch;

#[async_trait::async_trait]
impl Tool for WebFetch {
    fn def(&self) -> ToolDefinition {
        ToolDefinition {
            name: "web_fetch".into(),
            description: "抓取 URL 的可读文本（HTML 转纯文本，默认截断 2 万字符）。\
                只支持 http/https。"
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string" },
                    "max_chars": { "type": "number", "description": "默认 20000，上限 100000" }
                },
                "required": ["url"]
            }),
            is_read_only: true,
        }
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> Result<ToolResult> {
        let url = input["url"].as_str().unwrap_or_default().to_string();
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return Ok(ToolResult::err(format!(
                "web_fetch: 仅支持 http/https URL，收到 {url:?}"
            )));
        }
        let max = input["max_chars"]
            .as_u64()
            .unwrap_or(MAX_FETCH_CHARS as u64)
            .clamp(1000, 100_000) as usize;
        let client = http_client(30)?;
        let resp = client.get(&url).send().await;
        let resp = match resp {
            Ok(r) => r,
            Err(e) => return Ok(ToolResult::err(format!("web_fetch {url}: {e}"))),
        };
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Ok(ToolResult::err(format!("web_fetch {url}: HTTP {status}")));
        }
        let text = html_to_text(&body);
        let truncated: String = text.chars().take(max).collect();
        let mut out = truncated;
        if text.chars().count() > max {
            out.push_str(&format!(
                "\n\n[已截断，原文共 {} 字符]",
                text.chars().count()
            ));
        }
        Ok(ToolResult::ok(out).with_display(DisplayBlock::FileRef {
            path: url,
            lines: String::new(),
        }))
    }
}

/// HTML → 可读文本：去 script/style、块级标签换行、去标签、实体解码、压缩空白。
pub fn html_to_text(html: &str) -> String {
    let mut s = strip_blocks(html);
    for t in [
        "</p>", "</div>", "<br>", "<br/>", "<br />", "</h1>", "</h2>", "</h3>", "</h4>", "</li>",
        "</tr>",
    ] {
        s = s.replace(t, "\n");
    }
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    decode_entities(&out)
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn strip_blocks(html: &str) -> String {
    let mut s = html.to_string();
    for tag in ["script", "style", "noscript"] {
        let open = format!("<{tag}");
        let close = format!("</{tag}>");
        while let Some(start) = s.find(&open) {
            let Some(end_rel) = s[start..].find(&close) else {
                break;
            };
            let end = (start + end_rel + tag.len() + 3).min(s.len());
            s.replace_range(start..end, " ");
        }
    }
    s
}

pub fn decode_entities(s: &str) -> String {
    s.replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}

// ---------------------------------------------------------------------------
// web_search
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

pub struct WebSearch;

#[async_trait::async_trait]
impl Tool for WebSearch {
    fn def(&self) -> ToolDefinition {
        ToolDefinition {
            name: "web_search".into(),
            description: "网络搜索，返回 标题 + URL + 摘要（DuckDuckGo，被拦截时回落 Bing）。\
                回答后附上来源链接。"
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "max_results": { "type": "number", "description": "≤8，默认 5" }
                },
                "required": ["query"]
            }),
            is_read_only: true,
        }
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> Result<ToolResult> {
        let query = input["query"].as_str().unwrap_or_default().to_string();
        if query.trim().is_empty() {
            return Ok(ToolResult::err("web_search: query 不能为空"));
        }
        let max = input["max_results"]
            .as_u64()
            .unwrap_or(5)
            .clamp(1, MAX_RESULTS as u64) as usize;
        let client = http_client(20)?;
        // 1) DuckDuckGo HTML（失败/限流 → 空 body，由 Bing 回落兜底）
        let body = match client
            .post("https://html.duckduckgo.com/html/")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(format!("q={}", urlencode(&query)))
            .send()
            .await
        {
            Ok(r) => r.text().await.unwrap_or_default(),
            Err(_) => String::new(),
        };
        let mut results = parse_ddg(&body, max);
        // 2) 回落 Bing CN
        if results.is_empty() {
            let bing_url = format!(
                "https://cn.bing.com/search?q={}&setmkt=zh-CN",
                urlencode(&query)
            );
            if let Ok(resp) = client.get(&bing_url).send().await {
                if let Ok(html) = resp.text().await {
                    results = parse_bing(&html, max);
                }
            }
        }
        if results.is_empty() {
            return Ok(ToolResult::err(
                "web_search: 无搜索结果（可能被限流），可稍后重试或用 web_fetch 直接访问已知 URL",
            ));
        }
        let mut out = String::new();
        for (i, r) in results.iter().enumerate() {
            out.push_str(&format!(
                "{}. {}\n   {}\n   {}\n\n",
                i + 1,
                r.title,
                r.url,
                r.snippet
            ));
        }
        Ok(ToolResult::ok(out).with_display(DisplayBlock::FileRef {
            path: format!("web_search:{query}"),
            lines: String::new(),
        }))
    }
}

/// 解析 DuckDuckGo HTML 结果：result__a 链接 + result__snippet。
pub fn parse_ddg(html: &str, max: usize) -> Vec<SearchResult> {
    let re_link = regex::Regex::new(
        r#"<a[^>]*class="[^"]*result__a[^"]*"[^>]*href="([^"]+)"[^>]*>([\s\S]*?)</a>"#,
    )
    .expect("静态正则");
    let re_snip =
        regex::Regex::new(r#"<a[^>]*class="[^"]*result__snippet[^"]*"[^>]*>([\s\S]*?)</a>"#)
            .expect("静态正则");
    let links: Vec<(String, String)> = re_link
        .captures_iter(html)
        .map(|c| (c[1].to_string(), html_to_text(&c[2])))
        .collect();
    let snippets: Vec<String> = re_snip
        .captures_iter(html)
        .map(|c| html_to_text(&c[1]))
        .collect();
    links
        .into_iter()
        .take(max)
        .enumerate()
        .map(|(i, (url, title))| SearchResult {
            title,
            url: decode_ddg_url(&url),
            snippet: snippets.get(i).cloned().unwrap_or_default(),
        })
        .collect()
}

/// 解析 Bing CN 结果（li.b_algo 块）。
pub fn parse_bing(html: &str, max: usize) -> Vec<SearchResult> {
    let re_block = regex::Regex::new(
        r#"<li class="b_algo"[\s\S]*?<h2><a[^>]+href="([^"]+)"[^>]*>([\s\S]*?)</a></h2>([\s\S]*?)</li>"#,
    )
    .expect("静态正则");
    re_block
        .captures_iter(html)
        .take(max)
        .map(|c| {
            let snippet = regex::Regex::new(r#"<p[^>]*>([\s\S]*?)</p>"#)
                .expect("静态正则")
                .captures(&c[3])
                .map(|p| html_to_text(&p[1]))
                .unwrap_or_default();
            SearchResult {
                url: decode_entities(&c[1]),
                title: html_to_text(&c[2]),
                snippet,
            }
        })
        .collect()
}

/// DDG 重定向（//duckduckgo.com/l/?uddg=<encoded>&rut=...）还原真实地址。
pub fn decode_ddg_url(url: &str) -> String {
    if let Some(pos) = url.find("uddg=") {
        let rest = &url[pos + 5..];
        let end = rest.find('&').unwrap_or(rest.len());
        let decoded = urldecode(&rest[..end]);
        if !decoded.is_empty() {
            return decoded;
        }
    }
    url.trim_start_matches("//").to_string()
}

pub fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// 宽松 percent-decode：失败字节原样保留（永不报错）。
pub fn urldecode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = vec![];
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn all_web_tools() -> Vec<std::sync::Arc<dyn Tool>> {
    vec![
        std::sync::Arc::new(WebFetch),
        std::sync::Arc::new(WebSearch),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_to_text_strips_tags_and_entities() {
        let t = html_to_text(
            "<html><script>var x=1;</script><body><h1>Hi</h1><p>a&amp;b</p></body></html>",
        );
        assert!(t.contains("Hi"));
        assert!(t.contains("a&b"));
        assert!(!t.contains("var x"));
        assert!(!t.contains('<'));
    }

    #[test]
    fn strip_blocks_handles_multiple_and_unclosed() {
        let s = strip_blocks("<p>a</p><script>bad1</script><script>bad2</script><p>b</p>");
        assert!(!s.contains("bad"));
        assert!(s.contains('a') && s.contains('b'));
        // 未闭合 script：整块找不到 end → 原样保留（安全降级）
        let s2 = strip_blocks("<script>unclosed");
        assert!(!s2.is_empty());
    }

    #[test]
    fn ddg_url_decode() {
        assert_eq!(
            decode_ddg_url("//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fa&rut=abc"),
            "https://example.com/a"
        );
        assert_eq!(
            decode_ddg_url("https://direct.example.com"),
            "https://direct.example.com"
        );
    }

    #[test]
    fn parse_ddg_extracts_triples() {
        let html = r##"
        <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fa.io%2Fx&amp;rut=1">Title <b>A</b></a>
        <a class="result__snippet" href="#">first &amp; only</a>
        <a class="result__a" href="https://b.io">Title B</a>
        <a class="result__snippet" href="#">second</a>
        "##;
        let rs = parse_ddg(html, 5);
        assert_eq!(rs.len(), 2);
        assert_eq!(rs[0].url, "https://a.io/x");
        assert_eq!(rs[0].title, "Title A");
        assert_eq!(rs[0].snippet, "first & only");
        assert_eq!(rs[1].url, "https://b.io");
    }

    #[test]
    fn parse_bing_extracts_results() {
        let html = r#"<li class="b_algo"><h2><a href="https://x.io">X 站</a></h2><div><p>摘要一</p></div></li>
        <li class="b_algo"><h2><a href="https://y.io">Y 站</a></h2><div><p>摘要二</p></div></li>"#;
        let rs = parse_bing(html, 5);
        assert_eq!(rs.len(), 2);
        assert_eq!(rs[0].url, "https://x.io");
        assert_eq!(rs[0].title, "X 站");
        assert_eq!(rs[0].snippet, "摘要一");
    }

    #[test]
    fn urlencode_and_back() {
        assert_eq!(urlencode("a b&c"), "a%20b%26c");
        assert_eq!(urldecode("a%20b%26c"), "a b&c");
        assert_eq!(urldecode("a+b"), "a b");
        assert_eq!(urldecode("%ZZ"), "%ZZ"); // 非法序列原样保留
        assert_eq!(urldecode("%e4%bd%a0"), "你");
    }
}
