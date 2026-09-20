// linter.rs — linter 输出聚合：按规则与文件汇总问题数。
//
// 由 pi 版 pi-rtk-optimizer 的 src/techniques/linter.ts 移植。

use std::collections::BTreeMap;
use std::sync::LazyLock;

use regex::Regex;

use super::command_detection::{matches_command_patterns, normalize_command_for_detection};
use super::path_utils::compact_path;

/// linter 类命令。
static LINTER_COMMAND_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r"^(?:pnpm\s+)?(?:npx\s+)?eslint\b",
        r"^(?:npx\s+)?prettier\b",
        r"^ruff\b",
        r"^pylint\b",
        r"^mypy\b",
        r"^flake8\b",
        r"^black\b",
        r"^cargo\s+clippy\b",
        r"^golangci-lint\b",
        r"^zlint\b",
    ]
    .iter()
    .map(|p| Regex::new(p).expect("linter 命令正则应可编译"))
    .collect()
});

/// `file:line:col: message` 形式。
static FILE_LINE_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(.+):(\d+):(\d+):\s*(.+)$").expect("file:line 正则应可编译")
});

/// Rust 编译器风格：`error: msg at file:line:col`。
static RUST_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(error|warning):\s*(.+?)\s+at\s+(.+):(\d+):(\d+)$")
        .expect("rust 风格正则应可编译")
});

/// 行尾 `[rule-name]`。
static RULE_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[(.+?)\]$").expect("规则正则应可编译"));

/// 含 `warning` 即判为警告。
static WARNING_HINT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)warning").expect("警告提示正则应可编译"));

/// 单个问题。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Issue {
    severity: Severity,
    rule: String,
    file: String,
    message: String,
}

/// 严重级别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Severity {
    Error,
    Warning,
}

/// 命令是否属于 linter 类。
pub fn is_linter_command(command: Option<&str>) -> bool {
    matches_command_patterns(command, &LINTER_COMMAND_PATTERNS)
}

/// 解析一行问题；无法识别时返回 `None`。
fn parse_line(line: &str) -> Option<Issue> {
    if let Some(captures) = FILE_LINE_PATTERN.captures(line) {
        let file = captures.get(1).map_or("unknown", |m| m.as_str()).to_string();
        let content = captures
            .get(4)
            .map_or_else(|| line.to_string(), |m| m.as_str().to_string());
        let severity = if WARNING_HINT.is_match(&content) {
            Severity::Warning
        } else {
            Severity::Error
        };
        let rule = RULE_PATTERN
            .captures(&content)
            .and_then(|caps| caps.get(1))
            .map_or_else(|| "unknown".to_string(), |m| m.as_str().to_string());
        return Some(Issue {
            severity,
            rule,
            file,
            message: content,
        });
    }

    if let Some(captures) = RUST_PATTERN.captures(line) {
        let severity = if captures
            .get(1)
            .is_some_and(|m| m.as_str().eq_ignore_ascii_case("warning"))
        {
            Severity::Warning
        } else {
            Severity::Error
        };
        let message = captures
            .get(2)
            .map_or_else(|| line.to_string(), |m| m.as_str().to_string());
        let file = captures.get(3).map_or("unknown", |m| m.as_str()).to_string();
        return Some(Issue {
            severity,
            rule: "unknown".to_string(),
            file,
            message,
        });
    }

    None
}

/// 识别具体 linter 名称（用于输出抬头）。
fn detect_linter_type(command: Option<&str>) -> &'static str {
    let Some(normalized) = normalize_command_for_detection(command) else {
        return "Linter";
    };
    if normalized.contains("eslint") {
        return "ESLint";
    }
    if normalized.starts_with("ruff") {
        return "Ruff";
    }
    if normalized.starts_with("pylint") {
        return "Pylint";
    }
    if normalized.starts_with("mypy") {
        return "MyPy";
    }
    if normalized.starts_with("flake8") {
        return "Flake8";
    }
    if normalized.contains("clippy") {
        return "Clippy";
    }
    if normalized.starts_with("golangci-lint") {
        return "GolangCI-Lint";
    }
    if normalized.contains("prettier") {
        return "Prettier";
    }
    if normalized.contains("zlint") {
        return "ZLint";
    }
    "Linter"
}

