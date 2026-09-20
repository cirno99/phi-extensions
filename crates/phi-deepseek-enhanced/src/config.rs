// config.rs — phi-deepseek-enhanced 的持久化配置。
//
// 对应 Oh My Pi 版 deepseek-enhanced.ts 的隐含行为（原扩展没有配置文件，
// 一切写死）。移植到 phi 后，部分行为因宿主能力缺失而变成「可选项」，
// 因此把它们收敛为配置项，默认值尽量贴近原扩展、但对会破坏正常流程的
// 项（Eternal Minimal 运行时守卫）默认关闭。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use phi_ext_common::json::{Value, ValueAsArray, ValueAsScalar, ValueObjectAccess};

use phi_ext_common::config::{load_strict, save_atomic, to_bool, ConfigError};
use phi_ext_common::paths;

/// 扩展名（同时作为 `~/.phi/extensions/<name>/` 的目录名）。
pub const EXTENSION_NAME: &str = "phi-deepseek-enhanced";

/// 默认「核心工具」：Eternal Minimal 下唯一允许直接调用的工具。
pub const DEFAULT_CORE_TOOLS: [&str; 2] = ["bash", "str_replace_editor"];
/// 默认「传输工具」：`xd://` 网关在 phi 中不可落地，仅保留 read/write 直呼开关。
pub const DEFAULT_TRANSPORT_TOOLS: [&str; 2] = ["read", "write"];

/// 扩展配置。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Config {
    /// 总开关。关闭后所有钩子都不生效（str_replace_editor 仍会注册）。
    pub enabled: bool,
    /// 是否注入 "We need to ..." 推理风格锚点提示。
    pub inject_anchor: bool,
    /// 上下文压缩后是否重新注入锚点（压缩会丢弃早先的用户消息）。
    pub reanchor_after_compact: bool,
    /// 是否剥离用户消息开头的「Today / current working directory」系统提醒。
    ///
    /// 默认关闭：当前 phi 宿主已不再注入这种提醒（实测所有会话的 user 消息
    /// 均未出现该形态），开启后是纯空转。检测到时仍可手动 `/deepseek strip on`。
    pub strip_date_cwd_reminder: bool,
    /// Eternal Minimal 运行时守卫：阻止对非核心工具的直接调用。
    ///
    /// 默认关闭：phi 无法收缩模型可见的工具清单，也无法落地 `xd://` 网关，
    /// 开启后模型会真的无法调用被阻止的工具，仅在明确需要「强制 shell/编辑器
    /// 工作流」时启用。
    pub minimal: bool,
    /// 是否允许 read/write 作为传输工具直呼（`minimal` 生效时才有意义）。
    pub transport: bool,
    /// 核心工具名单（minimal 模式下允许直呼）。
    pub core_tools: Vec<String>,
    /// 传输工具名单（transport 开启时允许直呼）。
    pub transport_tools: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: true,
            inject_anchor: true,
            reanchor_after_compact: true,
            strip_date_cwd_reminder: false,
            minimal: false,
            transport: true,
            core_tools: DEFAULT_CORE_TOOLS.iter().map(|s| s.to_string()).collect(),
            transport_tools: DEFAULT_TRANSPORT_TOOLS
                .iter()
                .map(|s| s.to_string())
                .collect(),
        }
    }
}

impl Config {
    /// minimal 模式下允许直呼的全部工具名。
    pub fn allowed_direct_tools(&self) -> Vec<String> {
        let mut tools = self.core_tools.clone();
        if self.transport {
            tools.extend(self.transport_tools.iter().cloned());
        }
        tools
    }
}

/// 宽松归一化：未知值回落默认值，列表只保留字符串项。
pub fn normalize(raw: &Value) -> Config {
    let defaults = Config::default();
    let strings = |value: Option<&Value>, fallback: &[String]| -> Vec<String> {
        match value.and_then(Value::as_array) {
            Some(items) => items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect(),
            None => fallback.to_vec(),
        }
    };
    Config {
        enabled: to_bool(raw.get("enabled"), defaults.enabled),
        inject_anchor: to_bool(raw.get("injectAnchor"), defaults.inject_anchor),
        reanchor_after_compact: to_bool(
            raw.get("reanchorAfterCompact"),
            defaults.reanchor_after_compact,
        ),
        strip_date_cwd_reminder: to_bool(
            raw.get("stripDateCwdReminder"),
            defaults.strip_date_cwd_reminder,
        ),
        minimal: to_bool(raw.get("minimal"), defaults.minimal),
        transport: to_bool(raw.get("transport"), defaults.transport),
        core_tools: strings(raw.get("coreTools"), &defaults.core_tools),
        transport_tools: strings(raw.get("transportTools"), &defaults.transport_tools),
    }
}

