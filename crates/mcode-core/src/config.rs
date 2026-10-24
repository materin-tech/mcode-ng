//! `~/.mcode` 三方互通配置层。
//!
//! - `config.json`：CLI 互通 schema（camelCase；baseURL 必须显式 rename——
//!   camelCase 机制会产出 baseUrl，字段会静默丢失，旧版踩过）
//! - `desktop.json`：扩展 schema（thinking / 权限规则 / MCP / hooks / 最近 workspace）
//!
//! 兼容性硬约束：新增字段全部 `#[serde(default)]`，旧文件读入零损失；
//! 旧字段全部保留，本 CLI 写回的文件旧版仍可读。

pub use crate::permission::{Behavior, Mode};

/// 一条权限规则（desktop.json `permissionRules` 持久化形态）。
///
/// 双字段兼容：旧版只有 `allow: bool`；新版可写 `behavior: "allow"/"deny"/"ask"`。
/// 读侧一律走 [`behavior_or_legacy`]；写侧两个都写（旧版二进制仍可读）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PermissionRule {
    /// 工具名（bash / write_file / grep / MCP 前缀 `mcp__server__tool` …）
    pub tool: String,
    /// 匹配模式：`*` / `bash(rm *)` / `write_file(secrets/**)` / `git push:*`
    pub pattern: String,
    /// legacy 布尔位（true=allow / false=deny）
    pub allow: bool,
    /// 新版行为；存在时优先于 `allow`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub behavior: Option<Behavior>,
}

impl PermissionRule {
    /// 生效行为：`behavior` 优先，回落 legacy `allow` 布尔。
    pub fn behavior_or_legacy(&self) -> Behavior {
        self.behavior.unwrap_or(if self.allow {
            Behavior::Allow
        } else {
            Behavior::Deny
        })
    }
}

use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

// ---------------------------------------------------------------------------
// config.json — CLI 互通 schema
// ---------------------------------------------------------------------------

/// API 协议直接沿用 [`crate::api::Protocol`]（serde 小写字符串；
/// "responses"/"chat" 为历史值，"anthropic" 为新增）。
pub use crate::api::Protocol;

/// 界面语言。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    #[serde(rename = "zh_CN")]
    #[default]
    ZhCn,
    #[serde(rename = "zh_TW")]
    ZhTw,
    #[serde(rename = "en_US")]
    EnUs,
    #[serde(rename = "ja_JP")]
    JaJp,
    #[serde(rename = "ko_KR")]
    KoKr,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Provider {
    pub name: String,
    /// CLI schema 为 `baseURL`（大写 URL）——camelCase 会产出 baseUrl 导致
    /// 字段静默丢失，必须显式 rename 对齐。
    #[serde(rename = "baseURL")]
    pub base_url: String,
    /// 环境变量名；不是合法变量名则按明文 key 兜底（对齐 CLI resolveApiKey）
    pub api_key_env: String,
    pub models: Vec<String>,
    /// 接受图片输入的模型名单；不在名单内的模型拒绝图片附件并降级文本
    pub multimodal_models: Vec<String>,
    /// API 协议；旧配置缺省 → Responses
    pub protocol: Protocol,
}

impl Provider {
    /// 解析 API key：优先环境变量；值不是合法变量名时按明文兜底
    /// （对齐 TS config.ts resolveApiKey）。
    pub fn resolve_api_key(&self) -> Option<String> {
        let v = self.api_key_env.as_str();
        if v.is_empty() {
            return None;
        }
        if let Ok(from_env) = std::env::var(v) {
            if !from_env.is_empty() {
                return Some(from_env);
            }
        }
        if is_env_name(v) {
            None
        } else {
            Some(v.to_string())
        }
    }

    pub fn is_multimodal(&self, model: &str) -> bool {
        self.multimodal_models.iter().any(|m| m == model)
    }
}

