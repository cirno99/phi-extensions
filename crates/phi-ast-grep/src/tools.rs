//! LLM 可调用工具：`ast_grep_search` 与 `ast_grep_replace`。
//!
//! 由 pi 版 pi-ast-grep 的 `src/ast-grep/tools.ts` 移植。
//!
//! 与 pi 的差异：pi 的 `promptSnippet` / `promptGuidelines` 在 phi 的 `Tool`
//! 里没有对应字段，因此把「何时该用本工具而非 grep / edit」的指引并进
//! `description`（模型可见）。

use phi_ext::phi;
use phi_ext_common::json::{ValueAsArray, ValueAsScalar, ValueObjectAccess};
use serde::de::DeserializeOwned;
use serde::Deserialize;

use crate::cli::run_sg;
use crate::cwd::project_cwd;
use crate::pattern_hints::get_pattern_hint;
use crate::render::{replace_call_detail, replace_result_detail, search_call_detail, search_result_detail};
use crate::result_formatter::{format_replace_result, format_search_result};
use crate::types::{is_cli_language, RunSgOptions, CLI_LANGUAGES};

/// 搜索工具入参。
#[derive(Debug, Deserialize)]
struct SearchArgs {
    pattern: String,
    lang: String,
    #[serde(default)]
    paths: Option<Vec<String>>,
    #[serde(default)]
    globs: Option<Vec<String>>,
    #[serde(default)]
    context: Option<i64>,
}

/// 改写工具入参。
#[derive(Debug, Deserialize)]
struct ReplaceArgs {
    pattern: String,
    rewrite: String,
    lang: String,
    #[serde(default)]
    paths: Option<Vec<String>>,
    #[serde(default)]
    globs: Option<Vec<String>>,
    #[serde(default, rename = "dryRun")]
    dry_run: Option<bool>,
}

/// 解析工具入参（simd-json）。
fn parse_args<T: DeserializeOwned>(args: &[u8]) -> Result<T, String> {
    phi_ext_common::json::parse::<T>(args).map_err(|e| format!("invalid arguments: {e}"))
}

/// 非法语言的统一返回（与 pi 版 `invalidLanguageResult` 一致，作为 content 而非错误）。
fn invalid_language_result(language: &str) -> phi::ToolResult {
    phi::ToolResult {
        content: format!("Unsupported language: {language}"),
        ..Default::default()
    }
}

/// 入参缺省路径时回退到项目工作目录。
fn resolve_paths(paths: Vec<String>) -> Vec<String> {
    if paths.is_empty() {
        vec![project_cwd()]
    } else {
        paths
    }
}

/// 搜索工具的参数 schema。
fn search_schema() -> phi::Schema {
    phi::Schema::object()
        .property(
            "pattern",
            phi::Schema::string()
                .description("AST pattern with meta-variables ($VAR, $$$). Must be a complete AST node."),
        )
        .property(
            "lang",
            phi::Schema::string()
                .description("Target language")
                .enum_values(CLI_LANGUAGES),
        )
        .property(
            "paths",
            phi::Schema::array(phi::Schema::string())
                .description("Paths to search (default: current working directory)"),
        )
        .property(
            "globs",
            phi::Schema::array(phi::Schema::string())
                .description("Include/exclude globs (prefix ! to exclude)"),
        )
        .property(
            "context",
            phi::Schema::integer().description("Number of context lines around each match"),
        )
        .required(["pattern", "lang"])
}

/// 改写工具的参数 schema。
fn replace_schema() -> phi::Schema {
    phi::Schema::object()
        .property(
            "pattern",
            phi::Schema::string().description("AST pattern to match"),
        )
        .property(
            "rewrite",
            phi::Schema::string()
                .description("Replacement pattern (can use $VAR from pattern)"),
        )
        .property(
            "lang",
            phi::Schema::string()
                .description("Target language")
                .enum_values(CLI_LANGUAGES),
        )
        .property(
            "paths",
            phi::Schema::array(phi::Schema::string()).description("Paths to search"),
        )
        .property(
            "globs",
            phi::Schema::array(phi::Schema::string()).description("Include/exclude globs"),
        )
        .property(
            "dryRun",
            phi::Schema::boolean()
                .description("Preview changes without applying (default: true)"),
        )
        .required(["pattern", "rewrite", "lang"])
}

/// 注册两个工具。
pub fn register(ext: &mut phi::Extension) {
    register_search(ext);
    register_replace(ext);
}