/// 聚合 linter 输出；非 linter 命令返回 `None`。
pub fn aggregate_linter_output(output: &str, command: Option<&str>) -> Option<String> {
    if !is_linter_command(command) {
        return None;
    }

    let linter_type = detect_linter_type(command);
    let issues: Vec<Issue> = output.split('\n').filter_map(parse_line).collect();

    if issues.is_empty() {
        return Some(format!("[OK] {linter_type}: No issues found"));
    }

    let errors = issues
        .iter()
        .filter(|issue| issue.severity == Severity::Error)
        .count();
    let warnings = issues
        .iter()
        .filter(|issue| issue.severity == Severity::Warning)
        .count();

    let mut by_rule: BTreeMap<&str, usize> = BTreeMap::new();
    for issue in &issues {
        *by_rule.entry(issue.rule.as_str()).or_insert(0) += 1;
    }

    let mut by_file: BTreeMap<&str, Vec<&Issue>> = BTreeMap::new();
    for issue in &issues {
        by_file.entry(issue.file.as_str()).or_default().push(issue);
    }

    let mut result = format!(
        "{linter_type}: {errors} errors, {warnings} warnings in {} files\n",
        by_file.len()
    );
    result.push_str("═══════════════════════════════════════\n");

    result.push_str("Top rules:\n");
    let mut rules: Vec<(&str, usize)> = by_rule.into_iter().collect();
    rules.sort_by_key(|entry| std::cmp::Reverse(entry.1));
    for (rule, count) in rules.iter().take(10) {
        result.push_str(&format!("  {rule} ({count}x)\n"));
    }

    result.push_str("\nTop files:\n");
    let mut files: Vec<(&str, Vec<&Issue>)> = by_file.into_iter().collect();
    files.sort_by_key(|entry| std::cmp::Reverse(entry.1.len()));
    for (file, file_issues) in files.iter().take(10) {
        result.push_str(&format!(
            "  {} ({} issues)\n",
            compact_path(file, 40),
            file_issues.len()
        ));
        let mut file_rules: BTreeMap<&str, usize> = BTreeMap::new();
        for issue in file_issues {
            *file_rules.entry(issue.rule.as_str()).or_insert(0) += 1;
        }
        let mut top_rules: Vec<(&str, usize)> = file_rules.into_iter().collect();
        top_rules.sort_by_key(|entry| std::cmp::Reverse(entry.1));
        for (rule, count) in top_rules.iter().take(3) {
            result.push_str(&format!("    {rule} ({count})\n"));
        }
    }

    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_linter_command_should_match_common_linters() {
        assert!(is_linter_command(Some("cargo clippy --all-targets")));
        assert!(is_linter_command(Some("npx eslint src")));
        assert!(is_linter_command(Some("ruff check .")));
        assert!(is_linter_command(Some("zlint src")));
        assert!(!is_linter_command(Some("zig build")));
        assert!(!is_linter_command(Some("cargo build")));
        assert!(!is_linter_command(None));
    }

    #[test]
    fn no_issues_should_report_ok() {
        let result = aggregate_linter_output("", Some("cargo clippy")).expect("应压缩");
        assert_eq!(result, "[OK] Clippy: No issues found");
    }

    #[test]
    fn eslint_style_lines_should_be_grouped() {
        let output = "\
src/a.ts:10:5: error: unexpected any [@typescript-eslint/no-explicit-any]\n\
src/a.ts:20:1: warning: unused var [no-unused-vars]\n\
src/b.ts:3:2: error: missing semicolon [semi]";
        let result = aggregate_linter_output(output, Some("npx eslint src")).expect("应压缩");
        assert!(result.contains("ESLint: 2 errors, 1 warnings in 2 files"), "got {result}");
        assert!(result.contains("no-explicit-any (1x)"), "got {result}");
        assert!(result.contains("src/a.ts (2 issues)"), "got {result}");
    }

    #[test]
    fn rust_style_lines_should_be_parsed() {
        let output = "error: unused variable `x` at src/main.rs:4:9\nwarning: unused import at src/lib.rs:1:1";
        let result = aggregate_linter_output(output, Some("cargo clippy")).expect("应压缩");
        assert!(result.contains("Clippy: 1 errors, 1 warnings in 2 files"), "got {result}");
    }

    #[test]
    fn detect_linter_type_should_name_known_tools() {
        assert_eq!(detect_linter_type(Some("cargo clippy")), "Clippy");
        assert_eq!(detect_linter_type(Some("ruff check")), "Ruff");
        assert_eq!(detect_linter_type(Some("npx eslint .")), "ESLint");
        assert_eq!(detect_linter_type(Some("zlint src")), "ZLint");
        assert_eq!(detect_linter_type(Some("unknown-tool")), "Linter");
    }

    #[test]
    fn non_linter_command_should_return_none() {
        assert!(aggregate_linter_output("x", Some("ls")).is_none());
    }
}