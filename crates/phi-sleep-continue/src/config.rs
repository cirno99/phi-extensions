// config.rs — phi-sleep-continue 的持久化配置（自动审批相关）。
//
// 参照 pi-auto-approval 的 src/extension-config.ts + src/types.ts：
// 只保留在 phi 里真正可执行的字段（分类器模型、超时、审计等依赖宿主
// LLM 调用与 UI 的字段已去掉，见 PLAN.md）。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use phi_ext_common::json::{Value, ValueAsArray, ValueAsScalar, ValueObjectAccess};

use phi_ext_common::config::{load_strict, save_atomic, to_bool, to_enum, to_int, ConfigError};
use phi_ext_common::paths;

/// 扩展名（同时作为 `~/.phi/extensions/<name>/` 的目录名）。
pub const EXTENSION_NAME: &str = "phi-sleep-continue";

/// 自动审批模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalMode {
    /// 只放行「可证明安全」的动作（只读工具、工作区内写入、安全只读命令、
    /// 白名单），其余一律阻止。
    Safe,
    /// 在 `Safe` 之外，还放行所有未被 `deny` 命中的动作（信任 agent 时使用）。
    Permissive,
}

/// 自动审批配置。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ApprovalConfig {
    /// 是否启用自动审批路由（仅在无人值守模式开启时生效）。
    pub enabled: bool,
    /// 审批模式。
    pub mode: ApprovalMode,
    /// 连续拒绝多少次后，提示模型停下来向用户求助。
    pub max_consecutive_denials: u32,
    /// 额外视为安全的只读命令前缀（`*` 结尾表示前缀匹配）。
    pub safe_command_allowlist: Vec<String>,
    /// 额外放行的动作（工具名，或 bash 命令前缀）。
    pub allow: Vec<String>,
    /// 显式阻止的动作（工具名，或 bash 命令前缀）。
    pub deny: Vec<String>,
}

impl Default for ApprovalConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            mode: ApprovalMode::Safe,
            max_consecutive_denials: 3,
            safe_command_allowlist: Vec::new(),
            allow: Vec::new(),
            deny: Vec::new(),
        }
    }
}

/// 宽松归一化：未知值回落默认值，列表只保留字符串项。
pub fn normalize(raw: &Value) -> ApprovalConfig {
    let defaults = ApprovalConfig::default();
    let strings = |value: Option<&Value>| -> Vec<String> {
        value
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    };
    ApprovalConfig {
        enabled: to_bool(raw.get("enabled"), defaults.enabled),
        mode: to_enum(
            raw.get("mode"),
            &[
                ("safe", ApprovalMode::Safe),
                ("permissive", ApprovalMode::Permissive),
            ],
            defaults.mode,
        ),
        max_consecutive_denials: to_int(
            raw.get("maxConsecutiveDenials"),
            1,
            100,
            i64::from(defaults.max_consecutive_denials),
        ) as u32,
        safe_command_allowlist: strings(raw.get("safeCommandAllowlist")),
        allow: strings(raw.get("allow")),
        deny: strings(raw.get("deny")),
    }
}

/// 配置文件路径：`~/.phi/extensions/phi-sleep-continue/config.json`。
pub fn config_path() -> PathBuf {
    paths::extension_config_path(EXTENSION_NAME)
}

/// 读取配置；缺失或损坏时回落默认值并给出告警。
pub fn load(path: &Path) -> (ApprovalConfig, Option<String>) {
    match load_strict::<Value>(path) {
        Ok(Some(value)) => (normalize(&value), None),
        Ok(None) => (ApprovalConfig::default(), None),
        Err(err) => (
            ApprovalConfig::default(),
            Some(format!("Failed to parse {}: {err}", path.display())),
        ),
    }
}

/// 原子保存配置（写入前先归一化）。
pub fn save(config: &ApprovalConfig, path: &Path) -> Result<(), ConfigError> {
    let normalized = normalize(&phi_ext_common::json::to_value(config)?);
    save_atomic(path, &normalized)
}

#[cfg(test)]
mod tests {
    use super::*;
    use phi_ext_common::json::json;

    #[test]
    fn defaults_should_be_safe_and_enabled() {
        let config = ApprovalConfig::default();
        assert!(config.enabled);
        assert_eq!(config.mode, ApprovalMode::Safe);
        assert_eq!(config.max_consecutive_denials, 3);
        assert!(config.safe_command_allowlist.is_empty());
    }

    #[test]
    fn normalize_should_read_explicit_values() {
        let config = normalize(&json!({
            "enabled": false,
            "mode": "permissive",
            "maxConsecutiveDenials": 7,
            "safeCommandAllowlist": ["rg", "fd*"],
            "allow": ["web_fetch"],
            "deny": ["rm*"]
        }));
        assert!(!config.enabled);
        assert_eq!(config.mode, ApprovalMode::Permissive);
        assert_eq!(config.max_consecutive_denials, 7);
        assert_eq!(config.safe_command_allowlist, vec!["rg", "fd*"]);
        assert_eq!(config.allow, vec!["web_fetch"]);
        assert_eq!(config.deny, vec!["rm*"]);
    }

    #[test]
    fn normalize_should_clamp_and_ignore_bad_types() {
        let config = normalize(&json!({
            "mode": "nope",
            "maxConsecutiveDenials": 0,
            "safeCommandAllowlist": ["ok", 42, null]
        }));
        assert_eq!(config.mode, ApprovalMode::Safe);
        assert_eq!(config.max_consecutive_denials, 1);
        assert_eq!(config.safe_command_allowlist, vec!["ok"]);
    }

    #[test]
    fn save_then_load_should_round_trip() {
        let dir = std::env::temp_dir().join(format!("phi-sleep-config-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("config.json");
        let config = ApprovalConfig {
            mode: ApprovalMode::Permissive,
            deny: vec!["rm*".to_string()],
            ..ApprovalConfig::default()
        };
        save(&config, &path).expect("保存应成功");
        let (loaded, warning) = load(&path);
        assert!(warning.is_none());
        assert_eq!(loaded, config);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_should_warn_on_broken_json() {
        let dir = std::env::temp_dir().join(format!("phi-sleep-broken-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建目录应成功");
        let path = dir.join("config.json");
        std::fs::write(&path, "{ oops").expect("写入应成功");
        let (config, warning) = load(&path);
        assert!(warning.is_some());
        assert_eq!(config, ApprovalConfig::default());
        let _ = std::fs::remove_dir_all(&dir);
    }
}