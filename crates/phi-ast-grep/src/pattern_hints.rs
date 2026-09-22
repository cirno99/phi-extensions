//! 反模式提示。
//!
//! 由 pi 版 pi-ast-grep 的 `src/ast-grep/pattern-hints.ts` 移植：pattern 不是
//! 合法 AST 节点（写了正则之类）且零匹配时，反过来建议改用 grep 或补全节点。

use std::sync::LazyLock;

use regex::Regex;


/// 正则转义符误用。
static RE_ESCAPE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\\[wWdDsSbB]").expect("正则"));
/// 字符类误用：`[a-z]`。
static RE_CHAR_CLASS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[[a-zA-Z0-9]-[a-zA-Z0-9]\]").expect("正则"));
/// 通配符误用：`x.*` / `x.+`。
static RE_WILDCARD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\w\.[*+]").expect("正则"));
/// 交替误用：`foo|bar`。
static RE_ALTERNATION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[-\w.*]+\|[-\w.*|]+$").expect("正则"));
/// JS/TS 函数签名缺参数与函数体。
static RE_JS_FUNCTION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(export\s+)?(async\s+)?function\s+\$[A-Z_]+\s*$").expect("正则")
});
/// Go 函数签名缺参数与函数体。
static RE_GO_FUNC: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^func\s+\$[A-Z_]+\s*$").expect("正则"));
/// Rust 函数签名缺参数与函数体。
static RE_RUST_FN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^fn\s+\$[A-Z_]+\s*$").expect("正则"));

/// 检测正则语法误用，返回提示文本。
pub fn detect_regex_misuse(pattern: &str) -> Option<String> {
    let src = pattern.trim();

    if RE_ESCAPE.is_match(src) {
        return Some(
            "Hint: \"\\w\", \"\\d\", \"\\s\", \"\\b\" are regex escapes. ast-grep matches AST nodes, not text - use $VAR for identifiers, $$$ for node lists, or switch to grep for text search.".to_string(),
        );
    }

    if RE_CHAR_CLASS.is_match(src) {
        return Some(
            "Hint: \"[a-z]\" and similar character classes are regex, not AST. Use $VAR to match any identifier, or switch to grep for text search.".to_string(),
        );
    }

    if !src.contains('$') && RE_WILDCARD.is_match(src) {
        return Some(
            "Hint: \".*\" and \".+\" are regex wildcards. In ast-grep use $$$ for multiple AST nodes and $VAR for a single node. For text patterns, switch to grep.".to_string(),
        );
    }

    if RE_ALTERNATION.is_match(src) {
        return Some(
            "Hint: \"|\" is regex alternation and does NOT work in ast-grep patterns. Options: (a) fire one ast_grep_search per alternative, or (b) switch to grep with a regex pattern like \"foo|bar\".".to_string(),
        );
    }

    None
}

/// 检测语言专属的常见错误，返回提示文本。
pub fn detect_language_specific_mistake(pattern: &str, lang: &str) -> Option<String> {
    let src = pattern.trim();

    if lang == "python"
        && (src.starts_with("class ") || src.starts_with("def ") || src.starts_with("async def "))
        && src.ends_with(':')
    {
        return Some(format!(
            "Hint: Remove trailing colon. Try: \"{}\"",
            &src[..src.len() - 1]
        ));
    }

    if matches!(lang, "javascript" | "typescript" | "tsx") && RE_JS_FUNCTION.is_match(src) {
        return Some(
            "Hint: Function patterns need params and body. Try \"function $NAME($$$) { $$$ }\""
                .to_string(),
        );
    }

    if lang == "go" && RE_GO_FUNC.is_match(src) {
        return Some(
            "Hint: Go function patterns need params and body. Try \"func $NAME($$$) { $$$ }\""
                .to_string(),
        );
    }

    if lang == "rust" && RE_RUST_FN.is_match(src) {
        return Some(
            "Hint: Rust fn patterns need params and body. Try \"fn $NAME($$$) { $$$ }\""
                .to_string(),
        );
    }

    None
}

/// 综合提示：先查正则误用，再查语言专属错误。
pub fn get_pattern_hint(pattern: &str, lang: &str) -> Option<String> {
    detect_regex_misuse(pattern).or_else(|| detect_language_specific_mistake(pattern, lang))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_regex_escapes() {
        let hint = detect_regex_misuse(r"foo\d+").expect("应有提示");
        assert!(hint.contains("regex escapes"));
    }

    #[test]
    fn flags_char_class() {
        assert!(detect_regex_misuse("[a-z]").is_some());
    }

    #[test]
    fn flags_wildcard_only_without_meta_variable() {
        assert!(detect_regex_misuse("foo.*").is_some());
        // 含 $ 时不触发通配符提示。
        assert!(detect_regex_misuse("foo.$VAR.*").is_none());
    }

    #[test]
    fn flags_alternation() {
        assert!(detect_regex_misuse("foo|bar").is_some());
    }

    #[test]
    fn valid_ast_pattern_has_no_hint() {
        assert!(get_pattern_hint("console.log($MSG)", "typescript").is_none());
    }

    #[test]
    fn python_trailing_colon_hint() {
        let hint = detect_language_specific_mistake("class Foo:", "python").expect("应有提示");
        assert!(hint.contains("Remove trailing colon"));
        assert!(hint.contains("class Foo"));
        assert!(!hint.contains("Foo:"));
    }

    #[test]
    fn js_bare_function_hint() {
        assert!(detect_language_specific_mistake("function $NAME", "typescript").is_some());
        assert!(detect_language_specific_mistake("export async function $NAME", "tsx").is_some());
    }

    #[test]
    fn go_and_rust_bare_fn_hints() {
        assert!(detect_language_specific_mistake("func $NAME", "go").is_some());
        assert!(detect_language_specific_mistake("fn $NAME", "rust").is_some());
    }
}