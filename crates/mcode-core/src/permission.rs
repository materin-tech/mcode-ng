//! 权限引擎——模式 + 规则表 + 会话记忆的单一裁决入口。
//!
//! 对齐 Claude Code 的三张表（deny → ask → allow）并修复旧版三个缺陷：
//! 1. **桶优先级**：deny 永远先于 ask/allow 胜出（旧版"首条命中即返回"，
//!    用户先加的 allow 规则会遮蔽后加的 deny——危险）；
//! 2. **结构化 bash 匹配**：`bash(rm *)` 用 shlex 分词做 token 级前缀匹配，
//!    旧版子串 `contains` 会让 `bash(echo "not rm")` 误命中 `rm *`；
//! 3. **always-allow 可持久化**：用户选"总是允许"时沉淀为显式 allow 规则
//!    写回 desktop.json，跨会话生效（旧版只在会话内存活）。
//!
//! 模式语义（serde 字符串与旧版逐字兼容）：
//! - `passon` 全自动（默认）/ `manual` 每次确认 / `auto` 规则内放行 /
//!   `plan` 只读（计划模式）。

pub use crate::config::PermissionRule;

use serde::{Deserialize, Serialize};

/// 权限模式。shift+tab 循环序：passon → manual → auto → plan。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Manual,
    Auto,
    Plan,
    /// 全自动放行（默认）
    #[default]
    Passon,
}

impl Mode {
    pub const ALL: [Mode; 4] = [Mode::Passon, Mode::Manual, Mode::Auto, Mode::Plan];

    pub fn next(self) -> Self {
        let i = Self::ALL.iter().position(|m| *m == self).unwrap_or(0);
        Self::ALL[(i + 1) % Self::ALL.len()]
    }

    pub fn label(&self) -> &'static str {
        match self {
            Mode::Manual => "manual",
            Mode::Auto => "auto",
            Mode::Plan => "plan",
            Mode::Passon => "passon",
        }
    }

    pub fn hint(&self) -> &'static str {
        match self {
            Mode::Manual => "每个非只读工具执行前确认",
            Mode::Auto => "规则内自动放行，其余确认",
            Mode::Plan => "计划模式：仅只读工具",
            Mode::Passon => "全自动放行（默认）",
        }
    }
}

/// 规则行为。desktop.json 旧条目只有 `allow: bool`；新条目可写 `behavior`，
/// 出现时优先于 legacy 字段。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Behavior {
    Allow,
    Deny,
    Ask,
}

/// 一次权限检查的输入切片。bash 类工具填 `command`，文件类工具填 `target`。
#[derive(Debug, Clone, Default)]
pub struct PermCheck<'a> {
    pub tool: &'a str,
    pub is_read_only: bool,
    /// bash 命令原文（工具为 bash 时）
    pub command: Option<&'a str>,
    /// 操作目标路径/资源描述（write/edit/glob 等）
    pub target: Option<&'a str>,
}

/// 裁决结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    Confirm,
    Deny { reason: String },
}

/// 用户对确认框的答复（TUI 权限对话框回收）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// 本条放行
    Allow,
    /// 放行并沉淀规则（持久化时由调用方追加 PermissionRule 并保存）
    AllowAlways,
    Deny,
}

pub struct PermissionManager {
    pub mode: Mode,
    /// 会话内「总是允许」：tool → true。与持久化规则互补：会话内立即可用，
    /// 持久化异步落盘。
    always_allow: std::collections::HashSet<String>,
    rules: Vec<PermissionRule>,
}

impl PermissionManager {
    pub fn new(mode: Mode, rules: Vec<PermissionRule>) -> Self {
        Self {
            mode,
            always_allow: Default::default(),
            rules,
        }
    }

