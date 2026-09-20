//! phi 扩展目录解析。
//!
//! phi 从 `~/.phi/extensions/<name>/`（或 `<cwd>/.phi/extensions/<name>/`）
//! 发现扩展，扩展自身的可写数据（配置、状态）也约定放在同一目录下。

use std::path::{Path, PathBuf};

/// phi 主目录。
///
/// 优先使用 `PHI_HOME`，否则回退到 `$HOME/.phi`。当 `HOME` 也不可用时
/// 返回当前目录下的 `.phi`，保证函数永不失败。
pub fn phi_home() -> PathBuf {
    if let Some(home) = env_non_empty("PHI_HOME") {
        return PathBuf::from(home);
    }
    match env_non_empty("HOME") {
        Some(home) => Path::new(&home).join(".phi"),
        None => PathBuf::from(".phi"),
    }
}

/// 所有 phi 扩展的根目录：`<phi_home>/extensions`。
pub fn extensions_root() -> PathBuf {
    phi_home().join("extensions")
}

/// 指定扩展的数据目录：`<phi_home>/extensions/<name>`。
pub fn extension_dir(name: &str) -> PathBuf {
    extensions_root().join(name)
}

/// 指定扩展的配置文件路径：`<phi_home>/extensions/<name>/config.json`。
pub fn extension_config_path(name: &str) -> PathBuf {
    extension_dir(name).join("config.json")
}

/// 指定扩展的状态目录：`<phi_home>/extensions/<name>/state`。
pub fn extension_state_dir(name: &str) -> PathBuf {
    extension_dir(name).join("state")
}

/// 复刻 Go 侧 `project.ProjectDirName`（`internal/project/session_dir.go`）：
/// 去掉末尾路径分隔符，去掉前导分隔符，把 `/ \ :` 换成 `-`，两边包 `--`。
///
/// 例：`/home/cirno99/Code/Rust/phi-extensions` →
/// `--home-cirno99-Code-Rust-phi-extensions--`。
///
/// acp 与 cache-optimizer 都需要据此定位宿主持久化的会话目录，因此收敛到一处。
pub fn project_dir_name(cwd: &str) -> String {
    // 近似 `filepath.Clean`：去掉末尾分隔符（根路径除外）。
    let mut cleaned = cwd.trim();
    while cleaned.len() > 1 && (cleaned.ends_with('/') || cleaned.ends_with('\\')) {
        cleaned = &cleaned[..cleaned.len() - 1];
    }
    if cleaned.is_empty() || cleaned == "." {
        return "--unknown--".to_string();
    }
    let trimmed = cleaned.strip_prefix(['/', '\\']).unwrap_or(cleaned);
    let mut out = String::with_capacity(trimmed.len() + 4);
    for ch in trimmed.chars() {
        match ch {
            '/' | '\\' | ':' => out.push('-'),
            other => out.push(other),
        }
    }
    if out.is_empty() {
        out.push_str("unknown");
    }
    format!("--{out}--")
}

/// 创建目录（含父目录）；目录已存在时视为成功。
pub fn ensure_dir(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)
}

/// 读取非空环境变量。
fn env_non_empty(key: &str) -> Option<String> {
    let value = std::env::var(key).ok()?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_dir_should_append_name_to_extensions_root() {
        let dir = extension_dir("phi-demo");
        assert!(dir.ends_with("extensions/phi-demo"));
    }

    #[test]
    fn extension_config_path_should_end_with_config_json() {
        let path = extension_config_path("phi-demo");
        assert!(path.ends_with("phi-demo/config.json"));
    }

    #[test]
    fn project_dir_name_should_match_host_encoding() {
        assert_eq!(
            project_dir_name("/home/cirno99/Code/Rust/phi-extensions"),
            "--home-cirno99-Code-Rust-phi-extensions--"
        );
        assert_eq!(project_dir_name("/Users/foo/Phi/"), "--Users-foo-Phi--");
        assert_eq!(project_dir_name("/home/me/proj"), "--home-me-proj--");
        // 根路径、空串、`.` 都落到 unknown。
        assert_eq!(project_dir_name("/"), "--unknown--");
        assert_eq!(project_dir_name(""), "--unknown--");
        assert_eq!(project_dir_name("."), "--unknown--");
    }

    #[test]
    fn ensure_dir_should_be_idempotent() {
        let dir = std::env::temp_dir().join("phi-ext-common-test-ensure-dir");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(ensure_dir(&dir).is_ok());
        assert!(ensure_dir(&dir).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
