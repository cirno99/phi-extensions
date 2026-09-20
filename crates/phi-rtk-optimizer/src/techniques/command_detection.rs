// command_detection.rs — 从 shell 命令里提取「首个真实命令段」用于模式匹配。
//
// 由 pi 版 pi-rtk-optimizer 的 src/techniques/command-detection.ts 移植。
//
// 处理顺序：取第一行非空 → 去掉前导 `KEY=VALUE` 环境赋值 → 按
// `&&` / `||` / `;` / `|` 切出第一段 → 转小写。

use std::sync::LazyLock;

use regex::Regex;

/// 前导环境赋值：`FOO=bar `、`FOO="a b" `、`FOO='a b' `。
static ENV_PREFIX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"^(?:[A-Za-z_][A-Za-z0-9_]*=(?:"[^"]*"|'[^']*'|[^\s]+)\s+)*"#)
        .expect("环境前缀正则应可编译")
});

/// 命令链操作符（按 pi 版的顺序）。
const CHAIN_OPERATORS: [&str; 4] = ["&&", "||", ";", "|"];

/// 截到第一个链操作符之前。
fn slice_first_segment(command: &str) -> &str {
    let mut cut: Option<usize> = None;
    for operator in CHAIN_OPERATORS {
        if let Some(index) = command.find(operator) {
            cut = Some(cut.map_or(index, |current| current.min(index)));
        }
    }
    match cut {
        Some(index) => &command[..index],
        None => command,
    }
}

/// 归一化命令用于模式匹配；无法提取时返回 `None`。
pub fn normalize_command_for_detection(command: Option<&str>) -> Option<String> {
    let command = command?;
    let first_line = command
        .split('\n')
        .map(str::trim)
        .find(|line| !line.is_empty())?;

    let without_env = ENV_PREFIX.replace(first_line, "");
    let without_env = without_env.trim();
    if without_env.is_empty() {
        return None;
    }

    let segment = slice_first_segment(without_env).trim().to_lowercase();
    if segment.is_empty() {
        None
    } else {
        Some(segment)
    }
}

/// 命令是否命中任一模式。
pub fn matches_command_patterns(command: Option<&str>, patterns: &[Regex]) -> bool {
    match normalize_command_for_detection(command) {
        Some(normalized) => patterns.iter().any(|pattern| pattern.is_match(&normalized)),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_should_strip_env_prefix_and_chain() {
        assert_eq!(
            normalize_command_for_detection(Some("FOO=1 BAR='x y' cargo build --release && echo ok")),
            Some("cargo build --release".to_string())
        );
    }

    #[test]
    fn normalize_should_take_first_non_empty_line() {
        assert_eq!(
            normalize_command_for_detection(Some("\n  \n  git status\n")),
            Some("git status".to_string())
        );
    }

    #[test]
    fn normalize_should_slice_at_any_chain_operator() {
        assert_eq!(
            normalize_command_for_detection(Some("cargo test | tee log")),
            Some("cargo test".to_string())
        );
        assert_eq!(
            normalize_command_for_detection(Some("make; make install")),
            Some("make".to_string())
        );
    }

    #[test]
    fn normalize_should_lowercase() {
        assert_eq!(
            normalize_command_for_detection(Some("Cargo BUILD")),
            Some("cargo build".to_string())
        );
    }

    #[test]
    fn normalize_should_return_none_for_blank_input() {
        assert_eq!(normalize_command_for_detection(None), None);
        assert_eq!(normalize_command_for_detection(Some("")), None);
        assert_eq!(normalize_command_for_detection(Some("   ")), None);
        // 环境前缀需要「值 + 空白」才会被剥离；裸赋值会被当作命令本身（与 pi 版一致）。
        assert_eq!(
            normalize_command_for_detection(Some("FOO=1 ")),
            Some("foo=1".to_string())
        );
    }

    #[test]
    fn matches_should_apply_patterns_to_normalized_command() {
        static CARGO_BUILD: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"^cargo\s+(build|check)\b").unwrap());
        let patterns = [CARGO_BUILD.clone()];
        assert!(matches_command_patterns(Some("cargo build -q"), &patterns));
        assert!(matches_command_patterns(Some("cargo check"), &patterns));
        assert!(!matches_command_patterns(Some("cargo test"), &patterns));
        assert!(!matches_command_patterns(None, &patterns));
    }
}