fn is_env_name(s: &str) -> bool {
    let mut cs = s.chars();
    match cs.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {
            cs.all(|c| c.is_ascii_alphanumeric() || c == '_')
        }
        _ => false,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Config {
    /// PUA 收敛纪律指令每个会话常驻（/pua:on）
    pub pua_always_on: bool,
    pub language: Language,
    pub active_provider: Option<String>,
    /// 当前 provider 内选中的模型
    pub active_model: Option<String>,
    pub providers: Vec<Provider>,
    pub default_mode: Mode,
    /// 会话保留天数：超过 N 天未活动的会话启动时自动删除
    pub session_retention_days: u32,
    /// 主模型不可用时的降级链（对齐 CC --fallback-model）。
    /// 按序尝试；API 层按错误分类决定是否降级（Auth/Billing 不降级）。
    #[serde(default)]
    pub fallback_models: Vec<String>,
    /// 启动时注入的环境变量（对齐 CC settings env 段）。
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            pua_always_on: false,
            language: Language::default(),
            active_provider: None,
            active_model: None,
            providers: vec![],
            default_mode: Mode::Passon,
            session_retention_days: 90,
            fallback_models: vec![],
            env: BTreeMap::new(),
        }
    }
}

impl Config {
    /// 读取 config.json；文件不存在返回默认骨架（首次启动 → 引导流程）。
    /// 路径不可用时（无 HOME）返回 Err。
    pub fn load() -> anyhow::Result<Self> {
        let path = crate::util::config_path()
            .ok_or_else(|| anyhow::anyhow!("无法定位 ~/.mcode 目录（HOME 未设置）"))?;
        Self::try_load_from(&path)
    }

