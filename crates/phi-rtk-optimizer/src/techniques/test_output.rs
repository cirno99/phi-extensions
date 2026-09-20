// test_output.rs — 测试输出压缩：抽取 PASS/FAIL/SKIP 计数与失败摘要。
//
// 由 pi 版 pi-rtk-optimizer 的 src/techniques/test-output.ts 移植。
//
// 与 pi 版的差异（有意修正）：
// pi 版第一条「test result:」模式把 `(\w+)` 当作 passed 分组，导致
// `test result: ok. 5 passed; 0 failed;` 被解析成 passed=0 / failed=5。
// 本实现按模式分别声明分组下标，取正确的数字分组。

use std::sync::LazyLock;

use regex::Regex;

use super::command_detection::matches_command_patterns;

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
pub fn is_test_command(command: Option<&str>) -> bool {
    matches_command_patterns(command, &TEST_COMMAND_PATTERNS)
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

/// 压缩测试输出；非测试命令返回 `None`。
pub fn aggregate_test_output(output: &str, command: Option<&str>) -> Option<String> {
    if !is_test_command(command) {
        return None;
    }

    let lines: Vec<&str> = output.split('\n').collect();
    let (mut passed, mut failed, skipped) = extract_test_stats(output).unwrap_or((0, 0, 0));

    if passed == 0 && failed == 0 {
        for line in &lines {
            if FALLBACK_PASS_PATTERN.is_match(line) {
                passed += 1;
            }
            if FALLBACK_FAIL_PATTERN.is_match(line) {
                failed += 1;
            }
        }
    }

    let mut failures: Vec<String> = Vec::new();
    if failed > 0 {
        let mut in_failure = false;
        let mut current: Vec<String> = Vec::new();
        let mut blank_count = 0usize;

        for line in &lines {
            if FAILURE_START_PATTERNS
                .iter()
                .any(|pattern| pattern.is_match(line))
            {
                if in_failure && !current.is_empty() {
                    failures.push(current.join("\n"));
                }
                in_failure = true;
                current = vec![(*line).to_string()];
                blank_count = 0;
                continue;
            }
            if !in_failure {
                continue;
            }
            if line.trim().is_empty() {
                blank_count += 1;
                if blank_count >= 2 && current.len() > 3 {
                    failures.push(std::mem::take(&mut current).join("\n"));
                    in_failure = false;
                } else {
                    current.push((*line).to_string());
                }
                continue;
            }
            if FAILURE_CONTINUATION.is_match(line) {
                current.push((*line).to_string());
                blank_count = 0;
                continue;
            }
            failures.push(std::mem::take(&mut current).join("\n"));
            in_failure = false;
        }

        if in_failure && !current.is_empty() {
            failures.push(current.join("\n"));
        }
    }

    let mut result = vec!["Test Results:".to_string()];
    result.push(format!("   PASS: {passed} passed"));
    if failed > 0 {
        result.push(format!("   FAIL: {failed} failed"));
    }
    if skipped > 0 {
        result.push(format!("   SKIP: {skipped} skipped"));
    }

    if failed > 0 && !failures.is_empty() {
        result.push("\n   Failures:".to_string());
        for failure in failures.iter().take(5) {
            let failure_lines: Vec<&str> = failure.split('\n').collect();
            let first = failure_lines.first().copied().unwrap_or("");
            let first_cut = cut(first, 70);
            result.push(format!("   - {first_cut}"));
            for detail in failure_lines.iter().skip(1).take(3) {
                if !detail.trim().is_empty() {
                    let detail_cut = cut(detail, 65);
                    result.push(format!("     {detail_cut}"));
                }
            }
            if failure_lines.len() > 4 {
                result.push(format!("     ... ({} more lines)", failure_lines.len() - 4));
            }
        }
        if failures.len() > 5 {
            result.push(format!("   ... and {} more failures", failures.len() - 5));
        }
    }

    Some(result.join("\n"))
}

/// 按字符截断到 `max`，超出时追加 `...`。
fn cut(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let kept: String = text.chars().take(max).collect();
    format!("{kept}...")
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let result = aggregate_test_output(output, Some("cargo test")).expect("应压缩");
        assert!(result.contains("PASS: 5 passed"), "got {result}");
        assert!(result.contains("FAIL: 1 failed"), "got {result}");
    }

    #[test]
    fn zig_test_runner_summary_should_be_extracted() {
        let output = "All 3 tests passed.";
        let result = aggregate_test_output(output, Some("zig test src/main.zig")).expect("应压缩");
        assert!(result.contains("PASS: 3 passed"), "got {result}");
    }

    #[test]
    fn zig_failing_test_should_be_collected() {
        let output = "1/2 test.add... FAIL (TestUnexpectedResult)\n  expected 3, found 4\n2/2 test.sub... OK\n1 passed; 1 failed.";
        let result = aggregate_test_output(output, Some("zig build test")).expect("应压缩");
        assert!(result.contains("FAIL: 1 failed"), "got {result}");
        assert!(result.contains("Failures:"), "got {result}");
        assert!(result.contains("FAIL (TestUnexpectedResult)"), "got {result}");
    }

    #[test]
    fn jest_style_counts_should_be_extracted() {
        let output = "Tests:       12 passed, 2 failed, 1 skipped";
        let result = aggregate_test_output(output, Some("jest")).expect("应压缩");
        assert!(result.contains("PASS: 12 passed"), "got {result}");
        assert!(result.contains("FAIL: 2 failed"), "got {result}");
        assert!(result.contains("SKIP: 1 skipped"), "got {result}");
    }

    #[test]
    fn fallback_should_count_marker_lines() {
        let output = "✓ first\n✓ second\n✗ third";
        let result = aggregate_test_output(output, Some("vitest")).expect("应压缩");
        assert!(result.contains("PASS: 2 passed"), "got {result}");
        assert!(result.contains("FAIL: 1 failed"), "got {result}");
    }

    #[test]
    fn failures_should_be_summarised_with_continuations() {
        // 注意用显式 \n 拼接：Rust 的字符串续行会吃掉下一行行首缩进，
        // 而失败块的续行判定依赖缩进。
        let output = "test result: FAILED. 0 passed; 1 failed;\nFAIL src/demo.rs\n  assertion failed: left == right\n\n\n";
        let result = aggregate_test_output(output, Some("cargo test")).expect("应压缩");
        assert!(result.contains("Failures:"), "got {result}");
        assert!(result.contains("FAIL src/demo.rs"), "got {result}");
        assert!(result.contains("assertion failed"), "got {result}");
    }

    #[test]
    fn non_test_command_should_return_none() {
        assert!(aggregate_test_output("anything", Some("ls")).is_none());
    }
}