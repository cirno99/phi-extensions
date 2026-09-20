// config.rs — RTK 集成配置：默认值、宽松归一化、读写。
//
// 由 pi 版 pi-rtk-optimizer 的 src/types.ts + src/config-store.ts 移植。
//
// 与 pi 版的差异：
// - 配置路径改为 phi 约定：`~/.phi/extensions/phi-rtk-optimizer/config.json`。
// - 归一化复用 `phi-ext-common::config` 的宽松解析（布尔/整数/枚举），
//   行为与 pi 版一致（未知值回落默认值，整数夹紧到区间）。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use phi_ext_common::config::{load_strict, save_atomic, to_bool, to_enum, to_int, ConfigError};
use phi_ext_common::paths;

/// 扩展名（同时作为 `~/.phi/extensions/<name>/` 的目录名）。
pub const EXTENSION_NAME: &str = "phi-rtk-optimizer";

/// 重写模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RtkMode {
    /// 直接把命令改写进 tool_call。
    Rewrite,
    /// 只提示不改写。
    Suggest,
}

/// 源码过滤强度。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceFilterLevel {
    None,
    Minimal,
    Aggressive,
}

/// `outputCompaction.readCompaction`。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ReadCompaction {
    pub enabled: bool,
}

/// `outputCompaction.truncate`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Truncate {
    pub enabled: bool,
    pub max_chars: u32,
}

impl Default for Truncate {
    fn default() -> Self {
        Self {
            enabled: true,
            max_chars: 12_000,
        }
    }
}

/// `outputCompaction.smartTruncate`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SmartTruncate {
    pub enabled: bool,
    pub max_lines: u32,
}

impl Default for SmartTruncate {
    fn default() -> Self {
        Self {
            enabled: false,
            max_lines: 220,
        }
    }
}

/// `outputCompaction`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct OutputCompaction {
    pub enabled: bool,
    pub strip_ansi: bool,
    pub read_compaction: ReadCompaction,
    pub truncate: Truncate,
    pub source_code_filtering_enabled: bool,
    pub preserve_exact_skill_reads: bool,
    pub source_code_filtering: SourceFilterLevel,
    pub smart_truncate: SmartTruncate,
    pub aggregate_test_output: bool,
    pub filter_build_output: bool,
    pub compact_git_output: bool,
    pub aggregate_linter_output: bool,
    pub group_search_output: bool,
    pub track_savings: bool,
}

impl Default for OutputCompaction {
    fn default() -> Self {
        Self {
            enabled: true,
            strip_ansi: true,
            read_compaction: ReadCompaction::default(),
            truncate: Truncate::default(),
            source_code_filtering_enabled: false,
            preserve_exact_skill_reads: false,
            source_code_filtering: SourceFilterLevel::None,
            smart_truncate: SmartTruncate::default(),
            aggregate_test_output: true,
            filter_build_output: true,
            compact_git_output: true,
            aggregate_linter_output: true,
            group_search_output: true,
            track_savings: true,
        }
    }
}

/// 顶层配置。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct RtkIntegrationConfig {
    pub enabled: bool,
    pub mode: RtkMode,
    pub guard_when_rtk_missing: bool,
    pub show_rewrite_notifications: bool,
    pub output_compaction: OutputCompaction,
}

impl Default for RtkIntegrationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            mode: RtkMode::Rewrite,
            guard_when_rtk_missing: true,
            show_rewrite_notifications: true,
            output_compaction: OutputCompaction::default(),
        }
    }
}

