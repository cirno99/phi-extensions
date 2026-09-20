// test_output.rs — 测试输出压缩：抽取 PASS/FAIL/SKIP 计数与失败摘要。
//
// 由 pi 版 pi-rtk-optimizer 的 src/techniques/test-output.ts 移植。
//
// 与 pi 版的差异：
// - （有意修正）pi 第一条「test result:」模式把 `(\w+)` 当作 passed 分组，
//   导致 `test result: ok. 5 passed; 0 failed;` 被解析成 passed=0 / failed=5。
//   本实现按模式分别声明分组下标，取正确的数字分组。
// - （性能）行索引与失败块放进竞技场，元素一律借用输入 `&str`；
//   仅在需要截断时才在竞技场里生成新字符串。

use std::fmt::Write;
use std::sync::LazyLock;

use bumpalo::collections::{String as ArenaString, Vec as ArenaVec};
use bumpalo::Bump;
use regex::Regex;

use phi_ext_common::arena::split_lines;

use super::command_detection::matches_normalized_patterns;

/// 测试类命令。
static TEST_COMMAND_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r"^npm\s+test\b",
        r"^pnpm\s+test\b",
        r"^yarn\s+test\b",
        r"^bun\s+test\b",
        r"^cargo\s+test\b",
        r"^go\s+test\b",
        r"^zig\s+test\b",
        r"^zig\s+build\s+test\b",
        r"^pytest\b",
        r"^python\s+-m\s+pytest\b",
        r"^(?:pnpm\s+)?(?:npx\s+)?vitest\b",
        r"^(?:npx\s+)?jest\b",
        r"^mocha\b",
        r"^ava\b",
        r"^tap\b",
    ]
    .iter()
    .map(|p| Regex::new(p).expect("测试命令正则应可编译"))
    .collect()
});

/// 一条结果行模式及其数字分组下标。
struct ResultPattern {
    regex: Regex,
    passed_group: usize,
    failed_group: Option<usize>,
    skipped_group: Option<usize>,
}

/// 结果行模式（按 pi 版顺序，首个命中者胜出）。
static TEST_RESULT_PATTERNS: LazyLock<Vec<ResultPattern>> = LazyLock::new(|| {
    let pattern = |source: &str,
                   passed_group: usize,
                   failed_group: Option<usize>,
                   skipped_group: Option<usize>| ResultPattern {
        regex: Regex::new(source).expect("测试结果正则应可编译"),
        passed_group,
        failed_group,
        skipped_group,
    };
    vec![
        pattern(
            r"test result:\s*(\w+)\.\s*(\d+)\s*passed;\s*(\d+)\s*failed;",
            2,
            Some(3),
            None,
        ),
        // Zig 测试运行器用分号分隔：`1 passed; 1 failed.`
        // 必须排在逗号版本之前，否则会只匹配到 passed 而丢掉 failed。
        pattern(r"(?i)(\d+)\s*passed;\s*(\d+)\s*failed", 1, Some(2), None),
        pattern(
            r"(?i)(\d+)\s*passed(?:,\s*(\d+)\s*failed)?(?:,\s*(\d+)\s*skipped)?",
            1,
            Some(2),
            Some(3),
        ),
        pattern(
            r"(?i)(\d+)\s*pass(?:,\s*(\d+)\s*fail)?(?:,\s*(\d+)\s*skip)?",
            1,
            Some(2),
            Some(3),
        ),
        pattern(
            r"(?i)tests?:\s*(\d+)\s*passed(?:,\s*(\d+)\s*failed)?(?:,\s*(\d+)\s*skipped)?",
            1,
            Some(2),
            Some(3),
        ),
        // Zig 测试运行器：`All 3 tests passed.`
        pattern(r"(?i)all\s+(\d+)\s+tests?\s+passed", 1, None, None),
    ]
});

/// 失败块起始行。
static FAILURE_START_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r"^FAIL\s+",
        r"^FAILED\s+",
        r"^\s*●\s+",
        r"^\s*✕\s+",
        r"test\s+\w+\s+\.\.\.\s*FAILED",
        r"thread\s+'\w+'\s+panicked",
        // Zig 测试运行器：`1/3 test.foo... FAIL (TestUnexpectedResult)`
        r"^\s*\d+/\d+\s+\S+.*\.\.\.\s*FAIL\b",
    ]
    .iter()
    .map(|p| Regex::new(p).expect("失败行正则应可编译"))
    .collect()
});

/// 逐行兜底：统计通过标记。
static FALLBACK_PASS_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:\b(?:ok|PASS)\b|[✓✔])").expect("通过标记正则应可编译")
});

/// 逐行兜底：统计失败标记。
static FALLBACK_FAIL_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:\b(?:FAIL|fail)\b|[✗✕])").expect("失败标记正则应可编译")
});