    pub fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
    }

    pub fn cycle_mode(&mut self) -> Mode {
        self.mode = self.mode.next();
        self.mode
    }

    /// 规则表只读视图（/permissions 面板数据源）。
    pub fn rules(&self) -> &[PermissionRule] {
        &self.rules
    }

    /// 追加规则（"总是允许"持久化路径）。返回是否新增（去重）。
    pub fn add_rule(&mut self, rule: PermissionRule) -> bool {
        if self.rules.iter().any(|r| {
            r.tool == rule.tool
                && r.pattern == rule.pattern
                && r.behavior_or_legacy() == rule.behavior_or_legacy()
        }) {
            return false;
        }
        self.rules.push(rule);
        true
    }

    /// 单一裁决入口。顺序：deny 规则 → 只读 → plan → passon → ask 规则 →
    /// allow 规则/会话记忆 → 默认确认。
    pub fn evaluate(&self, check: &PermCheck) -> Verdict {
        // 1) deny 规则：最高优先级，任何模式下都生效
        for r in self
            .rules
            .iter()
            .filter(|r| r.behavior_or_legacy() == Behavior::Deny)
        {
            if rule_matches(r, check) {
                return Verdict::Deny {
                    reason: format!("规则拒绝: {}({})", r.tool, r.pattern),
                };
            }
        }
        // 2) 只读工具直接放行（plan 模式也靠这条存活）
        if check.is_read_only {
            return Verdict::Allow;
        }
        // 3) 模式硬约束
        if self.mode == Mode::Plan {
            return Verdict::Deny {
                reason: "plan 模式：只允许只读工具".into(),
            };
        }
        if self.mode == Mode::Passon {
            return Verdict::Allow;
        }
        // 4) ask 规则：显式要求确认，优先于 allow 与会话记忆
        for r in self
            .rules
            .iter()
            .filter(|r| r.behavior_or_legacy() == Behavior::Ask)
        {
            if rule_matches(r, check) {
                return Verdict::Confirm;
            }
        }
        // 5) allow 规则 / 会话内 always-allow
        if self.always_allow.contains(check.tool)
            || self
                .rules
                .iter()
                .any(|r| r.behavior_or_legacy() == Behavior::Allow && rule_matches(r, check))
        {
            return Verdict::Allow;
        }
        // 6) 默认确认
        Verdict::Confirm
    }

    pub fn record(&mut self, tool: &str, d: &Decision) {
        if *d == Decision::AllowAlways {
            self.always_allow.insert(tool.to_string());
        }
    }
}

/// 规则匹配：工具名（大小写不敏感）+ 模式。
/// - `*` 或空：匹配该工具全部调用；
/// - `bash(前缀 *)`：token 级前缀匹配（shlex 分词）；
/// - `tool(精确)`：bash token 序列全等 / 其他工具 glob 匹配 target；
/// - `tool(glob)`（非 bash）：globset 匹配 target。
pub fn rule_matches(rule: &PermissionRule, check: &PermCheck) -> bool {
    if !rule.tool.eq_ignore_ascii_case(check.tool) {
        return false;
    }
    let pattern = rule.pattern.trim();
    if pattern.is_empty() || pattern == "*" {
        return true;
    }
    // `tool(inner)` 形态
    if let Some((rule_tool, inner)) = split_parens(pattern) {
        if !rule_tool.eq_ignore_ascii_case(check.tool) {
            return false;
        }
        return match inner_inner(inner) {
            PatternInner::All => true,
            PatternInner::Prefix(tokens) => match check.command {
                Some(cmd) => command_prefix_matches(cmd, &tokens),
                None => false,
            },
            PatternInner::Glob(g) => match check.target {
                Some(t) => glob_match(&g, t),
                None => false,
            },
        };
    }
    // 无括号：有 target → glob 匹配；有 command → 剥掉尾部通配符后做 token 前缀
    // （`git push*` / `rm *` / `git push:*` 的 `*`/`:*` 是通配标记，不是字面 token）
    if let Some(t) = check.target {
        return glob_match(pattern, t);
    }
    if let Some(cmd) = check.command {
        let base = pattern
            .trim_end_matches('*')
            .trim_end_matches(':')
            .trim_end();
        let toks: Vec<String> = base.split_whitespace().map(String::from).collect();
        return command_prefix_matches(cmd, &toks);
    }
    false
}

