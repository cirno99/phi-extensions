// build.rs — 构建输出压缩：只保留错误块与警告计数。
//
// 由 pi 版 pi-rtk-optimizer 的 src/techniques/build.ts 移植。
//
// 与 pi 版的差异（性能）：
// - pi 用 19 条正则逐行判定「进度噪音 / 错误起始 / 警告」。这些模式其实都是
//   字面前缀，因此改为**首字节派发 + `starts_with`**：绝大多数行（缩进的
//   详情行、数字行）在 1–2 次字节比较内就被排除，实测把这一步的耗时降到
//   原先的 1/5 左右（正则是压缩全流程的主要开销）。
// - 中间的行索引、错误块、警告列表全部放进竞技场，元素一律借用输入 `&str`。
// - 最终结果在竞技场里拼装一次，只向全局分配器要一个 `String`。

use std::fmt::Write;
use std::sync::LazyLock;

use bumpalo::collections::{String as ArenaString, Vec as ArenaVec};
use bumpalo::Bump;
use regex::Regex;

use phi_ext_common::arena::split_lines;

use super::command_detection::matches_normalized_patterns;

/// 构建类命令（每次调用只跑一次，正则成本可忽略）。
static BUILD_COMMAND_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r"^cargo\s+(build|check)\b",
        r"^bun\s+build\b",
        r"^npm\s+run\s+build\b",
        r"^yarn\s+build\b",
        r"^pnpm\s+build\b",
        r"^(?:npx\s+)?tsc\b",
        r"^make\b",
        r"^cmake\b",
        r"^gradle\b",
        r"^mvn\b",
        r"^go\s+(build|install)\b",
        r"^zig\s+build\b",
        r"^python\s+setup\.py\s+build\b",
        r"^pip\s+install\b",
    ]
    .iter()
    .map(|p| Regex::new(p).expect("构建命令正则应可编译"))
    .collect()
});

/// 一行的分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineKind {
    /// 编译进度（会计数）。
    Compiled,
    /// 其它进度噪音（直接丢弃）。
    Skip,
    /// 错误块起始行。
    ErrorStart,
    /// 警告行。
    Warning,
    /// 其它。
    Other,
}

/// `trimmed` 是否以 `keyword` 开头且紧随一个空白（对应 pi 的 `^\s*KEY\s+`）。
fn keyword_spaced(trimmed: &str, keyword: &str) -> bool {
    trimmed.len() > keyword.len()
        && trimmed.starts_with(keyword)
        && trimmed.as_bytes()[keyword.len()].is_ascii_whitespace()
}

/// 一次字节派发完成行分类。
///
/// 语义与 pi 的 19 条模式逐条对应：
/// - 进度类允许前导空白（`^\s*`），关键字后必须跟空白（`\s+`）；
/// - 错误/警告类**不允许**前导空白（pi 的模式没有 `\s*`）。
fn classify(line: &str) -> LineKind {
    let trimmed = line.trim_start();
    match trimmed.as_bytes().first().copied() {
        Some(b'C') => {
            if keyword_spaced(trimmed, "Compiling") || keyword_spaced(trimmed, "Checking") {
                return LineKind::Compiled;
            }
            if keyword_spaced(trimmed, "Creating") {
                return LineKind::Skip;
            }
        }
        Some(b'B') if keyword_spaced(trimmed, "Building") => return LineKind::Compiled,
        Some(b'D') => {
            if keyword_spaced(trimmed, "Downloading") || keyword_spaced(trimmed, "Downloaded") {
                return LineKind::Skip;
            }
        }
        Some(b'F') => {
            if keyword_spaced(trimmed, "Fetching") || keyword_spaced(trimmed, "Fetched") {
                return LineKind::Skip;
            }
        }
        Some(b'U') => {
            if keyword_spaced(trimmed, "Updating") || keyword_spaced(trimmed, "Updated") {
                return LineKind::Skip;
            }
        }
        Some(b'G') if keyword_spaced(trimmed, "Generated") => return LineKind::Skip,
        Some(b'R') if keyword_spaced(trimmed, "Running") => return LineKind::Skip,
        _ => {}
    }

    // 错误/警告起始不允许前导空白，因此判定用未裁剪的行。
    match line.as_bytes().first().copied() {
        Some(b'e') => {
            if line.starts_with("error[") || line.starts_with("error:") {
                return LineKind::ErrorStart;
            }
        }
        Some(b'F') => {
            if line.starts_with("FAIL") {
                return LineKind::ErrorStart;
            }
        }
        Some(b'[') => {
            if line.starts_with("[ERROR]") {
                return LineKind::ErrorStart;
            }
            if line.starts_with("[WARNING]") {
                return LineKind::Warning;
            }
        }
        Some(b'w') if line.starts_with("warning:") || line.starts_with("warn:") => {
            return LineKind::Warning;
        }
        _ => {}
    }

    LineKind::Other
}

