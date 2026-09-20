//! JSON 配置文件读写。
//!
//! 所有扩展的配置都放在 `<phi_home>/extensions/<name>/config.json`。
//! 写入采用「临时文件 + rename」的原子方式，避免进程被杀时留下半个文件。
//! 编解码统一走 simd-json（与 `crate::json` 同一实现），不再依赖 serde_json。

use std::fs;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::json::{Value, ValueAsScalar, ValueObjectAccess};

/// 配置读写错误。
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// 文件系统操作失败。
    #[error("配置文件 IO 失败（{path}）：{source}")]
    Io {
        /// 出错的文件路径。
        path: PathBuf,
        /// 底层 IO 错误。
        #[source]
        source: std::io::Error,
    },
    /// JSON 序列化失败。
    #[error("配置序列化失败：{0}")]
    Serialize(#[from] simd_json::Error),
    /// 目标路径没有父目录，无法创建目录。
    #[error("配置路径没有父目录：{path}")]
    NoParent {
        /// 出错的路径。
        path: PathBuf,
    },
}

impl From<crate::json::SerializeError> for ConfigError {
    fn from(err: crate::json::SerializeError) -> Self {
        ConfigError::Serialize(err.0)
    }
}

/// 用 simd-json 反序列化（内部复制一份再原地解析）。
fn parse<T: DeserializeOwned>(raw: &str) -> Result<T, simd_json::Error> {
    let mut bytes = raw.as_bytes().to_vec();
    simd_json::serde::from_slice(&mut bytes)
}

/// 读取 JSON 配置。
///
/// 文件不存在、内容为空或解析失败时返回 `T::default()`，并保留原始文件不动
/// —— 扩展不应因为用户手改坏了配置文件就无法启动。
pub fn load_or_default<T>(path: &Path) -> T
where
    T: DeserializeOwned + Default,
{
    let Ok(raw) = fs::read_to_string(path) else {
        return T::default();
    };
    if raw.trim().is_empty() {
        return T::default();
    }
    parse(&raw).unwrap_or_default()
}

/// 读取 JSON 配置；文件存在但解析失败时返回 `Err`。
///
/// 与 [`load_or_default`] 不同，这个版本不会静默吞掉解析错误，适合需要
/// 明确告知用户「你的配置文件写错了」的场景。
pub fn load_strict<T>(path: &Path) -> Result<Option<T>, ConfigError>
where
    T: DeserializeOwned,
{
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(ConfigError::Io {
                path: path.to_path_buf(),
                source,
            })
        }
    };
    if raw.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(parse(&raw)?))
}

/// 原子写入 JSON 配置。
///
/// 步骤：创建父目录 → 写入 `<path>.tmp` → rename 覆盖目标。
/// rename 在同一文件系统内是原子的，因此不会出现半写状态。
pub fn save_atomic<T>(path: &Path, value: &T) -> Result<(), ConfigError>
where
    T: Serialize,
{
    let parent = path.parent().ok_or_else(|| ConfigError::NoParent {
        path: path.to_path_buf(),
    })?;
    fs::create_dir_all(parent).map_err(|source| ConfigError::Io {
        path: parent.to_path_buf(),
        source,
    })?;

    let body = simd_json::serde::to_string_pretty(value)?;
    let tmp = temp_path(path);
    fs::write(&tmp, body).map_err(|source| ConfigError::Io {
        path: tmp.clone(),
        source,
    })?;
    fs::rename(&tmp, path).map_err(|source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    })
}

/// 生成同目录下的临时文件路径。
fn temp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}.tmp", std::process::id()));
    path.with_file_name(name)
}

/// 从可选的父对象里取子字段（父缺失时返回 `None`）。
///
/// 归一化函数里大量出现「父级可选、再取子字段」的写法，此辅助函数避免把
/// `Value::Null` 之类的占位值四处传递。
pub fn child<'a>(parent: Option<&'a Value>, key: &str) -> Option<&'a Value> {
    parent.and_then(|value| value.get(key))
}

/// 宽松布尔解析：接受 `true/false`、`1/0`、`yes/no`、`on/off`（大小写不敏感）。
///
/// 无法识别时返回 `fallback`。用于兼容手写配置文件里的各种写法。
pub fn to_bool(value: Option<&Value>, fallback: bool) -> bool {
    let Some(value) = value else {
        return fallback;
    };
    if let Some(flag) = value.as_bool() {
        return flag;
    }
    if let Some(number) = value.as_i64() {
        return number != 0;
    }
    if let Some(text) = value.as_str() {
        return match text.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => true,
            "false" | "0" | "no" | "off" => false,
            _ => fallback,
        };
    }
    fallback
}

