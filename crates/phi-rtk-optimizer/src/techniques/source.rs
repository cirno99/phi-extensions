// source.rs — 源码过滤与智能截断：按语言识别注释、签名与导入。
//
// 由 pi 版 pi-rtk-optimizer 的 src/techniques/source.ts 移植。
//
// 与 pi 版的差异：pi 用 UTF-16 下标扫描行内字符，这里用 `Vec<char>`，
// 对多字节注释符/字符串更安全。

use std::sync::LazyLock;

use regex::Regex;

/// 识别到的语言。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    TypeScript,
    JavaScript,
    Python,
    Rust,
    Go,
    Java,
    C,
    Cpp,
    /// Zig（含构建描述文件 `.zon`）。
    Zig,
    Unknown,
}

/// 源码过滤强度。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterLevel {
    None,
    Minimal,
    Aggressive,
}

/// 按扩展名映射语言。
pub fn detect_language(file_path: &str) -> Language {
    let Some(dot) = file_path.rfind('.') else {
        return Language::Unknown;
    };
    match file_path[dot..].to_ascii_lowercase().as_str() {
        ".ts" | ".tsx" => Language::TypeScript,
        ".js" | ".jsx" | ".mjs" => Language::JavaScript,
        ".py" | ".pyw" => Language::Python,
        ".rs" => Language::Rust,
        ".go" => Language::Go,
        ".java" => Language::Java,
        ".c" | ".h" => Language::C,
        ".cpp" | ".hpp" | ".cc" => Language::Cpp,
        ".zig" | ".zon" => Language::Zig,
        _ => Language::Unknown,
    }
}

/// 语言的注释记号。
struct CommentPatterns {
    line: Option<&'static str>,
    block_start: Option<&'static str>,
    block_end: Option<&'static str>,
    /// 需要保留的文档注释前缀（Zig 同时有 `///` 与 `//!`）。
    doc_lines: &'static [&'static str],
    doc_block_start: Option<&'static str>,
}

fn comment_patterns(language: Language) -> CommentPatterns {
    match language {
        Language::Python => CommentPatterns {
            line: Some("#"),
            block_start: Some("\"\"\""),
            block_end: Some("\"\"\""),
            doc_lines: &[],
            doc_block_start: Some("\"\"\""),
        },
        Language::Rust => CommentPatterns {
            line: Some("//"),
            block_start: Some("/*"),
            block_end: Some("*/"),
            doc_lines: &["///"],
            doc_block_start: Some("/**"),
        },
        Language::Zig => CommentPatterns {
            line: Some("//"),
            block_start: Some("/*"),
            block_end: Some("*/"),
            // Zig 的文档注释：`///` 声明文档，`//!` 容器文档。
            doc_lines: &["///", "//!"],
            doc_block_start: Some("/**"),
        },
        Language::Unknown => CommentPatterns {
            line: Some("//"),
            block_start: Some("/*"),
            block_end: Some("*/"),
            doc_lines: &[],
            doc_block_start: None,
        },
        _ => CommentPatterns {
            line: Some("//"),
            block_start: Some("/*"),
            block_end: Some("*/"),
            doc_lines: &[],
            doc_block_start: Some("/**"),
        },
    }
}

/// 导入行。
static IMPORT_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(use\s+|import\s+|from\s+|require\(|#include)").expect("导入正则应可编译")
});

/// 函数/类型签名行。
static SIGNATURE_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(pub\s+)?(async\s+)?(fn|def|function|func|class|struct|enum|trait|interface|type)\s+\w+",
    )
    .expect("签名正则应可编译")
});

/// 常量/静态声明行。
static CONST_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(const|static|let|pub\s+const|pub\s+static)\s+").expect("常量正则应可编译")
});

/// 用户脚本元数据块起始行。
static USERSCRIPT_START: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^//\s*==\s*userscript\s*==$").expect("userscript 起始正则应可编译")
});

/// 用户脚本元数据块结束行。
static USERSCRIPT_END: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^//\s*==\s*/userscript\s*==$").expect("userscript 结束正则应可编译")
});

/// 连续 3 个以上换行折叠为 2 个。
static MULTI_NEWLINE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\n{3,}").expect("换行正则应可编译"));

/// `chars[index..]` 是否以 `needle` 开头。
fn starts_with_at(chars: &[char], index: usize, needle: &str) -> bool {
    let needle: Vec<char> = needle.chars().collect();
    chars.len() >= index + needle.len() && chars[index..index + needle.len()] == needle[..]
}