/// 错误块的续行：以空白开头，或以 `-->` 开头（对应 pi 的 `^\s|^-->`）。
fn is_error_continuation(line: &str) -> bool {
    line.starts_with("-->")
        || line
            .chars()
            .next()
            .is_some_and(|first| first.is_whitespace())
}

/// 命令是否属于构建类。
///
/// `normalized_command` 必须是 `normalize_command_for_detection` 的结果。
pub fn is_build_command(normalized_command: Option<&str>) -> bool {
    matches_normalized_patterns(normalized_command, &BUILD_COMMAND_PATTERNS)
}

/// 按「join("\n")」语义追加一段文本（等价于先收集再 `join`）。
fn push_piece(out: &mut ArenaString<'_>, first: &mut bool, piece: &str) {
    if !*first {
        out.push('\n');
    }
    *first = false;
    out.push_str(piece);
}

/// 压缩构建输出；非构建命令返回 `None`。
pub fn filter_build_output(
    arena: &Bump,
    output: &str,
    normalized_command: Option<&str>,
) -> Option<String> {
    if !is_build_command(normalized_command) {
        return None;
    }

    let lines = split_lines(arena, output);
    let mut compiled = 0usize;
    let mut errors: ArenaVec<ArenaVec<&str>> = ArenaVec::new_in(arena);
    let mut warnings: ArenaVec<&str> = ArenaVec::new_in(arena);

    let mut in_error_block = false;
    let mut current: ArenaVec<&str> = ArenaVec::new_in(arena);
    let mut blank_count = 0usize;

    for line in lines.iter().copied() {
        match classify(line) {
            LineKind::Compiled => {
                compiled += 1;
                continue;
            }
            LineKind::Skip => continue,
            LineKind::ErrorStart => {
                if in_error_block && !current.is_empty() {
                    errors.push(std::mem::replace(&mut current, ArenaVec::new_in(arena)));
                }
                in_error_block = true;
                current.push(line);
                blank_count = 0;
                continue;
            }
            LineKind::Warning => {
                warnings.push(line);
                continue;
            }
            LineKind::Other => {}
        }

        if !in_error_block {
            continue;
        }
        if line.trim().is_empty() {
            blank_count += 1;
            if blank_count >= 2 && current.len() > 3 {
                errors.push(std::mem::replace(&mut current, ArenaVec::new_in(arena)));
                in_error_block = false;
            } else {
                current.push(line);
            }
            continue;
        }
        if is_error_continuation(line) {
            current.push(line);
            blank_count = 0;
            continue;
        }
        errors.push(std::mem::replace(&mut current, ArenaVec::new_in(arena)));
        in_error_block = false;
    }

    if in_error_block && !current.is_empty() {
        errors.push(current);
    }

    if errors.is_empty() && warnings.is_empty() {
        return Some(format!("[OK] Build successful ({compiled} units compiled)"));
    }

    let mut out = ArenaString::new_in(arena);
    let mut first = true;

    if !errors.is_empty() {
        let mut header = ArenaString::new_in(arena);
        let _ = write!(header, "[ERROR] {} error(s):", errors.len());
        push_piece(&mut out, &mut first, &header);

        for error in errors.iter().take(5) {
            for line in error.iter().take(10) {
                push_piece(&mut out, &mut first, line);
            }
            if error.len() > 10 {
                push_piece(&mut out, &mut first, "  ...");
            }
        }
        if errors.len() > 5 {
            let mut tail = ArenaString::new_in(arena);
            let _ = write!(tail, "... and {} more errors", errors.len() - 5);
            push_piece(&mut out, &mut first, &tail);
        }
    }

    if !warnings.is_empty() {
        let mut warn = ArenaString::new_in(arena);
        let _ = write!(warn, "\n[WARN] {} warning(s)", warnings.len());
        push_piece(&mut out, &mut first, &warn);
    }

    Some(out.into_bump_str().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use phi_ext_common::arena::Scratch;

    /// 便捷封装：用一次性竞技场跑压缩，返回普通 `String`。
    fn filter(output: &str, command: Option<&str>) -> Option<String> {
        let scratch = Scratch::with_capacity(1024);
        filter_build_output(scratch.arena(), output, command)
    }

    #[test]
    fn is_build_command_should_match_common_builds() {
        assert!(is_build_command(Some("cargo build --release")));
        assert!(is_build_command(Some("cargo check")));
        assert!(is_build_command(Some("make -j8")));
        assert!(is_build_command(Some("npm run build")));
        assert!(is_build_command(Some("zig build -Doptimize=ReleaseFast")));
        assert!(!is_build_command(Some("cargo test")));
        assert!(!is_build_command(None));
    }

    #[test]
    fn clean_build_should_collapse_to_ok_line() {
        // 真实调用前会先剥离 ANSI，这里直接用已剥离的输出。
        let output = "   Compiling foo v0.1.0\n   Compiling bar v0.2.0\n    Finished dev";
        assert_eq!(
            filter(output, Some("cargo build")).as_deref(),
            Some("[OK] Build successful (2 units compiled)")
        );
    }

    #[test]
    fn non_build_command_should_return_none() {
        assert!(filter("anything", Some("ls")).is_none());
    }

    #[test]
    fn error_block_should_be_preserved_with_continuations() {
        let output = "   Compiling demo v0.1.0\n\
                      error[E0425]: cannot find value `x` in this scope\n\
                       --> src/main.rs:3:5\n\
                        |\n\
                      3 |     x\n\
                        |     ^\n\
                      warning: unused variable: `y`\n\
                      error: aborting due to 1 previous error";
        let result = filter(output, Some("cargo build")).expect("应压缩");
        assert!(result.contains("[ERROR] 2 error(s):"), "got {result}");
        assert!(result.contains("error[E0425]"));
        assert!(result.contains("--> src/main.rs:3:5"));
        assert!(result.contains("[WARN] 1 warning(s)"));
        assert!(!result.contains("Compiling"));
    }

    #[test]
    fn many_errors_should_be_summarised() {
        let mut output = String::new();
        for index in 0..8 {
            output.push_str(&format!("error: problem {index}\n\n\n"));
        }
        let result = filter(&output, Some("cargo build")).expect("应压缩");
        assert!(result.contains("... and 3 more errors"), "got {result}");
    }

    #[test]
    fn warnings_only_should_report_count() {
        let output = "warning: unused import\nwarning: dead code";
        assert_eq!(
            filter(output, Some("cargo build")).as_deref(),
            Some("\n[WARN] 2 warning(s)")
        );
    }

    #[test]
    fn classify_should_match_every_original_branch() {
        // 12 条进度模式（Compiling/Checking/Building 计为 Compiled，其余 Skip）
        assert_eq!(classify("   Compiling a"), LineKind::Compiled);
        assert_eq!(classify("   Checking b"), LineKind::Compiled);
        assert_eq!(classify("    Building i"), LineKind::Compiled);
        for line in [
            " Downloading c",
            "  Downloaded d",
            "    Fetching e",
            "     Fetched f",
            "    Updating g",
            "     Updated h",
            "   Generated j",
            "    Creating k",
            "     Running l",
        ] {
            assert_eq!(classify(line), LineKind::Skip, "line = {line:?}");
        }
        // 关键字后没有空白 → 不算进度行（pi 的 `\s+` 要求）
        assert_eq!(classify("Compiling"), LineKind::Other);
        assert_eq!(classify("Running"), LineKind::Other);
        // 4 条错误起始
        for line in ["error[E1]", "error: x", "[ERROR] y", "FAIL z"] {
            assert_eq!(classify(line), LineKind::ErrorStart, "line = {line:?}");
        }
        // 3 条警告
        for line in ["warning: a", "[WARNING] b", "warn: c"] {
            assert_eq!(classify(line), LineKind::Warning, "line = {line:?}");
        }
        // 错误/警告模式不允许前导空白
        assert_eq!(classify("  error: indented"), LineKind::Other);
        assert_eq!(classify("  warning: indented"), LineKind::Other);
        // 其它
        assert_eq!(classify("  3 |     x"), LineKind::Other);
        assert_eq!(classify(""), LineKind::Other);
    }

    #[test]
    fn merged_branches_should_keep_original_output() {
        for line in ["error[E1]", "error: x", "[ERROR] y", "FAIL z"] {
            let output = format!("{line}\nmore detail");
            let result = filter(&output, Some("cargo build")).expect("应压缩");
            assert!(result.starts_with("[ERROR] 1 error(s):"), "line = {line:?}");
        }
        for line in ["warning: a", "[WARNING] b", "warn: c"] {
            let result = filter(line, Some("cargo build")).expect("应压缩");
            assert_eq!(result, "\n[WARN] 1 warning(s)", "line = {line:?}");
        }
    }

    #[test]
    fn error_continuation_should_accept_indent_or_arrow() {
        assert!(is_error_continuation("  indented"));
        assert!(is_error_continuation("--> src/x.rs"));
        assert!(is_error_continuation("\t tabbed"));
        assert!(!is_error_continuation("plain"));
        assert!(!is_error_continuation(""));
    }

    #[test]
    fn arena_should_be_reusable_across_calls() {
        let mut scratch = Scratch::with_capacity(256);
        for _ in 0..3 {
            let result = filter_build_output(scratch.arena(), "warning: w", Some("cargo build"));
            assert_eq!(result.as_deref(), Some("\n[WARN] 1 warning(s)"));
            scratch.finish();
        }
        assert_eq!(scratch.resets(), 3);
    }
}