/// 宽松归一化：与 pi 版 `normalizeRtkIntegrationConfig` 逐字段对应。
///
/// 关键兼容点：旧版配置没有 `outputCompaction.readCompaction` 字段，
/// 此时读压缩、源码过滤、智能截断三项按「旧默认」取 `true` / `true` / `minimal`。
pub fn normalize(raw: &Value) -> RtkIntegrationConfig {
    let defaults = RtkIntegrationConfig::default();
    let compaction_source = raw.get("outputCompaction").unwrap_or(&Value::Null);
    let read_source = compaction_source.get("readCompaction");
    let truncate_source = compaction_source.get("truncate").unwrap_or(&Value::Null);
    let smart_source = compaction_source.get("smartTruncate").unwrap_or(&Value::Null);

    let has_read_compaction = read_source.is_some();
    let legacy = !has_read_compaction;
    let source_filtering_fallback = if legacy {
        true
    } else {
        defaults.output_compaction.source_code_filtering_enabled
    };
    let source_filter_level_fallback = if legacy {
        SourceFilterLevel::Minimal
    } else {
        defaults.output_compaction.source_code_filtering
    };
    let smart_truncate_enabled_fallback = if legacy {
        true
    } else {
        defaults.output_compaction.smart_truncate.enabled
    };

    let dc = &defaults.output_compaction;
    RtkIntegrationConfig {
        enabled: to_bool(
            raw.get("enabled").unwrap_or(&Value::Null),
            defaults.enabled,
        ),
        mode: to_enum(
            raw.get("mode").unwrap_or(&Value::Null),
            &[("rewrite", RtkMode::Rewrite), ("suggest", RtkMode::Suggest)],
            defaults.mode,
        ),
        guard_when_rtk_missing: to_bool(
            raw.get("guardWhenRtkMissing").unwrap_or(&Value::Null),
            defaults.guard_when_rtk_missing,
        ),
        show_rewrite_notifications: to_bool(
            raw.get("showRewriteNotifications").unwrap_or(&Value::Null),
            defaults.show_rewrite_notifications,
        ),
        output_compaction: OutputCompaction {
            enabled: to_bool(
                compaction_source.get("enabled").unwrap_or(&Value::Null),
                dc.enabled,
            ),
            strip_ansi: to_bool(
                compaction_source.get("stripAnsi").unwrap_or(&Value::Null),
                dc.strip_ansi,
            ),
            read_compaction: ReadCompaction {
                enabled: if has_read_compaction {
                    to_bool(
                        read_source
                            .and_then(|v| v.get("enabled"))
                            .unwrap_or(&Value::Null),
                        dc.read_compaction.enabled,
                    )
                } else {
                    true
                },
            },
            source_code_filtering_enabled: to_bool(
                compaction_source
                    .get("sourceCodeFilteringEnabled")
                    .unwrap_or(&Value::Null),
                source_filtering_fallback,
            ),
            preserve_exact_skill_reads: to_bool(
                compaction_source
                    .get("preserveExactSkillReads")
                    .unwrap_or(&Value::Null),
                dc.preserve_exact_skill_reads,
            ),
            truncate: Truncate {
                enabled: to_bool(
                    truncate_source.get("enabled").unwrap_or(&Value::Null),
                    dc.truncate.enabled,
                ),
                max_chars: to_int(
                    truncate_source.get("maxChars").unwrap_or(&Value::Null),
                    1_000,
                    200_000,
                    i64::from(dc.truncate.max_chars),
                ) as u32,
            },
            source_code_filtering: to_enum(
                compaction_source
                    .get("sourceCodeFiltering")
                    .unwrap_or(&Value::Null),
                &[
                    ("none", SourceFilterLevel::None),
                    ("minimal", SourceFilterLevel::Minimal),
                    ("aggressive", SourceFilterLevel::Aggressive),
                ],
                source_filter_level_fallback,
            ),
            smart_truncate: SmartTruncate {
                enabled: to_bool(
                    smart_source.get("enabled").unwrap_or(&Value::Null),
                    smart_truncate_enabled_fallback,
                ),
                max_lines: to_int(
                    smart_source.get("maxLines").unwrap_or(&Value::Null),
                    40,
                    4_000,
                    i64::from(dc.smart_truncate.max_lines),
                ) as u32,
            },
            aggregate_test_output: to_bool(
                compaction_source
                    .get("aggregateTestOutput")
                    .unwrap_or(&Value::Null),
                dc.aggregate_test_output,
            ),
            filter_build_output: to_bool(
                compaction_source
                    .get("filterBuildOutput")
                    .unwrap_or(&Value::Null),
                dc.filter_build_output,
            ),
            compact_git_output: to_bool(
                compaction_source
                    .get("compactGitOutput")
                    .unwrap_or(&Value::Null),
                dc.compact_git_output,
            ),
            aggregate_linter_output: to_bool(
                compaction_source
                    .get("aggregateLinterOutput")
                    .unwrap_or(&Value::Null),
                dc.aggregate_linter_output,
            ),
            group_search_output: to_bool(
                compaction_source
                    .get("groupSearchOutput")
                    .unwrap_or(&Value::Null),
                dc.group_search_output,
            ),
            track_savings: to_bool(
                compaction_source
                    .get("trackSavings")
                    .unwrap_or(&Value::Null),
                dc.track_savings,
            ),
        },
    }
}

/// 配置文件路径：`~/.phi/extensions/phi-rtk-optimizer/config.json`。
pub fn config_path() -> PathBuf {
    paths::extension_config_path(EXTENSION_NAME)
}

/// 配置加载结果。
#[derive(Debug, Clone)]
pub struct LoadOutcome {
    /// 归一化后的配置。
    pub config: RtkIntegrationConfig,
    /// 解析失败时的告警（成功时为 `None`）。
    pub warning: Option<String>,
}

/// 读取配置；文件缺失或解析失败时回落到默认值并给出告警。
pub fn load(path: &Path) -> LoadOutcome {
    match load_strict::<Value>(path) {
        Ok(Some(value)) => LoadOutcome {
            config: normalize(&value),
            warning: None,
        },
        Ok(None) => LoadOutcome {
            config: RtkIntegrationConfig::default(),
            warning: None,
        },
        Err(err) => LoadOutcome {
            config: RtkIntegrationConfig::default(),
            warning: Some(format!("Failed to parse {}: {err}", path.display())),
        },
    }
}