/// 从 `start` 起查找 `needle` 首次出现的下标。
fn find_from(chars: &[char], start: usize, needle: &str) -> Option<usize> {
    let needle: Vec<char> = needle.chars().collect();
    if needle.is_empty() || chars.len() < needle.len() {
        return None;
    }
    (start..=chars.len() - needle.len()).find(|&index| chars[index..index + needle.len()] == needle[..])
}

/// 去掉行内注释与字符串字面量，返回「代码部分」。
fn get_code_portion(line: &str, language: Language) -> String {
    let patterns = comment_patterns(language);
    let chars: Vec<char> = line.chars().collect();
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut code = String::new();
    let mut index = 0usize;

    while index < chars.len() {
        let character = chars[index];

        if escaped {
            escaped = false;
            index += 1;
            continue;
        }
        if let Some(active_quote) = quote {
            if character == '\\' {
                escaped = true;
                index += 1;
                continue;
            }
            if character == active_quote {
                quote = None;
            }
            index += 1;
            continue;
        }
        if let Some(line_comment) = patterns.line {
            if starts_with_at(&chars, index, line_comment) {
                break;
            }
        }
        if let (Some(block_start), Some(block_end)) = (patterns.block_start, patterns.block_end) {
            if starts_with_at(&chars, index, block_start) {
                let search_start = index + block_start.chars().count();
                match find_from(&chars, search_start, block_end) {
                    Some(end) => {
                        index = end + block_end.chars().count();
                        continue;
                    }
                    None => break,
                }
            }
        }
        if character == '"' || character == '\'' || character == '`' {
            quote = Some(character);
            index += 1;
            continue;
        }
        code.push(character);
        index += 1;
    }

    code
}

/// 统计代码部分的花括号数量。
fn count_code_braces(line: &str, language: Language) -> (usize, usize) {
    let mut open = 0usize;
    let mut close = 0usize;
    for character in get_code_portion(line, language).chars() {
        match character {
            '{' => open += 1,
            '}' => close += 1,
            _ => {}
        }
    }
    (open, close)
}

/// 最小过滤：去注释与空行，保留结构与文档注释。
pub fn filter_minimal(content: &str, language: Language) -> String {
    let patterns = comment_patterns(language);
    let mut result: Vec<String> = Vec::new();
    let mut in_block_comment = false;
    let mut in_docstring = false;
    let mut in_userscript_block = false;

    for line in content.split('\n') {
        let trimmed = line.trim();

        if USERSCRIPT_START.is_match(trimmed) {
            in_userscript_block = true;
            result.push(line.to_string());
            continue;
        }
        if in_userscript_block {
            result.push(line.to_string());
            if USERSCRIPT_END.is_match(trimmed) {
                in_userscript_block = false;
            }
            continue;
        }

        if let (Some(block_start), Some(block_end)) = (patterns.block_start, patterns.block_end) {
            if !in_docstring
                && trimmed.contains(block_start)
                && !patterns
                    .doc_block_start
                    .is_some_and(|doc| trimmed.starts_with(doc))
            {
                in_block_comment = true;
            }
            if in_block_comment {
                if trimmed.contains(block_end) {
                    in_block_comment = false;
                }
                continue;
            }
        }

        if language == Language::Python && trimmed.starts_with("\"\"\"") {
            in_docstring = !in_docstring;
            result.push(line.to_string());
            continue;
        }
        if in_docstring {
            result.push(line.to_string());
            continue;
        }

        if let Some(line_comment) = patterns.line {
            if trimmed.starts_with(line_comment) {
                if patterns.doc_lines.iter().any(|doc| trimmed.starts_with(doc)) {
                    result.push(line.to_string());
                }
                continue;
            }
        }

        if trimmed.is_empty() {
            result.push(String::new());
            continue;
        }

        result.push(line.to_string());
    }

    let joined = result.join("\n");
    MULTI_NEWLINE.replace_all(&joined, "\n\n").trim().to_string()
}

