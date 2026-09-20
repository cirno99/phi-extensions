// build.rs — 构建输出压缩：只保留错误块与警告计数。
//
// 由 pi 版 pi-rtk-optimizer 的 src/techniques/build.ts 移植。

use std::sync::LazyLock;

use regex::Regex;

use super::command_detection::matches_command_patterns;

/// 构建类命令。
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

/// 进度噪音行（直接丢弃）。
static SKIP_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r"^\s*Compiling\s+",
        r"^\s*Checking\s+",
        r"^\s*Downloading\s+",
        r"^\s*Downloaded\s+",
        r"^\s*Fetching\s+",
        r"^\s*Fetched\s+",
        r"^\s*Updating\s+",
        r"^\s*Updated\s+",
        r"^\s*Building\s+",
        r"^\s*Generated\s+",
        r"^\s*Creating\s+",
        r"^\s*Running\s+",
    ]
    .iter()
    .map(|p| Regex::new(p).expect("跳过行正则应可编译"))
    .collect()
});

/// 错误块起始行。
static ERROR_START_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [r"^error\[", r"^error:", r"^\[ERROR\]", r"^FAIL"]
        .iter()
        .map(|p| Regex::new(p).expect("错误起始正则应可编译"))
        .collect()
});

/// 警告行。
static WARNING_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [r"^warning:", r"^\[WARNING\]", r"^warn:"]
        .iter()
        .map(|p| Regex::new(p).expect("警告正则应可编译"))
        .collect()
});

/// 已编译单元计数行。
static COMPILED_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(Compiling|Checking|Building)\s+").expect("编译行正则应可编译")
});

/// 错误块的续行（缩进行或 `-->` 定位行）。
static ERROR_CONTINUATION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s|^-->").expect("续行正则应可编译"));

/// 命令是否属于构建类。
pub fn is_build_command(command: Option<&str>) -> bool {
    matches_command_patterns(command, &BUILD_COMMAND_PATTERNS)
}

fn matches_any(patterns: &[Regex], line: &str) -> bool {
    patterns.iter().any(|pattern| pattern.is_match(line))
}

/// 压缩构建输出；非构建命令返回 `None`。
pub fn filter_build_output(output: &str, command: Option<&str>) -> Option<String> {
    if !is_build_command(command) {
        return None;
    }

    let mut compiled = 0usize;
    let mut errors: Vec<Vec<String>> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();

    let mut in_error_block = false;
    let mut current_error: Vec<String> = Vec::new();
    let mut blank_count = 0usize;

    for line in output.split('\n') {
        if COMPILED_PATTERN.is_match(line) {
            compiled += 1;
            continue;
        }
        if matches_any(&SKIP_PATTERNS, line) {
            continue;
        }
        if matches_any(&ERROR_START_PATTERNS, line) {
            if in_error_block && !current_error.is_empty() {
                errors.push(std::mem::take(&mut current_error));
            }
            in_error_block = true;
            current_error = vec![line.to_string()];
            blank_count = 0;
            continue;
        }
        if matches_any(&WARNING_PATTERNS, line) {
            warnings.push(line.to_string());
            continue;
        }
        if !in_error_block {
            continue;
        }
        if line.trim().is_empty() {
            blank_count += 1;
            if blank_count >= 2 && current_error.len() > 3 {
                errors.push(std::mem::take(&mut current_error));
                in_error_block = false;
            } else {
                current_error.push(line.to_string());
            }
            continue;
        }
        if ERROR_CONTINUATION.is_match(line) {
            current_error.push(line.to_string());
            blank_count = 0;
            continue;
        }
        errors.push(std::mem::take(&mut current_error));
        in_error_block = false;
    }

    if in_error_block && !current_error.is_empty() {
        errors.push(current_error);
    }

    if errors.is_empty() && warnings.is_empty() {
        return Some(format!("[OK] Build successful ({compiled} units compiled)"));
    }

    let mut result: Vec<String> = Vec::new();
    if !errors.is_empty() {
        result.push(format!("[ERROR] {} error(s):", errors.len()));
        for error in errors.iter().take(5) {
            for line in error.iter().take(10) {
                result.push(line.clone());
            }
            if error.len() > 10 {
                result.push("  ...".to_string());
            }
        }
        if errors.len() > 5 {
            result.push(format!("... and {} more errors", errors.len() - 5));
        }
    }
    if !warnings.is_empty() {
        result.push(format!("\n[WARN] {} warning(s)", warnings.len()));
    }

    Some(result.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let result = filter_build_output(output, Some("cargo build")).expect("应压缩");
        assert_eq!(result, "[OK] Build successful (2 units compiled)");
    }

    #[test]
    fn non_build_command_should_return_none() {
        assert!(filter_build_output("anything", Some("ls")).is_none());
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
        let result = filter_build_output(output, Some("cargo build")).expect("应压缩");
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
        let result = filter_build_output(&output, Some("cargo build")).expect("应压缩");
        assert!(result.contains("... and 3 more errors"), "got {result}");
    }

    #[test]
    fn warnings_only_should_report_count() {
        let output = "warning: unused import\nwarning: dead code";
        let result = filter_build_output(output, Some("cargo build")).expect("应压缩");
        assert_eq!(result, "\n[WARN] 2 warning(s)");
    }
}