// linter.rs — linter 输出聚合：按规则与文件汇总问题数。
//
// 由 pi 版 pi-rtk-optimizer 的 src/techniques/linter.ts 移植。
//
// 与 pi 版的差异（性能）：`Issue` 的规则名/文件/消息改为借用输入 `&str`
// （原先每条问题 3 次 `String` 克隆），分组与排序表放进竞技场，
// 最终输出在竞技场里拼装一次。

use std::collections::BTreeMap;
use std::fmt::Write;
use std::sync::LazyLock;

use bumpalo::collections::{String as ArenaString, Vec as ArenaVec};
use bumpalo::Bump;
use regex::Regex;

use phi_ext_common::arena::split_lines;

use super::command_detection::matches_normalized_patterns;
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
static FILE_LINE_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(.+):(\d+):(\d+):\s*(.+)$").expect("file:line 正则应可编译"));

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

/// 严重级别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Severity {
    Error,
    Warning,
}

/// 单个问题（字段借用输入）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Issue<'a> {
    severity: Severity,
    rule: &'a str,
    file: &'a str,
}

/// 命令是否属于 linter 类。
///
/// `normalized_command` 必须是 [`normalize_command_for_detection`] 的结果。
pub fn is_linter_command(normalized_command: Option<&str>) -> bool {
    matches_normalized_patterns(normalized_command, &LINTER_COMMAND_PATTERNS)
}

/// 解析一行问题；无法识别时返回 `None`。
///
/// 两条模式都要求行内至少有一个冒号，且 Rust 风格模式必须整行以
/// `error` / `warning` 开头，因此先用零成本的字节检查快速排除——
/// linter 输出里大量缩进详情行会在这一步直接跳过，不必进正则。
fn parse_line(line: &str) -> Option<Issue<'_>> {
    if !line.contains(':') {
        return None;
    }

    if let Some(captures) = FILE_LINE_PATTERN.captures(line) {
        let file = captures.get(1).map_or("unknown", |m| m.as_str());
        let content = captures.get(4).map_or(line, |m| m.as_str());
        let severity = if WARNING_HINT.is_match(content) {
            Severity::Warning
        } else {
            Severity::Error
        };
        let rule = RULE_PATTERN
            .captures(content)
            .and_then(|caps| caps.get(1))
            .map_or("unknown", |m| m.as_str());
        return Some(Issue {
            severity,
            rule,
            file,
        });
    }

    if !(line.starts_with("error") || line.starts_with("warning")) {
        return None;
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
        return Some(Issue {
            severity,
            rule: "unknown",
            file: captures.get(3).map_or("unknown", |m| m.as_str()),
        });
    }

    None
}