/// 原子保存配置（写入前先归一化）。
pub fn save(config: &RtkIntegrationConfig, path: &Path) -> Result<(), ConfigError> {
    let normalized = normalize(&serde_json::to_value(config)?);
    save_atomic(path, &normalized)
}

/// 配置文件不存在时写出默认配置。
pub fn ensure_exists(path: &Path) -> Result<bool, ConfigError> {
    if path.exists() {
        return Ok(false);
    }
    save(&RtkIntegrationConfig::default(), path)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn defaults_should_match_pi_version() {
        let config = RtkIntegrationConfig::default();
        assert!(config.enabled);
        assert_eq!(config.mode, RtkMode::Rewrite);
        assert!(config.guard_when_rtk_missing);
        assert!(config.show_rewrite_notifications);
        let oc = &config.output_compaction;
        assert!(oc.enabled && oc.strip_ansi);
        assert!(!oc.read_compaction.enabled);
        assert_eq!(oc.truncate.max_chars, 12_000);
        assert_eq!(oc.source_code_filtering, SourceFilterLevel::None);
        assert_eq!(oc.smart_truncate.max_lines, 220);
    }

    #[test]
    fn normalize_should_keep_explicit_values() {
        let config = normalize(&json!({
            "enabled": false,
            "mode": "suggest",
            "outputCompaction": {
                "stripAnsi": false,
                "readCompaction": { "enabled": true },
                "truncate": { "maxChars": 500 },
                "smartTruncate": { "maxLines": 10 }
            }
        }));
        assert!(!config.enabled);
        assert_eq!(config.mode, RtkMode::Suggest);
        assert!(!config.output_compaction.strip_ansi);
        assert!(config.output_compaction.read_compaction.enabled);
        // 夹紧到区间下界
        assert_eq!(config.output_compaction.truncate.max_chars, 1_000);
        assert_eq!(config.output_compaction.smart_truncate.max_lines, 40);
    }

    #[test]
    fn normalize_should_apply_legacy_defaults_without_read_compaction() {
        let config = normalize(&json!({ "outputCompaction": { "enabled": true } }));
        assert!(config.output_compaction.read_compaction.enabled);
        assert!(config.output_compaction.source_code_filtering_enabled);
        assert_eq!(
            config.output_compaction.source_code_filtering,
            SourceFilterLevel::Minimal
        );
        assert!(config.output_compaction.smart_truncate.enabled);
    }

    #[test]
    fn normalize_should_respect_new_defaults_when_read_compaction_present() {
        let config = normalize(&json!({ "outputCompaction": { "readCompaction": {} } }));
        assert!(!config.output_compaction.read_compaction.enabled);
        assert!(!config.output_compaction.source_code_filtering_enabled);
        assert_eq!(
            config.output_compaction.source_code_filtering,
            SourceFilterLevel::None
        );
        assert!(!config.output_compaction.smart_truncate.enabled);
    }

    #[test]
    fn normalize_should_fall_back_on_unknown_enum() {
        // 没有 readCompaction 字段 → 走旧默认，源码过滤级别回落 minimal。
        let config = normalize(&json!({ "mode": "nope", "outputCompaction": { "sourceCodeFiltering": "loud" } }));
        assert_eq!(config.mode, RtkMode::Rewrite);
        assert_eq!(
            config.output_compaction.source_code_filtering,
            SourceFilterLevel::Minimal
        );
    }

    #[test]
    fn save_then_load_should_round_trip() {
        let dir = std::env::temp_dir().join(format!("phi-rtk-config-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("config.json");
        let mut config = RtkIntegrationConfig {
            mode: RtkMode::Suggest,
            ..RtkIntegrationConfig::default()
        };
        config.output_compaction.truncate.max_chars = 4_000;
        save(&config, &path).expect("保存应成功");
        let loaded = load(&path);
        assert!(loaded.warning.is_none());
        assert_eq!(loaded.config, config);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_should_warn_on_broken_json_and_use_defaults() {
        let dir = std::env::temp_dir().join(format!("phi-rtk-broken-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建目录应成功");
        let path = dir.join("config.json");
        std::fs::write(&path, "{ not json").expect("写入应成功");
        let loaded = load(&path);
        assert!(loaded.warning.is_some());
        assert_eq!(loaded.config, RtkIntegrationConfig::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ensure_exists_should_create_once() {
        let dir = std::env::temp_dir().join(format!("phi-rtk-ensure-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("config.json");
        assert!(ensure_exists(&path).expect("创建应成功"));
        assert!(!ensure_exists(&path).expect("二次调用应成功"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn serialized_keys_should_be_camel_case() {
        let value = serde_json::to_value(RtkIntegrationConfig::default()).expect("序列化应成功");
        assert!(value.get("guardWhenRtkMissing").is_some());
        assert!(value.get("outputCompaction").is_some());
        assert!(value["outputCompaction"].get("stripAnsi").is_some());
        assert!(value["outputCompaction"].get("smartTruncate").is_some());
        assert_eq!(value["mode"], json!("rewrite"));
    }
}