/// 激进过滤：在最小过滤之上只保留导入、签名与常量。
pub fn filter_aggressive(content: &str, language: Language) -> String {
    let minimal = filter_minimal(content, language);
    let mut result: Vec<String> = Vec::new();
    let mut brace_depth: i64 = 0;
    let mut in_implementation = false;

    for line in minimal.split('\n') {
        let trimmed = line.trim();

        if IMPORT_PATTERN.is_match(trimmed) {
            result.push(line.to_string());
            continue;
        }
        if SIGNATURE_PATTERN.is_match(trimmed) {
            result.push(line.to_string());
            in_implementation = true;
            brace_depth = 0;
            continue;
        }

        let (open, close) = count_code_braces(line, language);
        let code_trimmed = get_code_portion(line, language);
        let code_trimmed = code_trimmed.trim();

        if in_implementation {
            brace_depth += open as i64;
            brace_depth -= close as i64;

            if brace_depth <= 1
                && (code_trimmed == "{" || code_trimmed == "}" || code_trimmed.ends_with('{'))
            {
                result.push(line.to_string());
            }
            if brace_depth <= 0 {
                in_implementation = false;
                if !trimmed.is_empty() && trimmed != "}" {
                    result.push("    // ... implementation".to_string());
                }
            }
            continue;
        }

        if CONST_PATTERN.is_match(trimmed) {
            result.push(line.to_string());
        }
    }

    result.join("\n").trim().to_string()
}

/// 智能截断：保留签名/导入/花括号与前半部分，其余折叠为省略行。
pub fn smart_truncate(content: &str, max_lines: usize, _language: Language) -> String {
    let lines: Vec<&str> = content.split('\n').collect();
    if lines.len() <= max_lines {
        return content.to_string();
    }

    let mut result: Vec<String> = Vec::new();
    let mut kept_lines = 0usize;
    let mut skipped_section = false;

    for line in &lines {
        let trimmed = line.trim();
        let is_important = SIGNATURE_PATTERN.is_match(trimmed)
            || IMPORT_PATTERN.is_match(trimmed)
            || trimmed.starts_with("pub ")
            || trimmed.starts_with("export ")
            || trimmed == "}"
            || trimmed == "{";

        if is_important || kept_lines < max_lines / 2 {
            if skipped_section {
                result.push(format!("    // ... {} lines omitted", lines.len() - kept_lines));
                skipped_section = false;
            }
            result.push((*line).to_string());
            kept_lines += 1;
        } else {
            skipped_section = true;
        }

        if kept_lines >= max_lines - 1 {
            break;
        }
    }

    if skipped_section || kept_lines < lines.len() {
        result.push(format!(
            "// ... {} more lines (total: {})",
            lines.len() - kept_lines,
            lines.len()
        ));
    }

    result.join("\n")
}