/// 整数解析并夹紧到 `[min, max]`。
///
/// 浮点数会被截断为整数。无法解析时返回 `fallback`（同样会被夹紧）。
pub fn to_int(value: Option<&Value>, min: i64, max: i64, fallback: i64) -> i64 {
    let parsed = value.and_then(|value| {
        value
            .as_i64()
            .or_else(|| value.as_f64().map(|number| number as i64))
            .or_else(|| value.as_str().and_then(|text| text.trim().parse::<i64>().ok()))
    });
    parsed.unwrap_or(fallback).clamp(min, max)
}

/// 字符串枚举解析：大小写不敏感地匹配候选值，未命中时返回 `fallback`。
pub fn to_enum<T: Copy>(value: Option<&Value>, candidates: &[(&str, T)], fallback: T) -> T {
    let Some(raw) = value.and_then(|value| value.as_str()) else {
        return fallback;
    };
    let lowered = raw.trim().to_ascii_lowercase();
    candidates
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(&lowered))
        .map(|(_, v)| *v)
        .unwrap_or(fallback)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::json;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
    struct Demo {
        enabled: bool,
        limit: u32,
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("phi-ext-common-config-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn load_or_default_should_return_default_when_file_missing() {
        let path = temp_dir("missing").join("config.json");
        assert_eq!(load_or_default::<Demo>(&path), Demo::default());
    }

    #[test]
    fn load_or_default_should_return_default_when_json_is_broken() {
        let dir = temp_dir("broken");
        fs::create_dir_all(&dir).expect("创建临时目录应成功");
        let path = dir.join("config.json");
        fs::write(&path, "{ not json").expect("写入应成功");
        assert_eq!(load_or_default::<Demo>(&path), Demo::default());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_then_load_should_round_trip() {
        let dir = temp_dir("roundtrip");
        let path = dir.join("config.json");
        let value = Demo {
            enabled: true,
            limit: 42,
        };
        save_atomic(&path, &value).expect("保存应成功");
        assert_eq!(load_or_default::<Demo>(&path), value);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_atomic_should_not_leave_temp_file_behind() {
        let dir = temp_dir("notmp");
        let path = dir.join("config.json");
        save_atomic(&path, &Demo::default()).expect("保存应成功");
        let entries: Vec<_> = fs::read_dir(&dir)
            .expect("读取目录应成功")
            .filter_map(|e| e.ok().map(|e| e.file_name()))
            .collect();
        assert_eq!(entries.len(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_strict_should_return_none_when_file_missing() {
        let path = temp_dir("strict-missing").join("config.json");
        assert!(matches!(load_strict::<Demo>(&path), Ok(None)));
    }

    #[test]
    fn load_strict_should_report_parse_error() {
        let dir = temp_dir("strict-broken");
        fs::create_dir_all(&dir).expect("创建临时目录应成功");
        let path = dir.join("config.json");
        fs::write(&path, "[1,2,3]").expect("写入应成功");
        assert!(load_strict::<Demo>(&path).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn to_bool_should_accept_common_spellings() {
        assert!(to_bool(Some(&json!(true)), false));
        assert!(to_bool(Some(&json!("YES")), false));
        assert!(to_bool(Some(&json!(1)), false));
        assert!(!to_bool(Some(&json!("off")), true));
        assert!(!to_bool(Some(&json!(0)), true));
        assert!(to_bool(Some(&json!("maybe")), true));
        assert!(!to_bool(None, false));
    }

    #[test]
    fn to_int_should_clamp_to_range() {
        assert_eq!(to_int(Some(&json!(5000)), 1000, 200_000, 12_000), 5000);
        assert_eq!(to_int(Some(&json!(10)), 1000, 200_000, 12_000), 1000);
        assert_eq!(to_int(Some(&json!(999_999)), 1000, 200_000, 12_000), 200_000);
    }

    #[test]
    fn to_int_should_fallback_and_clamp_on_bad_input() {
        assert_eq!(to_int(Some(&json!("abc")), 1000, 200_000, 50), 1000);
        assert_eq!(to_int(Some(&json!("4200")), 1000, 200_000, 50), 4200);
        assert_eq!(to_int(Some(&json!(3.9)), 0, 100, 0), 3);
    }

    #[test]
    fn to_enum_should_match_case_insensitively() {
        #[derive(Copy, Clone, Debug, PartialEq, Eq)]
        enum Mode {
            Rewrite,
            Suggest,
        }
        let candidates = [("rewrite", Mode::Rewrite), ("suggest", Mode::Suggest)];
        assert_eq!(to_enum(Some(&json!("Rewrite")), &candidates, Mode::Suggest), Mode::Rewrite);
        assert_eq!(to_enum(Some(&json!(" SUGGEST ")), &candidates, Mode::Rewrite), Mode::Suggest);
        assert_eq!(to_enum(Some(&json!("other")), &candidates, Mode::Rewrite), Mode::Rewrite);
        assert_eq!(to_enum(Some(&json!(7)), &candidates, Mode::Rewrite), Mode::Rewrite);
    }
}