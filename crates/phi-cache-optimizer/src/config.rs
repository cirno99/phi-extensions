// config.rs — phi-cache-optimizer 配置。
//
// 参照 pi 版 pi-cache-optimizer 的配置项，只保留在 phi 里仍有意义的字段：
// pi 的 `prompt_cache_key` 注入与 footer 统计都依赖宿主能力，见 doctor.rs。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use phi_ext_common::json::{Value, ValueObjectAccess};

use phi_ext_common::config::{load_strict, save_atomic, to_bool, to_enum, ConfigError};
use phi_ext_common::paths;

/// 扩展名（同时作为 `~/.phi/extensions/<name>/` 的目录名）。
pub const EXTENSION_NAME: &str = "phi-cache-optimizer";

/// footer 统计口径（pi 版同名概念）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FooterMode {
    /// 当前会话。
    Session,
    /// 本地全部会话合计。
    Total,
    /// 当前扩展进程。
    Process,
}

/// `prompt_cache_key` 策略（pi 版同名概念）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PromptCacheKeyMode {
    /// 交给宿主决定。
    Auto,
    /// 显式省略（phi 目前无法注入请求体，仅记录意图）。
    Omit,
}

/// 缓存优化配置。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CacheOptimizerConfig {
    /// 是否启用（用于 status / doctor 的展示口径）。
    pub enabled: bool,
    /// footer 统计口径。
    pub footer_mode: FooterMode,
    /// `prompt_cache_key` 策略。
    pub prompt_cache_key: PromptCacheKeyMode,
}

impl Default for CacheOptimizerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            footer_mode: FooterMode::Session,
            prompt_cache_key: PromptCacheKeyMode::Auto,
        }
    }
}

/// 宽松归一化。
pub fn normalize(raw: &Value) -> CacheOptimizerConfig {
    let defaults = CacheOptimizerConfig::default();
    CacheOptimizerConfig {
        enabled: to_bool(raw.get("enabled"), defaults.enabled),
        footer_mode: to_enum(
            raw.get("footerMode"),
            &[
                ("session", FooterMode::Session),
                ("total", FooterMode::Total),
                ("process", FooterMode::Process),
            ],
            defaults.footer_mode,
        ),
        prompt_cache_key: to_enum(
            raw.get("promptCacheKey"),
            &[
                ("auto", PromptCacheKeyMode::Auto),
                ("omit", PromptCacheKeyMode::Omit),
            ],
            defaults.prompt_cache_key,
        ),
    }
}

/// 配置文件路径。
pub fn config_path() -> PathBuf {
    paths::extension_config_path(EXTENSION_NAME)
}

/// 读取配置；缺失或损坏时回落默认值并给出告警。
pub fn load(path: &Path) -> (CacheOptimizerConfig, Option<String>) {
    match load_strict::<Value>(path) {
        Ok(Some(value)) => (normalize(&value), None),
        Ok(None) => (CacheOptimizerConfig::default(), None),
        Err(err) => (
            CacheOptimizerConfig::default(),
            Some(format!("Failed to parse {}: {err}", path.display())),
        ),
    }
}

/// 原子保存配置。
pub fn save(config: &CacheOptimizerConfig, path: &Path) -> Result<(), ConfigError> {
    let normalized = normalize(&phi_ext_common::json::to_value(config)?);
    save_atomic(path, &normalized)
}

#[cfg(test)]
mod tests {
    use super::*;
    use phi_ext_common::json::json;

    #[test]
    fn defaults_should_match_pi_intent() {
        let config = CacheOptimizerConfig::default();
        assert!(config.enabled);
        assert_eq!(config.footer_mode, FooterMode::Session);
        assert_eq!(config.prompt_cache_key, PromptCacheKeyMode::Auto);
    }

    #[test]
    fn normalize_should_read_explicit_values() {
        let config = normalize(&json!({
            "enabled": false,
            "footerMode": "total",
            "promptCacheKey": "omit"
        }));
        assert!(!config.enabled);
        assert_eq!(config.footer_mode, FooterMode::Total);
        assert_eq!(config.prompt_cache_key, PromptCacheKeyMode::Omit);
    }

    #[test]
    fn normalize_should_fall_back_on_unknown_values() {
        let config = normalize(&json!({ "footerMode": "nope", "promptCacheKey": 7 }));
        assert_eq!(config.footer_mode, FooterMode::Session);
        assert_eq!(config.prompt_cache_key, PromptCacheKeyMode::Auto);
    }

    #[test]
    fn save_then_load_should_round_trip() {
        let dir = std::env::temp_dir().join(format!("phi-cache-cfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("config.json");
        let config = CacheOptimizerConfig {
            footer_mode: FooterMode::Process,
            prompt_cache_key: PromptCacheKeyMode::Omit,
            ..CacheOptimizerConfig::default()
        };
        save(&config, &path).expect("保存应成功");
        let (loaded, warning) = load(&path);
        assert!(warning.is_none());
        assert_eq!(loaded, config);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_should_warn_on_broken_json() {
        let dir = std::env::temp_dir().join(format!("phi-cache-broken-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建目录应成功");
        let path = dir.join("config.json");
        std::fs::write(&path, "[not, json").expect("写入应成功");
        let (config, warning) = load(&path);
        assert!(warning.is_some());
        assert_eq!(config, CacheOptimizerConfig::default());
        let _ = std::fs::remove_dir_all(&dir);
    }
}