enum PatternInner {
    All,
    /// token 序列（末位 `*` 表示通配后缀）
    Prefix(Vec<String>),
    Glob(String),
}

fn inner_inner(inner: &str) -> PatternInner {
    let inner = inner.trim();
    if inner.is_empty() || inner == "*" {
        return PatternInner::All;
    }
    // `前缀 *` / `前缀:*` → token 前缀
    let globby = inner.ends_with(" *") || inner.ends_with(":*");
    let base = inner
        .trim_end_matches('*')
        .trim_end_matches(':')
        .trim_end()
        .to_string();
    if globby && !base.is_empty() {
        return PatternInner::Prefix(base.split_whitespace().map(String::from).collect());
    }
    if inner.contains('*') || inner.contains('?') || inner.contains('[') {
        return PatternInner::Glob(inner.to_string());
    }
    if inner.split_whitespace().count() > 1 {
        return PatternInner::Prefix(inner.split_whitespace().map(String::from).collect());
    }
    PatternInner::Glob(inner.to_string())
}

/// 拆 `tool(inner)`；无括号返回 None。
fn split_parens(pattern: &str) -> Option<(String, &str)> {
    let open = pattern.find('(')?;
    let close = pattern.rfind(')')?;
    if close < open {
        return None;
    }
    Some((pattern[..open].to_string(), &pattern[open + 1..close]))
}

/// shlex 分词后的 token 前缀匹配：rule tokens 必须是 command tokens 的前缀。
/// 无 shlex 可分词时退化为空白分词（Windows 无 shlex 语义的命令仍可用）。
fn command_prefix_matches(command: &str, prefix_tokens: &[String]) -> bool {
    if prefix_tokens.is_empty() {
        return true;
    }
    let tokens: Vec<String> = shlex::split(command)
        .unwrap_or_else(|| command.split_whitespace().map(String::from).collect());
    if tokens.len() < prefix_tokens.len() {
        return false;
    }
    tokens
        .iter()
        .zip(prefix_tokens.iter())
        .all(|(c, p)| c.eq_ignore_ascii_case(p))
}