/// 失败块的续行（缩进行或 `-` 开头）。
static FAILURE_CONTINUATION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s|^-").expect("续行正则应可编译"));

/// 命令是否属于测试类。
///
/// `normalized_command` 必须是 [`normalize_command_for_detection`] 的结果。
pub fn is_test_command(normalized_command: Option<&str>) -> bool {
    matches_normalized_patterns(normalized_command, &TEST_COMMAND_PATTERNS)
}

/// 从输出中抽取 (passed, failed, skipped)。
fn extract_test_stats(output: &str) -> Option<(u64, u64, u64)> {
    for pattern in TEST_RESULT_PATTERNS.iter() {
        let Some(captures) = pattern.regex.captures(output) else {
            continue;
        };
        let number = |index: Option<usize>| -> u64 {
            index
                .and_then(|index| captures.get(index))
                .and_then(|group| group.as_str().parse::<u64>().ok())
                .unwrap_or(0)
        };
        return Some((
            number(Some(pattern.passed_group)),
            number(pattern.failed_group),
            number(pattern.skipped_group),
        ));
    }
    None
}

/// 按「join("\n")」语义追加一段文本。
fn push_piece(out: &mut ArenaString<'_>, first: &mut bool, piece: &str) {
    if !*first {
        out.push('\n');
    }
    *first = false;
    out.push_str(piece);
}

/// 截断到 `max` 个字符；未超长时直接借用输入，否则在竞技场里生成。
fn cut<'a>(arena: &'a Bump, text: &'a str, max: usize) -> &'a str {
    for (taken, (index, _)) in text.char_indices().enumerate() {
        if taken == max {
            let mut out = ArenaString::new_in(arena);
            out.push_str(&text[..index]);
            out.push_str("...");
            return out.into_bump_str();
        }
    }
    text
}