/// 识别具体 linter 名称（用于输出抬头）。
fn detect_linter_type(normalized: Option<&str>) -> &'static str {
    let Some(normalized) = normalized else {
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
pub fn aggregate_linter_output(
    arena: &Bump,
    output: &str,
    normalized_command: Option<&str>,
) -> Option<String> {
    if !is_linter_command(normalized_command) {
        return None;
    }

    let linter_type = detect_linter_type(normalized_command);
    let lines = split_lines(arena, output);
    let mut issues: ArenaVec<Issue<'_>> = ArenaVec::new_in(arena);
    issues.extend(lines.iter().filter_map(|line| parse_line(line)));

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
    for issue in issues.iter() {
        *by_rule.entry(issue.rule).or_insert(0) += 1;
    }

    let mut by_file: BTreeMap<&str, ArenaVec<&Issue<'_>>> = BTreeMap::new();
    for issue in issues.iter() {
        by_file
            .entry(issue.file)
            .or_insert_with(|| ArenaVec::new_in(arena))
            .push(issue);
    }

    let mut out = ArenaString::new_in(arena);
    let _ = writeln!(
        out,
        "{linter_type}: {errors} errors, {warnings} warnings in {} files",
        by_file.len()
    );
    out.push_str("═══════════════════════════════════════\n");

    out.push_str("Top rules:\n");
    let mut rules: ArenaVec<(&str, usize)> = ArenaVec::new_in(arena);
    rules.extend(by_rule.iter().map(|(rule, count)| (*rule, *count)));
    rules.sort_by_key(|entry| std::cmp::Reverse(entry.1));
    for (rule, count) in rules.iter().take(10) {
        let _ = writeln!(out, "  {rule} ({count}x)");
    }

    out.push_str("\nTop files:\n");
    let mut files: ArenaVec<(&str, usize, &ArenaVec<&Issue<'_>>)> = ArenaVec::new_in(arena);
    files.extend(
        by_file
            .iter()
            .map(|(file, file_issues)| (*file, file_issues.len(), file_issues)),
    );
    files.sort_by_key(|entry| std::cmp::Reverse(entry.1));

    for (file, count, file_issues) in files.iter().take(10) {
        let _ = writeln!(out, "  {} ({count} issues)", compact_path(arena, file, 40));
        let mut file_rules: BTreeMap<&str, usize> = BTreeMap::new();
        for issue in file_issues.iter() {
            *file_rules.entry(issue.rule).or_insert(0) += 1;
        }
        let mut top_rules: ArenaVec<(&str, usize)> = ArenaVec::new_in(arena);
        top_rules.extend(file_rules.iter().map(|(rule, count)| (*rule, *count)));
        top_rules.sort_by_key(|entry| std::cmp::Reverse(entry.1));
        for (rule, rule_count) in top_rules.iter().take(3) {
            let _ = writeln!(out, "    {rule} ({rule_count})");
        }
    }

    Some(out.into_bump_str().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use phi_ext_common::arena::Scratch;

    fn aggregate(output: &str, command: Option<&str>) -> Option<String> {
        let scratch = Scratch::with_capacity(1024);
        aggregate_linter_output(scratch.arena(), output, command)
    }

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
        assert_eq!(
            aggregate("", Some("cargo clippy")).as_deref(),
            Some("[OK] Clippy: No issues found")
        );
    }

    #[test]
    fn eslint_style_lines_should_be_grouped() {
        let output = "\
src/a.ts:10:5: error: unexpected any [@typescript-eslint/no-explicit-any]\n\
src/a.ts:20:1: warning: unused var [no-unused-vars]\n\
src/b.ts:3:2: error: missing semicolon [semi]";
        let result = aggregate(output, Some("npx eslint src")).expect("应压缩");
        assert!(
            result.contains("ESLint: 2 errors, 1 warnings in 2 files"),
            "got {result}"
        );
        assert!(result.contains("no-explicit-any (1x)"), "got {result}");
        assert!(result.contains("src/a.ts (2 issues)"), "got {result}");
    }

    #[test]
    fn rust_style_lines_should_be_parsed() {
        let output =
            "error: unused variable `x` at src/main.rs:4:9\nwarning: unused import at src/lib.rs:1:1";
        let result = aggregate(output, Some("cargo clippy")).expect("应压缩");
        assert!(
            result.contains("Clippy: 1 errors, 1 warnings in 2 files"),
            "got {result}"
        );
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
    fn prefilter_should_not_drop_valid_lines() {
        // 含冒号但非 Rust 风格前缀的行仍按 file:line:col 解析。
        let result = aggregate(
            "src/a.ts:1:2: error: x [r]\nsrc/b.ts:3:4: warning: y [s]",
            Some("npx eslint src"),
        )
        .expect("应压缩");
        assert!(result.contains("ESLint: 1 errors, 1 warnings in 2 files"), "got {result}");
        // 无冒号的行被快速排除，但仍计入文件数以外的内容不产生问题
        let ignored = aggregate("no colons at all\nplain text", Some("cargo clippy")).expect("应压缩");
        assert_eq!(ignored, "[OK] Clippy: No issues found");
    }

    #[test]
    fn non_linter_command_should_return_none() {
        assert!(aggregate("x", Some("ls")).is_none());
    }

    #[test]
    fn many_issues_should_be_capped_at_ten_rules_and_files() {
        let mut output = String::new();
        for index in 0..20 {
            output.push_str(&format!("src/f{index}.rs:1:1: error: boom [rule{index}]\n"));
        }
        let result = aggregate(&output, Some("cargo clippy")).expect("应压缩");
        assert!(result.contains("20 errors, 0 warnings in 20 files"), "got {result}");
        assert_eq!(result.matches("issues)\n").count(), 10, "文件应截到 10 个");
        assert_eq!(result.matches("x)\n").count(), 10, "规则应截到 10 条");
    }
}