fn register_search(ext: &mut phi::Extension) {
    let description = "Search code patterns across the filesystem using AST-aware matching. \
        Use meta-variables: $VAR (single node), $$$ (multiple nodes). \
        Patterns must be complete AST nodes (valid code). \
        Examples: 'console.log($MSG)', 'def $FUNC($$$):', 'function $NAME($$$) { $$$ }'. \
        Use ast_grep_search instead of grep when the pattern depends on code structure \
        (function/class/import/call shape); use grep for plain text or cross-language regex.";
    let tool = phi::Tool::new(
        "ast_grep_search",
        description,
        search_schema(),
        |args: &[u8]| -> Result<phi::ToolResult, String> {
            let parsed: SearchArgs = parse_args(args)?;
            if !is_cli_language(&parsed.lang) {
                return Ok(invalid_language_result(&parsed.lang));
            }
            let paths = resolve_paths(parsed.paths.unwrap_or_default());
            let options = RunSgOptions {
                pattern: parsed.pattern.clone(),
                lang: parsed.lang.clone(),
                paths,
                globs: parsed.globs.unwrap_or_default(),
                rewrite: None,
                context: parsed.context,
                update_all: false,
            };
            let result = run_sg(&options);

            let text = format_search_result(&result);
            let hint = if result.matches.is_empty() && result.error.is_none() {
                get_pattern_hint(&parsed.pattern, &parsed.lang)
            } else {
                None
            };
            let final_text = match hint {
                Some(hint) => format!("{text}\n\n{hint}"),
                None => text,
            };

            Ok(phi::ToolResult {
                content: final_text,
                detail: search_result_detail(&result),
                ..Default::default()
            })
        },
    )
    // 搜索无副作用，宿主可并发批量调用。
    .readable()
    .detail_from_args(|args| {
        let Ok(value) = phi_ext_common::json::value(args) else {
            return String::new();
        };
        let pattern = value.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
        let lang = value.get("lang").and_then(|v| v.as_str()).unwrap_or("");
        let paths = read_string_array(&value, "paths");
        let globs = read_string_array(&value, "globs");
        let context = value.get("context").and_then(|v| v.as_i64());
        search_call_detail(pattern, &paths, lang, &globs, context)
    });
    ext.register_tool(tool);
}

fn register_replace(ext: &mut phi::Extension) {
    let description = "Replace code patterns across the filesystem with AST-aware rewriting. \
        Dry-run by default. Use meta-variables in `rewrite` to preserve matched content. \
        Example: pattern='console.log($MSG)' rewrite='logger.info($MSG)'. \
        Use ast_grep_replace dryRun=true first to preview changes; only set dryRun=false after \
        confirming the match list. Use ast_grep_replace instead of edit when the rewrite spans \
        many files with the same structural pattern.";
    let tool = phi::Tool::new(
        "ast_grep_replace",
        description,
        replace_schema(),
        |args: &[u8]| -> Result<phi::ToolResult, String> {
            let parsed: ReplaceArgs = parse_args(args)?;
            if !is_cli_language(&parsed.lang) {
                return Ok(invalid_language_result(&parsed.lang));
            }
            let paths = resolve_paths(parsed.paths.unwrap_or_default());
            let dry_run = parsed.dry_run.unwrap_or(true);
            let options = RunSgOptions {
                pattern: parsed.pattern.clone(),
                lang: parsed.lang.clone(),
                paths,
                globs: parsed.globs.unwrap_or_default(),
                rewrite: Some(parsed.rewrite.clone()),
                context: None,
                update_all: !dry_run,
            };
            let result = run_sg(&options);

            let text = format_replace_result(&result, dry_run);

            Ok(phi::ToolResult {
                content: text,
                detail: replace_result_detail(&result, dry_run),
                ..Default::default()
            })
        },
    )
    .detail_from_args(|args| {
        let Ok(value) = phi_ext_common::json::value(args) else {
            return String::new();
        };
        let pattern = value.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
        let rewrite = value.get("rewrite").and_then(|v| v.as_str()).unwrap_or("");
        let lang = value.get("lang").and_then(|v| v.as_str()).unwrap_or("");
        let paths = read_string_array(&value, "paths");
        let globs = read_string_array(&value, "globs");
        let dry_run = value
            .get("dryRun")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        replace_call_detail(pattern, rewrite, &paths, lang, &globs, dry_run)
    });
    ext.register_tool(tool);
}

/// 读取对象里的字符串数组字段。
fn read_string_array(value: &phi_ext_common::json::Value, key: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_language_is_content_not_error() {
        let result = invalid_language_result("cobol");
        assert_eq!(result.content, "Unsupported language: cobol");
    }

    #[test]
    fn resolve_paths_defaults_to_project_cwd() {
        let paths = resolve_paths(Vec::new());
        assert_eq!(paths.len(), 1);
        assert!(!paths[0].trim().is_empty());
    }

    #[test]
    fn resolve_paths_keeps_explicit_paths() {
        let paths = resolve_paths(vec!["src".to_string()]);
        assert_eq!(paths, vec!["src".to_string()]);
    }

    #[test]
    fn search_schema_has_required_and_language_enum() {
        let bytes = search_schema().to_json_bytes();
        let value = phi_ext_common::json::value(&bytes).expect("解析 schema");
        let required = value
            .get("required")
            .and_then(|v| v.as_array())
            .expect("应有 required");
        assert!(required.iter().any(|v| v.as_str() == Some("pattern")));
        assert!(required.iter().any(|v| v.as_str() == Some("lang")));
        let lang_enum = value
            .get("properties")
            .and_then(|p| p.get("lang"))
            .and_then(|l| l.get("enum"))
            .and_then(|e| e.as_array())
            .expect("lang 应有 enum");
        assert_eq!(lang_enum.len(), CLI_LANGUAGES.len());
    }

    #[test]
    fn replace_schema_requires_rewrite_and_lang() {
        let bytes = replace_schema().to_json_bytes();
        let value = phi_ext_common::json::value(&bytes).expect("解析 schema");
        let required = value
            .get("required")
            .and_then(|v| v.as_array())
            .expect("应有 required");
        assert!(required.iter().any(|v| v.as_str() == Some("rewrite")));
        assert!(required.iter().any(|v| v.as_str() == Some("lang")));
    }
}