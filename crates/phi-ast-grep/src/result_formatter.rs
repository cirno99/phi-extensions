//! 结果文本格式化。
//!
//! 由 pi 版 pi-ast-grep 的 `src/ast-grep/result-formatter.ts` 逐字移植。

use crate::types::{SgResult, SgTruncationReason};

/// 截断原因的可读描述。
fn format_truncation_reason(result: &SgResult) -> String {
    match result.truncated_reason {
        Some(SgTruncationReason::MaxMatches) => {
            format!(
                "showing first {} of {}",
                result.matches.len(),
                result.total_matches
            )
        }
        Some(SgTruncationReason::MaxOutputBytes) => "output exceeded 1MB limit".to_string(),
        _ => "search timed out".to_string(),
    }
}

/// 搜索结果的文本。
pub fn format_search_result(result: &SgResult) -> String {
    if let Some(error) = &result.error {
        return format!("Error: {error}");
    }

    if result.matches.is_empty() {
        return "No matches found".to_string();
    }

    let mut lines: Vec<String> = Vec::new();

    if result.truncated {
        lines.push(format!(
            "[TRUNCATED] Results truncated ({})\n",
            format_truncation_reason(result)
        ));
    }

    let truncation_note = if result.truncated {
        format!(" (truncated from {})", result.total_matches)
    } else {
        String::new()
    };
    lines.push(format!(
        "Found {} match(es){}:\n",
        result.matches.len(),
        truncation_note
    ));

    for m in &result.matches {
        let loc = format!(
            "{}:{}:{}",
            m.file,
            m.range.start.line + 1,
            m.range.start.column + 1
        );
        lines.push(loc);
        lines.push(format!("  {}", m.lines.trim()));
        lines.push(String::new());
    }

    lines.join("\n")
}

/// 改写结果的文本。
pub fn format_replace_result(result: &SgResult, is_dry_run: bool) -> String {
    if let Some(error) = &result.error {
        return format!("Error: {error}");
    }

    if result.matches.is_empty() {
        return "No matches found to replace".to_string();
    }

    let prefix = if is_dry_run { "[DRY RUN] " } else { "" };
    let mut lines: Vec<String> = Vec::new();

    if result.truncated {
        lines.push(format!(
            "[TRUNCATED] Results truncated ({})\n",
            format_truncation_reason(result)
        ));
    }

    lines.push(format!(
        "{}{} replacement(s):\n",
        prefix,
        result.matches.len()
    ));

    for m in &result.matches {
        let loc = format!(
            "{}:{}:{}",
            m.file,
            m.range.start.line + 1,
            m.range.start.column + 1
        );
        lines.push(loc);
        lines.push(format!("  {}", m.text));
        lines.push(String::new());
    }

    if is_dry_run {
        lines.push("Use dryRun=false to apply changes".to_string());
    }

    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{CliMatch, Position, Range};

    fn sample_result(count: usize) -> SgResult {
        let matches = (0..count)
            .map(|i| CliMatch {
                text: format!("console.log(\"{i}\")"),
                file: "src/foo.ts".to_string(),
                lines: format!("  console.log(\"{i}\");"),
                range: Range {
                    start: Position {
                        line: i as i64,
                        column: 2,
                    },
                    ..Default::default()
                },
                ..Default::default()
            })
            .collect();
        SgResult {
            total_matches: count,
            matches,
            ..Default::default()
        }
    }

    #[test]
    fn search_no_matches() {
        assert_eq!(format_search_result(&SgResult::default()), "No matches found");
    }

    #[test]
    fn search_error_is_prefixed() {
        let result = SgResult {
            error: Some("boom".to_string()),
            ..Default::default()
        };
        assert_eq!(format_search_result(&result), "Error: boom");
    }

    #[test]
    fn search_lists_location_and_line() {
        let text = format_search_result(&sample_result(1));
        assert!(text.contains("Found 1 match(es):"));
        assert!(text.contains("src/foo.ts:1:3"));
        assert!(text.contains("console.log(\"0\");"));
    }

    #[test]
    fn replace_dry_run_has_prefix_and_footer() {
        let text = format_replace_result(&sample_result(2), true);
        assert!(text.contains("[DRY RUN] 2 replacement(s):"));
        assert!(text.contains("Use dryRun=false to apply changes"));
    }

    #[test]
    fn replace_apply_has_no_dry_run_markers() {
        let text = format_replace_result(&sample_result(2), false);
        assert!(!text.contains("[DRY RUN]"));
        assert!(!text.contains("Use dryRun=false to apply changes"));
        assert!(text.contains("2 replacement(s):"));
    }

    #[test]
    fn replace_no_matches() {
        assert_eq!(
            format_replace_result(&SgResult::default(), true),
            "No matches found to replace"
        );
    }
}