/// 按强度过滤源码。
pub fn filter_source_code(content: &str, language: Language, level: FilterLevel) -> String {
    match level {
        FilterLevel::None => content.to_string(),
        FilterLevel::Minimal => filter_minimal(content, language),
        FilterLevel::Aggressive => filter_aggressive(content, language),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_language_should_map_extensions() {
        assert_eq!(detect_language("src/main.rs"), Language::Rust);
        assert_eq!(detect_language("a.tsx"), Language::TypeScript);
        assert_eq!(detect_language("a.py"), Language::Python);
        assert_eq!(detect_language("a.h"), Language::C);
        assert_eq!(detect_language("build.zig"), Language::Zig);
        assert_eq!(detect_language("build.zig.zon"), Language::Zig);
        assert_eq!(detect_language("noext"), Language::Unknown);
        assert_eq!(detect_language("a.unknown"), Language::Unknown);
    }

    #[test]
    fn filter_minimal_should_drop_line_and_block_comments() {
        let source = "// leading comment\nfn main() {\n    /* block */\n    let x = 1;\n}\n";
        let filtered = filter_minimal(source, Language::Rust);
        assert!(!filtered.contains("leading comment"), "got {filtered}");
        assert!(!filtered.contains("block"), "got {filtered}");
        assert!(filtered.contains("fn main()"), "got {filtered}");
        assert!(filtered.contains("let x = 1;"), "got {filtered}");
    }

    #[test]
    fn filter_minimal_should_keep_rust_doc_comments() {
        let source = "/// docs\nfn main() {}\n";
        let filtered = filter_minimal(source, Language::Rust);
        assert!(filtered.contains("/// docs"), "got {filtered}");
    }

    #[test]
    fn filter_minimal_should_keep_zig_doc_comments() {
        let source = "//! container docs\n/// decl docs\n// plain\npub fn main() void {}\n";
        let filtered = filter_minimal(source, Language::Zig);
        assert!(filtered.contains("//! container docs"), "got {filtered}");
        assert!(filtered.contains("/// decl docs"), "got {filtered}");
        assert!(!filtered.contains("// plain"), "got {filtered}");
        assert!(filtered.contains("pub fn main() void {}"), "got {filtered}");
    }

    #[test]
    fn filter_minimal_should_drop_zig_block_comments() {
        let source = "/* block */\nconst std = @import(\"std\");\n";
        let filtered = filter_minimal(source, Language::Zig);
        assert!(!filtered.contains("block"), "got {filtered}");
        assert!(filtered.contains("@import"), "got {filtered}");
    }

    #[test]
    fn filter_aggressive_should_keep_zig_signatures_and_consts() {
        let source = "const std = @import(\"std\");\npub fn add(a: i32, b: i32) i32 {\n    return a + b;\n}\n";
        let filtered = filter_aggressive(source, Language::Zig);
        assert!(filtered.contains("@import"), "got {filtered}");
        assert!(filtered.contains("pub fn add"), "got {filtered}");
        assert!(!filtered.contains("return a + b;"), "got {filtered}");
    }

    #[test]
    fn code_portion_should_handle_zig_comment_forms() {
        let portion = get_code_portion("const x = 1; //! note", Language::Zig);
        assert_eq!(portion.trim(), "const x = 1;");
    }

    #[test]
    fn filter_minimal_should_preserve_python_docstrings() {
        let source = "def f():\n    \"\"\"doc\"\"\"\n    return 1\n";
        let filtered = filter_minimal(source, Language::Python);
        assert!(filtered.contains("\"\"\"doc\"\"\""), "got {filtered}");
        assert!(filtered.contains("return 1"), "got {filtered}");
    }

    #[test]
    fn filter_minimal_should_collapse_blank_runs() {
        let source = "a\n\n\n\n\nb";
        let filtered = filter_minimal(source, Language::Rust);
        assert_eq!(filtered, "a\n\nb");
    }

    #[test]
    fn filter_aggressive_should_keep_imports_signatures_and_consts() {
        let source = "use std::fmt;\nconst N: usize = 3;\nfn foo() {\n    let inner = 1;\n    inner\n}\n";
        let filtered = filter_aggressive(source, Language::Rust);
        assert!(filtered.contains("use std::fmt;"), "got {filtered}");
        assert!(filtered.contains("const N: usize = 3;"), "got {filtered}");
        assert!(filtered.contains("fn foo()"), "got {filtered}");
        assert!(!filtered.contains("let inner"), "got {filtered}");
        assert!(filtered.contains("// ... implementation"), "got {filtered}");
    }

    #[test]
    fn smart_truncate_should_keep_important_lines() {
        let source = (0..100)
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let truncated = smart_truncate(&source, 20, Language::Rust);
        assert!(truncated.lines().count() <= 20, "got {}", truncated.lines().count());
        assert!(truncated.contains("more lines (total: 100)"), "got {truncated}");
    }

    #[test]
    fn smart_truncate_should_pass_through_short_input() {
        let source = "a\nb\nc";
        assert_eq!(smart_truncate(source, 10, Language::Rust), source);
    }

    #[test]
    fn filter_source_code_should_honour_level() {
        let source = "// c\nlet x = 1;";
        assert_eq!(
            filter_source_code(source, Language::Rust, FilterLevel::None),
            source
        );
        assert_eq!(
            filter_source_code(source, Language::Rust, FilterLevel::Minimal),
            "let x = 1;"
        );
        assert!(filter_source_code(source, Language::Rust, FilterLevel::Aggressive)
            .contains("let x = 1;"));
    }

    #[test]
    fn code_portion_should_ignore_comments_inside_strings() {
        let portion = get_code_portion(r#"let url = "http://x"; // real comment"#, Language::Rust);
        // 字符串字面量的内容会被跳过（与 pi 版一致），行尾注释则整段丢弃。
        assert!(portion.contains("let url"), "got {portion}");
        assert!(!portion.contains("http://x"), "got {portion}");
        assert!(!portion.contains("real comment"), "got {portion}");
    }

    #[test]
    fn code_portion_should_strip_block_comments_inline() {
        let portion = get_code_portion("let a = 1; /* note */ let b = 2;", Language::Rust);
        assert!(portion.contains("let a = 1;"), "got {portion}");
        assert!(portion.contains("let b = 2;"), "got {portion}");
        assert!(!portion.contains("note"), "got {portion}");
    }

    #[test]
    fn count_code_braces_should_ignore_braces_in_strings() {
        assert_eq!(count_code_braces(r#"f("{")"#, Language::Rust), (0, 0));
        assert_eq!(count_code_braces("fn f() { }", Language::Rust), (1, 1));
    }
}