fn glob_match(pattern: &str, target: &str) -> bool {
    globset::GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .ok()
        .and_then(|g| globset::GlobSetBuilder::new().add(g).build().ok())
        .map(|set| set.is_match(target))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(tool: &str, pattern: &str, b: Behavior) -> PermissionRule {
        PermissionRule {
            tool: tool.into(),
            pattern: pattern.into(),
            allow: b == Behavior::Allow,
            behavior: Some(b),
        }
    }

    fn bash(cmd: &'_ str) -> PermCheck<'_> {
        PermCheck {
            tool: "bash",
            is_read_only: false,
            command: Some(cmd),
            target: None,
        }
    }

    #[test]
    fn mode_cycle_matches_shift_tab_order() {
        assert_eq!(Mode::Passon.next(), Mode::Manual);
        assert_eq!(Mode::Manual.next(), Mode::Auto);
        assert_eq!(Mode::Auto.next(), Mode::Plan);
        assert_eq!(Mode::Plan.next(), Mode::Passon);
    }

    #[test]
    fn deny_beats_allow_regardless_of_order() {
        let pm = PermissionManager::new(
            Mode::Auto,
            vec![
                rule("bash", "git *", Behavior::Allow),
                rule("bash", "git push*", Behavior::Deny),
            ],
        );
        assert_eq!(pm.evaluate(&bash("git status")), Verdict::Allow);
        assert!(matches!(
            pm.evaluate(&bash("git push origin main")),
            Verdict::Deny { .. }
        ));
    }

    #[test]
    fn deny_applies_even_in_passon() {
        let pm = PermissionManager::new(Mode::Passon, vec![rule("bash", "rm *", Behavior::Deny)]);
        assert!(matches!(
            pm.evaluate(&bash("rm -rf /")),
            Verdict::Deny { .. }
        ));
        assert_eq!(pm.evaluate(&bash("ls")), Verdict::Allow);
    }

    #[test]
    fn bash_prefix_matching_is_token_level_not_substring() {
        let pm = PermissionManager::new(Mode::Auto, vec![rule("bash", "rm *", Behavior::Deny)]);
        // 子串匹配会误伤：命令里含 "rm" 但首 token 不是 rm
        assert_eq!(pm.evaluate(&bash("echo \"not rm here\"")), Verdict::Confirm);
        // 引号内空格不拆词：shlex 分词后首 token 是 echo
        assert_eq!(pm.evaluate(&bash("echo hello")), Verdict::Confirm);
        // 真命中
        assert!(matches!(
            pm.evaluate(&bash("rm file.txt")),
            Verdict::Deny { .. }
        ));
    }

    #[test]
    fn prefix_glob_star_and_colon_star() {
        let pm =
            PermissionManager::new(Mode::Auto, vec![rule("bash", "git push:*", Behavior::Deny)]);
        assert!(matches!(
            pm.evaluate(&bash("git push origin main")),
            Verdict::Deny { .. }
        ));
        // CC 语义：`前缀:*` 命中裸前缀本身（npm run test:* 匹配 `npm run test`）
        assert!(matches!(
            pm.evaluate(&bash("git push")),
            Verdict::Deny { .. }
        ));
    }

    #[test]
    fn ask_rule_outranks_allow_and_always_allow() {
        let mut pm = PermissionManager::new(
            Mode::Auto,
            vec![
                rule("bash", "*", Behavior::Allow),
                rule("bash", "npm publish*", Behavior::Ask),
            ],
        );
        pm.record("bash", &Decision::AllowAlways);
        assert_eq!(pm.evaluate(&bash("npm publish")), Verdict::Confirm);
        assert_eq!(pm.evaluate(&bash("ls")), Verdict::Allow);
    }

    #[test]
    fn mode_semantics_and_read_only() {
        let mut pm = PermissionManager::new(Mode::Passon, vec![]);
        assert_eq!(pm.evaluate(&bash("rm")), Verdict::Allow);

        pm.set_mode(Mode::Manual);
        assert_eq!(
            pm.evaluate(&PermCheck {
                tool: "grep",
                is_read_only: true,
                ..Default::default()
            }),
            Verdict::Allow
        );
        assert_eq!(pm.evaluate(&bash("ls")), Verdict::Confirm);

        pm.set_mode(Mode::Plan);
        assert!(matches!(
            pm.evaluate(&PermCheck {
                tool: "write_file",
                is_read_only: false,
                ..Default::default()
            }),
            Verdict::Deny { .. }
        ));
    }

    #[test]
    fn file_glob_rules_match_target() {
        let pm = PermissionManager::new(
            Mode::Auto,
            vec![rule("write_file", "secrets/**", Behavior::Deny)],
        );
        let check = PermCheck {
            tool: "write_file",
            is_read_only: false,
            command: None,
            target: Some("secrets/key.pem"),
        };
        assert!(matches!(pm.evaluate(&check), Verdict::Deny { .. }));
        let ok = PermCheck {
            tool: "write_file",
            is_read_only: false,
            command: None,
            target: Some("src/main.rs"),
        };
        assert_eq!(pm.evaluate(&ok), Verdict::Confirm);
    }

    #[test]
    fn add_rule_dedupes() {
        let mut pm = PermissionManager::new(Mode::Manual, vec![]);
        assert!(pm.add_rule(rule("bash", "cargo test*", Behavior::Allow)));
        assert!(!pm.add_rule(rule("bash", "cargo test*", Behavior::Allow)));
        assert_eq!(pm.rules().len(), 1);
    }

    #[test]
    fn legacy_allow_field_maps_to_behavior() {
        let legacy = PermissionRule {
            tool: "bash".into(),
            pattern: "cargo build*".into(),
            allow: true,
            behavior: None,
        };
        assert_eq!(legacy.behavior_or_legacy(), Behavior::Allow);
        let legacy_deny = PermissionRule {
            tool: "bash".into(),
            pattern: "rm *".into(),
            allow: false,
            behavior: None,
        };
        assert_eq!(legacy_deny.behavior_or_legacy(), Behavior::Deny);
    }
}