/// 配置文件路径：`~/.phi/extensions/phi-deepseek-enhanced/config.json`。
pub fn config_path() -> PathBuf {
    paths::extension_config_path(EXTENSION_NAME)
}

/// 读取配置；缺失或损坏时回落默认值并给出告警。
pub fn load(path: &Path) -> (Config, Option<String>) {
    match load_strict::<Value>(path) {
        Ok(Some(value)) => (normalize(&value), None),
        Ok(None) => (Config::default(), None),
        Err(err) => (
            Config::default(),
            Some(format!("Failed to parse {}: {err}", path.display())),
        ),
    }
}

/// 原子保存配置（写入前先归一化）。
pub fn save(config: &Config, path: &Path) -> Result<(), ConfigError> {
    let normalized = normalize(&phi_ext_common::json::to_value(config)?);
    save_atomic(path, &normalized)
}

#[cfg(test)]
mod tests {
    use super::*;
    use phi_ext_common::json::json;

    #[test]
    fn defaults_should_match_upstream_intent() {
        let config = Config::default();
        assert!(config.enabled);
        assert!(config.inject_anchor);
        assert!(config.reanchor_after_compact);
        // 宿主已不再注入 Today/cwd 提醒，默认关闭以避免空转（见字段注释）。
        assert!(!config.strip_date_cwd_reminder);
        // Eternal Minimal 守卫在 phi 中默认关闭（见文件头注释）。
        assert!(!config.minimal);
        assert!(config.transport);
        assert_eq!(config.core_tools, vec!["bash", "str_replace_editor"]);
        assert_eq!(config.transport_tools, vec!["read", "write"]);
    }

    #[test]
    fn normalize_should_read_explicit_values() {
        let config = normalize(&json!({
            "enabled": false,
            "injectAnchor": false,
            "reanchorAfterCompact": false,
            "stripDateCwdReminder": false,
            "minimal": true,
            "transport": false,
            "coreTools": ["bash"],
            "transportTools": ["read"]
        }));
        assert!(!config.enabled);
        assert!(!config.inject_anchor);
        assert!(!config.reanchor_after_compact);
        assert!(!config.strip_date_cwd_reminder);
        assert!(config.minimal);
        assert!(!config.transport);
        assert_eq!(config.core_tools, vec!["bash"]);
        assert_eq!(config.transport_tools, vec!["read"]);
    }

    #[test]
    fn normalize_should_ignore_bad_types_and_keep_default_lists() {
        let config = normalize(&json!({
            "enabled": "yes",
            "coreTools": ["bash", 42, null]
        }));
        assert!(config.enabled);
        assert_eq!(config.core_tools, vec!["bash"]);
        assert_eq!(config.transport_tools, vec!["read", "write"]);
    }

    #[test]
    fn allowed_direct_tools_should_include_transport_when_enabled() {
        let mut config = Config {
            minimal: true,
            ..Config::default()
        };
        assert_eq!(
            config.allowed_direct_tools(),
            vec!["bash", "str_replace_editor", "read", "write"]
        );
        config.transport = false;
        assert_eq!(
            config.allowed_direct_tools(),
            vec!["bash", "str_replace_editor"]
        );
    }

    #[test]
    fn save_then_load_should_round_trip() {
        let dir = std::env::temp_dir().join(format!("phi-dse-config-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("config.json");
        let config = Config {
            minimal: true,
            core_tools: vec!["bash".to_string()],
            ..Config::default()
        };
        save(&config, &path).expect("保存应成功");
        let (loaded, warning) = load(&path);
        assert!(warning.is_none());
        assert_eq!(loaded, config);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_should_warn_on_broken_json() {
        let dir = std::env::temp_dir().join(format!("phi-dse-broken-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建目录应成功");
        let path = dir.join("config.json");
        std::fs::write(&path, "{ oops").expect("写入应成功");
        let (config, warning) = load(&path);
        assert!(warning.is_some());
        assert_eq!(config, Config::default());
        let _ = std::fs::remove_dir_all(&dir);
    }
}