/// 压缩测试输出；非测试命令返回 `None`。
pub fn aggregate_test_output(
    arena: &Bump,
    output: &str,
    normalized_command: Option<&str>,
) -> Option<String> {
    if !is_test_command(normalized_command) {
        return None;
    }

    let lines = split_lines(arena, output);
    let (mut passed, mut failed, skipped) = extract_test_stats(output).unwrap_or((0, 0, 0));

    if passed == 0 && failed == 0 {
        for line in lines.iter().copied() {
            if FALLBACK_PASS_PATTERN.is_match(line) {
                passed += 1;
            }
            if FALLBACK_FAIL_PATTERN.is_match(line) {
                failed += 1;
            }
        }
    }

    let mut failures: ArenaVec<ArenaVec<&str>> = ArenaVec::new_in(arena);
    if failed > 0 {
        let mut in_failure = false;
        let mut current: ArenaVec<&str> = ArenaVec::new_in(arena);
        let mut blank_count = 0usize;

        for line in lines.iter().copied() {
            if FAILURE_START_PATTERNS
                .iter()
                .any(|pattern| pattern.is_match(line))
            {
                if in_failure && !current.is_empty() {
                    failures.push(std::mem::replace(&mut current, ArenaVec::new_in(arena)));
                }
                in_failure = true;
                current.push(line);
                blank_count = 0;
                continue;
            }
            if !in_failure {
                continue;
            }
            if line.trim().is_empty() {
                blank_count += 1;
                if blank_count >= 2 && current.len() > 3 {
                    failures.push(std::mem::replace(&mut current, ArenaVec::new_in(arena)));
                    in_failure = false;
                } else {
                    current.push(line);
                }
                continue;
            }
            if FAILURE_CONTINUATION.is_match(line) {
                current.push(line);
                blank_count = 0;
                continue;
            }
            failures.push(std::mem::replace(&mut current, ArenaVec::new_in(arena)));
            in_failure = false;
        }

        if in_failure && !current.is_empty() {
            failures.push(current);
        }
    }

    let mut out = ArenaString::new_in(arena);
    let mut first = true;
    push_piece(&mut out, &mut first, "Test Results:");

    let mut line = ArenaString::new_in(arena);
    let _ = write!(line, "   PASS: {passed} passed");
    push_piece(&mut out, &mut first, &line);

    if failed > 0 {
        let mut line = ArenaString::new_in(arena);
        let _ = write!(line, "   FAIL: {failed} failed");
        push_piece(&mut out, &mut first, &line);
    }
    if skipped > 0 {
        let mut line = ArenaString::new_in(arena);
        let _ = write!(line, "   SKIP: {skipped} skipped");
        push_piece(&mut out, &mut first, &line);
    }

    if failed > 0 && !failures.is_empty() {
        push_piece(&mut out, &mut first, "\n   Failures:");
        for failure in failures.iter().take(5) {
            let head = failure.first().copied().unwrap_or("");
            let mut line = ArenaString::new_in(arena);
            line.push_str("   - ");
            line.push_str(cut(arena, head, 70));
            push_piece(&mut out, &mut first, &line);

            for detail in failure.iter().skip(1).take(3) {
                if detail.trim().is_empty() {
                    continue;
                }
                let mut line = ArenaString::new_in(arena);
                line.push_str("     ");
                line.push_str(cut(arena, detail, 65));
                push_piece(&mut out, &mut first, &line);
            }
            if failure.len() > 4 {
                let mut line = ArenaString::new_in(arena);
                let _ = write!(line, "     ... ({} more lines)", failure.len() - 4);
                push_piece(&mut out, &mut first, &line);
            }
        }
        if failures.len() > 5 {
            let mut line = ArenaString::new_in(arena);
            let _ = write!(line, "   ... and {} more failures", failures.len() - 5);
            push_piece(&mut out, &mut first, &line);
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
        aggregate_test_output(scratch.arena(), output, command)
    }

    #[test]
    fn is_test_command_should_match_common_runners() {
        assert!(is_test_command(Some("cargo test --all")));
        assert!(is_test_command(Some("npm test")));
        assert!(is_test_command(Some("pytest -q")));
        assert!(is_test_command(Some("zig test src/main.zig")));
        assert!(is_test_command(Some("zig build test")));
        assert!(!is_test_command(Some("zig build")));
        assert!(!is_test_command(Some("cargo build")));
        assert!(!is_test_command(None));
    }

    #[test]
    fn cargo_result_line_should_map_passed_and_failed_correctly() {
        let output = "running 3 tests\ntest result: ok. 5 passed; 1 failed; 0 ignored;\n";
        let result = aggregate(output, Some("cargo test")).expect("应压缩");
        assert!(result.contains("PASS: 5 passed"), "got {result}");
        assert!(result.contains("FAIL: 1 failed"), "got {result}");
    }

    #[test]
    fn zig_test_runner_summary_should_be_extracted() {
        let result = aggregate("All 3 tests passed.", Some("zig test src/main.zig")).expect("应压缩");
        assert!(result.contains("PASS: 3 passed"), "got {result}");
    }

    #[test]
    fn zig_failing_test_should_be_collected() {
        let output = "1/2 test.add... FAIL (TestUnexpectedResult)\n  expected 3, found 4\n2/2 test.sub... OK\n1 passed; 1 failed.";
        let result = aggregate(output, Some("zig build test")).expect("应压缩");
        assert!(result.contains("FAIL: 1 failed"), "got {result}");
        assert!(result.contains("Failures:"), "got {result}");
        assert!(result.contains("FAIL (TestUnexpectedResult)"), "got {result}");
    }

    #[test]
    fn jest_style_counts_should_be_extracted() {
        let output = "Tests:       12 passed, 2 failed, 1 skipped";
        let result = aggregate(output, Some("jest")).expect("应压缩");
        assert!(result.contains("PASS: 12 passed"), "got {result}");
        assert!(result.contains("FAIL: 2 failed"), "got {result}");
        assert!(result.contains("SKIP: 1 skipped"), "got {result}");
    }

    #[test]
    fn fallback_should_count_marker_lines() {
        let result = aggregate("✓ first\n✓ second\n✗ third", Some("vitest")).expect("应压缩");
        assert!(result.contains("PASS: 2 passed"), "got {result}");
        assert!(result.contains("FAIL: 1 failed"), "got {result}");
    }

    #[test]
    fn failures_should_be_summarised_with_continuations() {
        // 注意用显式 \n 拼接：Rust 的字符串续行会吃掉下一行行首缩进，
        // 而失败块的续行判定依赖缩进。
        let output = "test result: FAILED. 0 passed; 1 failed;\nFAIL src/demo.rs\n  assertion failed: left == right\n\n\n";
        let result = aggregate(output, Some("cargo test")).expect("应压缩");
        assert!(result.contains("Failures:"), "got {result}");
        assert!(result.contains("FAIL src/demo.rs"), "got {result}");
        assert!(result.contains("assertion failed"), "got {result}");
    }

    #[test]
    fn non_test_command_should_return_none() {
        assert!(aggregate("anything", Some("ls")).is_none());
    }

    #[test]
    fn cut_should_borrow_when_within_limit_and_ellipsize_otherwise() {
        let scratch = Scratch::with_capacity(64);
        assert_eq!(cut(scratch.arena(), "abc", 3), "abc");
        assert_eq!(cut(scratch.arena(), "abcd", 3), "abc...");
        assert_eq!(cut(scratch.arena(), "中文测试", 2), "中文...");
    }
}