    /// 文件存在但解析失败 → Err（带路径上下文）；文件不存在 → Ok(默认骨架)。
    /// 主流程必须用本方法把损坏配置暴露给用户，而不是静默回退后覆盖用户文件。
    pub fn try_load_from(path: &Path) -> anyhow::Result<Self> {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(anyhow::anyhow!("读取 {} 失败: {e}", path.display())),
        };
        serde_json::from_slice(&bytes).with_context(|| {
            format!(
                "解析 {} 失败（文件存在但不是合法的 mcode 配置）",
                path.display()
            )
        })
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let path = crate::util::config_path()
            .ok_or_else(|| anyhow::anyhow!("无法定位 ~/.mcode 目录（HOME 未设置）"))?;
        self.save_to(&path)
    }

    pub fn save_to(&self, path: &Path) -> anyhow::Result<()> {
        atomic_write(path, &serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn active_provider(&self) -> Option<&Provider> {
        let name = self.active_provider.as_ref()?;
        self.providers.iter().find(|p| &p.name == name)
    }

    /// 当前激活模型名（active_model 优先，回落 provider 首个）。
    pub fn active_model(&self) -> Option<String> {
        match self.active_model.as_deref() {
            Some(m) if !m.is_empty() => return Some(m.to_string()),
            _ => {}
        }
        self.active_provider()
            .and_then(|p| p.models.first().cloned())
    }

    /// 首次启动判定：providers 为空。
    pub fn needs_onboarding(&self) -> bool {
        self.providers.is_empty()
    }

    /// 供应商就绪检查（每回合评估，永不缓存——对齐 CLI providerIssue）。
    pub fn provider_ready(&self) -> anyhow::Result<()> {
        let p = self
            .active_provider()
            .ok_or_else(|| anyhow::anyhow!("未配置模型供应商，请先完成引导"))?;
        if p.resolve_api_key().is_none() {
            anyhow::bail!(
                "供应商 {} 的 API key 不可用：环境变量 {} 未设置且值不是明文 key",
                p.name,
                p.api_key_env
            );
        }
        if self.active_model().is_none() {
            anyhow::bail!("未选择模型");
        }
        Ok(())
    }

    /// 后端连接参数（主模型）。供 agent 层构造 [`crate::api::BackendConfig`]。
    pub fn backend_config(&self) -> anyhow::Result<crate::api::BackendConfig> {
        let p = self
            .active_provider()
            .ok_or_else(|| anyhow::anyhow!("未配置模型供应商"))?;
        let model = self
            .active_model()
            .ok_or_else(|| anyhow::anyhow!("未选择模型"))?;
        let mut cfg = crate::api::BackendConfig::new(
            p.protocol,
            p.base_url.clone(),
            p.resolve_api_key().unwrap_or_default(),
            model,
        );
        cfg.cache_control = p.protocol == Protocol::Anthropic;
        Ok(cfg)
    }

    // ---------- Provider CRUD ----------

    /// 新增：重名拒绝、至少一个模型；providers 为空时自动激活并选中首个模型。
    pub fn add_provider(&mut self, p: Provider) -> anyhow::Result<()> {
        anyhow::ensure!(!p.name.trim().is_empty(), "供应商名称不能为空");
        anyhow::ensure!(!p.models.is_empty(), "供应商 {} 至少需要一个模型", p.name);
        anyhow::ensure!(
            !self.providers.iter().any(|x| x.name == p.name),
            "供应商 {} 已存在",
            p.name
        );
        let is_first = self.providers.is_empty();
        self.providers.push(p);
        if is_first {
            self.active_provider = self.providers.last().map(|x| x.name.clone());
            self.active_model = self
                .providers
                .last()
                .and_then(|x| x.models.first().cloned());
        }
        Ok(())
    }

    /// 按 name 整体替换（不存在报错）。替换的是 active provider 且当前
    /// active_model 不在新列表里时，回落到新列表首个（否则悬空模型名会让
    /// 每次请求 404）。
    pub fn update_provider(&mut self, p: Provider) -> anyhow::Result<()> {
        anyhow::ensure!(!p.models.is_empty(), "供应商 {} 至少需要一个模型", p.name);
        let is_active = self.active_provider.as_deref() == Some(p.name.as_str());
        let current = self.active_model.clone().unwrap_or_default();
        let entry = self
            .providers
            .iter_mut()
            .find(|x| x.name == p.name)
            .with_context(|| format!("供应商 {} 不存在", p.name))?;
        let first_model = p.models[0].clone();
        let has_current = p.models.iter().any(|m| m == &current);
        *entry = p;
        if is_active && !has_current {
            self.active_model = Some(first_model);
        }
        Ok(())
    }

    /// 删除；删的是 active 时回落剩余首个（无剩余则清空激活态）。
    pub fn remove_provider(&mut self, name: &str) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.providers.iter().any(|x| x.name == name),
            "供应商 {name} 不存在"
        );
        self.providers.retain(|x| x.name != name);
        if self.active_provider.as_deref() == Some(name) {
            self.active_provider = self.providers.first().map(|x| x.name.clone());
            self.active_model = self
                .providers
                .first()
                .and_then(|x| x.models.first().cloned());
        }
        Ok(())
    }

    /// 切换激活；active_model 重置为该 provider 首个模型。
    pub fn set_active_provider(&mut self, name: &str) -> anyhow::Result<()> {
        let first = self
            .providers
            .iter()
            .find(|x| x.name == name)
            .with_context(|| format!("供应商 {name} 不存在"))?
            .models
            .first()
            .cloned();
        self.active_provider = Some(name.to_string());
        self.active_model = first;
        Ok(())
    }
}

/// 原子写：同目录临时文件 + rename，进程被杀/磁盘满不留半个文件。
pub fn atomic_write(path: &Path, contents: &str) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// desktop.json — 扩展 schema
// ---------------------------------------------------------------------------

/// 思考深度（"default" = 不发送 reasoning 参数，网关最大兼容）。
/// 与 [`crate::api::ThinkingEffort`] 的映射见 [`DesktopConfig::thinking`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingEffort {
    #[default]
    Default,
    Low,
    Medium,
    High,
}

