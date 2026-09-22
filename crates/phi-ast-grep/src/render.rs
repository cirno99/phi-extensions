//! TUI 单行详情。
//!
//! pi 版 pi-ast-grep 用富 TUI 组件（`src/ast-grep/render.ts`）渲染调用行与
//! 结果行；phi 的 SDK 只提供「调用行单行 detail（`detail_from_args`）」与
//! 「结果行单行 detail（`ToolResult.detail`）」两个面，因此这里抽取 pi 折叠态
//! 的核心信息，渲染为单行文本。

use crate::types::{SgResult, SgTruncationReason};

/// 路径显示的最大长度。
const MAX_PATH_LENGTH: usize = 42;

/// 折叠态最多列出的文件数（用于结果 detail 里的文件计数，这里仅取计数）。
fn pluralize(count: usize, singular: &str, plural: &str) -> String {
    format!("{count} {}", if count == 1 { singular } else { plural })
}

/// 缩短路径：`$HOME` 前缀替换为 `~`，过长则保留尾部。
fn shorten_path(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    let display = match std::env::var("HOME") {
        Ok(home) if !home.is_empty() => {
            let home = home.replace('\\', "/");
            if let Some(rest) = normalized.strip_prefix(&home) {
                format!("~{rest}")
            } else {
                normalized
            }
        }
        _ => normalized,
    };
    if display.is_empty() {
        return ".".to_string();
    }
    if display.chars().count() <= MAX_PATH_LENGTH {
        return display;
    }
    let tail: String = display
        .chars()
        .rev()
        .take(MAX_PATH_LENGTH - 1)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("…{tail}")
}

/// 路径列表显示：首个 + ` +N`。
fn format_paths(paths: &[String]) -> String {
    if paths.is_empty() {
        return ".".to_string();
    }
    let first = shorten_path(&paths[0]);
    if paths.len() > 1 {
        format!("{first} +{}", paths.len() - 1)
    } else {
        first
    }
}

/// glob 徽标。
fn format_glob_badge(globs: &[String]) -> String {
    if globs.is_empty() {
        return String::new();
    }
    let first = &globs[0];
    if globs.len() > 1 {
        format!(" [glob {first} +{}]", globs.len() - 1)
    } else {
        format!(" [glob {first}]")
    }
}

/// 截断原因短描述。
fn truncation_reason(reason: Option<SgTruncationReason>) -> &'static str {
    match reason {
        Some(SgTruncationReason::MaxMatches) => "match limit reached",
        Some(SgTruncationReason::MaxOutputBytes) => "output exceeded 1MB limit",
        Some(SgTruncationReason::Timeout) => "search timed out",
        None => "results truncated",
    }
}

/// 结果 detail 的截断后缀。
fn truncation_suffix(result: &SgResult) -> String {
    if result.truncated {
        format!(" [truncated: {}]", truncation_reason(result.truncated_reason))
    } else {
        String::new()
    }
}

/// 调用行 detail：搜索。
pub fn search_call_detail(
    pattern: &str,
    paths: &[String],
    lang: &str,
    globs: &[String],
    context: Option<i64>,
) -> String {
    let mut text = format!(
        "/{pattern}/ in {} [{lang}]",
        format_paths(paths)
    );
    text.push_str(&format_glob_badge(globs));
    if let Some(context) = context {
        text.push_str(&format!(" [context {context}]"));
    }
    text
}

/// 调用行 detail：改写。
pub fn replace_call_detail(
    pattern: &str,
    rewrite: &str,
    paths: &[String],
    lang: &str,
    globs: &[String],
    dry_run: bool,
) -> String {
    let mut text = format!(
        "/{pattern}/ → {rewrite} in {} [{lang}]",
        format_paths(paths)
    );
    text.push_str(&format_glob_badge(globs));
    if dry_run {
        text.push_str(" [dry-run]");
    }
    text
}

/// 结果行 detail：搜索。
pub fn search_result_detail(result: &SgResult) -> String {
    if let Some(error) = &result.error {
        return format!("Error: {error}");
    }
    if result.matches.is_empty() {
        return "No matches found".to_string();
    }
    let files = distinct_file_count(result);
    format!(
        "{} • {}{}",
        pluralize(result.total_matches, "match", "matches"),
        pluralize(files, "file", "files"),
        truncation_suffix(result)
    )
}

/// 结果行 detail：改写。
pub fn replace_result_detail(result: &SgResult, dry_run: bool) -> String {
    if let Some(error) = &result.error {
        return format!("Error: {error}");
    }
    if result.matches.is_empty() {
        return "No matches found to replace".to_string();
    }
    let files = distinct_file_count(result);
    let summary = if dry_run {
        format!("[DRY RUN] {} previewed", pluralize(result.total_matches, "replacement", "replacements"))
    } else {
        format!("Applied {}", pluralize(result.total_matches, "replacement", "replacements"))
    };
    format!(
        "{summary} • {}{}",
        pluralize(files, "file", "files"),
        truncation_suffix(result)
    )
}

/// 结果中出现的不同文件数。
fn distinct_file_count(result: &SgResult) -> usize {
    let mut files: Vec<&str> = result.matches.iter().map(|m| m.file.as_str()).collect();
    files.sort_unstable();
    files.dedup();
    files.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::CliMatch;

    fn result_with(files: &[&str]) -> SgResult {
        let matches = files
            .iter()
            .map(|f| CliMatch {
                file: (*f).to_string(),
                ..Default::default()
            })
            .collect();
        SgResult {
            total_matches: files.len(),
            matches,
            ..Default::default()
        }
    }

    #[test]
    fn search_call_detail_renders_pattern_and_lang() {
        let detail = search_call_detail(
            "console.log($MSG)",
            &["src".to_string()],
            "typescript",
            &[],
            None,
        );
        assert_eq!(detail, "/console.log($MSG)/ in src [typescript]");
    }

    #[test]
    fn replace_call_detail_marks_dry_run() {
        let detail = replace_call_detail(
            "console.log($MSG)",
            "logger.info($MSG)",
            &["src".to_string()],
            "typescript",
            &[],
            true,
        );
        assert!(detail.contains("→ logger.info($MSG)"));
        assert!(detail.contains("[dry-run]"));
    }

    #[test]
    fn search_result_detail_counts_matches_and_files() {
        let detail = search_result_detail(&result_with(&["a.ts", "a.ts", "b.ts"]));
        assert_eq!(detail, "3 matches • 2 files");
    }

    #[test]
    fn replace_result_detail_dry_run() {
        let detail = replace_result_detail(&result_with(&["a.ts"]), true);
        assert!(detail.starts_with("[DRY RUN] 1 replacement previewed • 1 file"));
    }

    #[test]
    fn replace_result_detail_applied() {
        let detail = replace_result_detail(&result_with(&["a.ts", "b.ts"]), false);
        assert!(detail.starts_with("Applied 2 replacements • 2 files"));
    }

    #[test]
    fn no_match_details() {
        assert_eq!(search_result_detail(&SgResult::default()), "No matches found");
        assert_eq!(
            replace_result_detail(&SgResult::default(), true),
            "No matches found to replace"
        );
    }
}