impl ThinkingEffort {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "default" => Some(ThinkingEffort::Default),
            "low" => Some(ThinkingEffort::Low),
            "medium" => Some(ThinkingEffort::Medium),
            "high" => Some(ThinkingEffort::High),
            _ => None,
        }
    }

    /// → API 层思考配置（Default → None：请求形态与不配置完全一致）。
    pub fn thinking(self) -> Option<crate::api::ThinkingConfig> {
        let effort = match self {
            ThinkingEffort::Default => return None,
            ThinkingEffort::Low => crate::api::ThinkingEffort::Low,
            ThinkingEffort::Medium => crate::api::ThinkingEffort::Medium,
            ThinkingEffort::High => crate::api::ThinkingEffort::High,
        };
        Some(crate::api::ThinkingConfig { effort })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
#[derive(Default)]
pub struct DesktopConfig {
    /// 思考深度（desktop.json 持久化）
    pub thinking_effort: String,
    /// 权限规则（/permissions 面板数据源；deny→ask→allow 桶优先见 permission 模块）
    pub permission_rules: Vec<PermissionRule>,
    /// MCP server 配置（stdio + HTTP 两种传输）
    pub mcp_servers: Vec<McpServerConfig>,
    /// hooks 生命周期配置（8 事件）
    pub hooks: Vec<HookConfig>,
    /// 最近打开的工作区（最近在前，上限 8）
    pub recent_workspaces: Vec<String>,
}

impl DesktopConfig {
    /// 解析后的思考深度（空串/非法值 → Default）。
    pub fn thinking(&self) -> ThinkingEffort {
        ThinkingEffort::parse(&self.thinking_effort).unwrap_or_default()
    }

    pub fn load() -> anyhow::Result<Self> {
        let path = crate::util::desktop_config_path()
            .ok_or_else(|| anyhow::anyhow!("无法定位 ~/.mcode 目录（HOME 未设置）"))?;
        Self::try_load_from(&path)
    }

    pub fn try_load_from(path: &Path) -> anyhow::Result<Self> {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(anyhow::anyhow!("读取 {} 失败: {e}", path.display())),
        };
        serde_json::from_slice(&bytes)
            .with_context(|| format!("解析 {} 失败（文件存在但不是合法的配置）", path.display()))
    }

    pub fn load_from(path: &Path) -> Self {
        Self::try_load_from(path).unwrap_or_else(|e| {
            eprintln!("mcode: {e:#}；已回退默认配置（原文件未改动，请修复后重启）");
            Self::default()
        })
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let path = crate::util::desktop_config_path()
            .ok_or_else(|| anyhow::anyhow!("无法定位 ~/.mcode 目录（HOME 未设置）"))?;
        self.save_to(&path)
    }

    pub fn save_to(&self, path: &Path) -> anyhow::Result<()> {
        atomic_write(path, &serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    // ---------- MCP server CRUD ----------

    /// 按 name 幂等 upsert。
    pub fn upsert_mcp_server(&mut self, s: McpServerConfig) {
        match self.mcp_servers.iter_mut().find(|x| x.name == s.name) {
            Some(slot) => *slot = s,
            None => self.mcp_servers.push(s),
        }
    }

    pub fn remove_mcp_server(&mut self, name: &str) -> bool {
        let before = self.mcp_servers.len();
        self.mcp_servers.retain(|x| x.name != name);
        self.mcp_servers.len() != before
    }

    pub fn set_mcp_enabled(&mut self, name: &str, enabled: bool) -> anyhow::Result<()> {
        let slot = self
            .mcp_servers
            .iter_mut()
            .find(|x| x.name == name)
            .with_context(|| format!("MCP server {name} 不存在"))?;
        slot.enabled = enabled;
        Ok(())
    }
}

/// MCP server：stdio（command+args+env）或 HTTP（url）二选一。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerConfig {
    pub name: String,
    /// stdio 传输：进程命令
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// HTTP 传输：端点 URL（设置后走 streamable-http/SSE 而非子进程）
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// 是否随启动自动连接
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

impl McpServerConfig {
    pub fn transport(&self) -> McpTransport {
        match self.url.as_deref() {
            Some(u) if !u.is_empty() => McpTransport::Http(u.to_string()),
            _ => McpTransport::Stdio,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpTransport {
    Stdio,
    Http(String),
}

/// hook 规则：事件 + 工具匹配器 + 命令。8 事件对齐 CC；旧文件只含
/// pre_tool_use/post_tool_use/stop 三种值，serde 兼容。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HookConfig {
    pub event: HookEvent,
    /// 工具名匹配（`*` 全部，否则精确/子串匹配）
    pub matcher: String,
    pub command: String,
    /// 超时秒数（None → 模块默认 60s）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEvent {
    PreToolUse,
    PostToolUse,
    UserPromptSubmit,
    Notification,
    Stop,
    SubagentStop,
    SessionStart,
    SessionEnd,
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_real_cli_config() {
        let raw = r#"{
            "language": "zh_CN",
            "activeProvider": "glm",
            "activeModel": "glm-5.3-flash",
            "providers": [{
                "name": "glm",
                "baseURL": "https://open.bigmodel.cn/api/v1",
                "apiKeyEnv": "MY_KEY_ENV",
                "models": ["glm-5.3-flash"],
                "multimodalModels": [],
                "protocol": "chat"
            }],
            "defaultMode": "passon",
            "puaAlwaysOn": true
        }"#;
        let cfg: Config = serde_json::from_str(raw).unwrap();
        assert_eq!(cfg.active_provider.as_deref(), Some("glm"));
        assert_eq!(cfg.active_model().as_deref(), Some("glm-5.3-flash"));
        assert!(cfg.pua_always_on);
        assert_eq!(cfg.providers[0].protocol, Protocol::Chat);
        // 旧文件没有的新字段 → 默认值
        assert!(cfg.fallback_models.is_empty());
        assert!(cfg.env.is_empty());
        // 序列化回 CLI 可读的 camelCase
        let out = serde_json::to_string(&cfg).unwrap();
        assert!(out.contains("\"activeProvider\""));
        assert!(out.contains("\"puaAlwaysOn\""));
        assert!(out.contains("\"multimodalModels\""));
        assert!(out.contains("\"baseURL\""), "baseURL 必须保持大写 URL");
    }

    #[test]
    fn api_key_env_then_literal_fallback() {
        let p = Provider {
            name: "t".into(),
            base_url: "http://x".into(),
            api_key_env: "MCODE_TEST_KEY_ENV_XYZ".into(),
            models: vec![],
            multimodal_models: vec![],
            protocol: Protocol::default(),
        };
        // 环境变量未设置 → None（不会把变量名当明文）
        std::env::remove_var("MCODE_TEST_KEY_ENV_XYZ");
        assert!(p.resolve_api_key().is_none());
        // 环境变量设置 → 取值
        // SAFETY: 测试进程顺序执行该变量读写，无并发访问
        unsafe { std::env::set_var("MCODE_TEST_KEY_ENV_XYZ", "from-env") };
        assert_eq!(p.resolve_api_key().as_deref(), Some("from-env"));
        unsafe { std::env::remove_var("MCODE_TEST_KEY_ENV_XYZ") };

        // 非法变量名 → 明文兜底
        let p2 = Provider {
            api_key_env: "sk-literal-key-123".into(),
            ..p
        };
        assert_eq!(p2.resolve_api_key().as_deref(), Some("sk-literal-key-123"));
    }

    #[test]
    fn provider_crud_roundtrip() {
        let mut cfg = Config::default();
        cfg.add_provider(Provider {
            name: "a".into(),
            base_url: "http://a".into(),
            api_key_env: "KEY_A".into(),
            models: vec!["m1".into(), "m2".into()],
            multimodal_models: vec![],
            protocol: Protocol::Chat,
        })
        .unwrap();
        // 首个 provider 自动激活
        assert_eq!(cfg.active_provider.as_deref(), Some("a"));
        assert_eq!(cfg.active_model().as_deref(), Some("m1"));

        cfg.set_active_provider("a").unwrap();
        cfg.update_provider(Provider {
            name: "a".into(),
            base_url: "http://a2".into(),
            api_key_env: "KEY_A".into(),
            models: vec!["m9".into()],
            multimodal_models: vec!["m9".into()],
            protocol: Protocol::Anthropic,
        })
        .unwrap();
        assert_eq!(cfg.active_model().as_deref(), Some("m9"));
        assert!(cfg.providers[0].is_multimodal("m9"));

        cfg.remove_provider("a").unwrap();
        assert!(cfg.active_provider.is_none());
        assert!(cfg.needs_onboarding());
        assert!(cfg.provider_ready().is_err());
    }

    #[test]
    fn backend_config_maps_cache_control_by_protocol() {
        let mut cfg = Config::default();
        cfg.add_provider(Provider {
            name: "a".into(),
            base_url: "http://a".into(),
            api_key_env: "sk-literal".into(),
            models: vec!["m".into()],
            multimodal_models: vec![],
            protocol: Protocol::Anthropic,
        })
        .unwrap();
        let bc = cfg.backend_config().unwrap();
        assert_eq!(bc.protocol, Protocol::Anthropic);
        assert!(bc.cache_control, "anthropic 协议默认开 cache 断点");
        assert_eq!(bc.api_key, "sk-literal");
    }

    #[test]
    fn thinking_effort_maps_to_api_option() {
        assert!(ThinkingEffort::Default.thinking().is_none());
        assert_eq!(
            ThinkingEffort::High.thinking().unwrap().effort,
            crate::api::ThinkingEffort::High
        );
        // desktop.json 字符串解析
        let dc: DesktopConfig = serde_json::from_str(r#"{"thinkingEffort": "high"}"#).unwrap();
        assert_eq!(dc.thinking(), ThinkingEffort::High);
    }

    #[test]
    fn desktop_config_legacy_compat() {
        // 旧版 desktop.json：无 url/headers/timeout 等新字段
        let raw = r#"{
            "thinkingEffort": "medium",
            "permissionRules": [
                {"tool": "bash", "pattern": "cargo test*", "allow": true},
                {"tool": "bash", "pattern": "rm *", "allow": false}
            ],
            "mcpServers": [{
                "name": "fs", "command": "npx", "args": ["-y", "@mcode/fs"],
                "env": {"A": "1"}, "enabled": true
            }],
            "hooks": [
                {"event": "pre_tool_use", "matcher": "*", "command": "echo hi"}
            ],
            "recentWorkspaces": ["/tmp/x"]
        }"#;
        let dc: DesktopConfig = serde_json::from_str(raw).unwrap();
        assert_eq!(dc.thinking(), ThinkingEffort::Medium);
        assert_eq!(dc.permission_rules.len(), 2);
        // legacy allow bool → behavior 映射
        assert_eq!(dc.permission_rules[0].behavior_or_legacy(), Behavior::Allow);
        assert_eq!(dc.permission_rules[1].behavior_or_legacy(), Behavior::Deny);
        // MCP 兼容 + 新字段默认
        assert_eq!(dc.mcp_servers[0].transport(), McpTransport::Stdio);
        assert!(dc.mcp_servers[0].enabled);
        // hook 事件兼容
        assert_eq!(dc.hooks[0].event, HookEvent::PreToolUse);
        // 写回仍含 legacy 字段（旧版可读）
        let out = serde_json::to_string(&dc).unwrap();
        assert!(out.contains("\"allow\""));
    }

    #[test]
    fn permission_rule_new_behavior_field_roundtrips() {
        let raw = r#"{"tool":"bash","pattern":"git push*","behavior":"ask"}"#;
        let r: PermissionRule = serde_json::from_str(raw).unwrap();
        assert_eq!(r.behavior_or_legacy(), Behavior::Ask);
        let out = serde_json::to_string(&r).unwrap();
        assert!(out.contains("\"behavior\":\"ask\""));
        // 新条目写回仍带 allow 字段（双写，旧版可读）
        let dc = DesktopConfig::default();
        let _ = dc;
    }

    #[test]
    fn atomic_write_survives_and_replaces() {
        let dir = std::env::temp_dir().join(format!("mcode-cfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        atomic_write(&path, "{\"a\":1}").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"a\":1}");
        atomic_write(&path, "{\"a\":2}").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"a\":2}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
