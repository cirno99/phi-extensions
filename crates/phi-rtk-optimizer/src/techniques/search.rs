// search.rs — grep 结果分组：按文件聚合命中行。
//
// 由 pi 版 pi-rtk-optimizer 的 src/techniques/search.ts 移植。

use std::collections::BTreeMap;
use std::sync::LazyLock;

use regex::Regex;

use super::path_utils::compact_path;

/// `path:line:content` 形式（line 可缺省）。
static RESULT_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(.+?):(\d+)?:(.+)$").expect("搜索结果正则应可编译")
});

/// 单条命中。
struct SearchResult {
    file: String,
    line_number: String,
    content: String,
}

/// 把 grep 输出按文件分组；无法识别出任何命中时返回 `None`。
pub fn group_search_results(output: &str, max_results: usize) -> Option<String> {
    let mut results: Vec<SearchResult> = Vec::new();

    for line in output.split('\n') {
        if line.trim().is_empty() {
            continue;
        }
        let Some(captures) = RESULT_PATTERN.captures(line) else {
            continue;
        };
        results.push(SearchResult {
            file: captures
                .get(1)
                .map_or_else(|| "unknown".to_string(), |m| m.as_str().to_string()),
            line_number: captures
                .get(2)
                .map_or_else(|| "?".to_string(), |m| m.as_str().to_string()),
            content: captures
                .get(3)
                .map_or_else(String::new, |m| m.as_str().to_string()),
        });
    }

    if results.is_empty() {
        return None;
    }

    let mut by_file: BTreeMap<String, Vec<&SearchResult>> = BTreeMap::new();
    for result in &results {
        by_file.entry(result.file.clone()).or_default().push(result);
    }

    let mut output_text = format!("{} matches in {} files:\n\n", results.len(), by_file.len());

    let mut shown = 0usize;
    for (file, matches) in &by_file {
        if shown >= max_results {
            break;
        }
        output_text.push_str(&format!(
            "> {} ({} matches):\n",
            compact_path(file, 50),
            matches.len()
        ));
        for hit in matches.iter().take(10) {
            let mut cleaned = hit.content.trim().to_string();
            if cleaned.chars().count() > 70 {
                cleaned = format!("{}...", cleaned.chars().take(67).collect::<String>());
            }
            output_text.push_str(&format!("    {}: {cleaned}\n", hit.line_number));
            shown += 1;
        }
        if matches.len() > 10 {
            output_text.push_str(&format!("  +{} more\n", matches.len() - 10));
        }
        output_text.push('\n');
    }

    if results.len() > shown {
        output_text.push_str(&format!("... +{} more\n", results.len() - shown));
    }

    Some(output_text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_group_hits_by_file() {
        let output = "\
src/a.rs:12:let x = 1;\n\
src/a.rs:30:let y = 2;\n\
src/b.rs:5:fn main() {}\n\
some noise line without colon-separated hits";
        let result = group_search_results(output, 50).expect("应分组");
        assert!(result.starts_with("3 matches in 2 files:"), "got {result}");
        assert!(result.contains("> src/a.rs (2 matches):"), "got {result}");
        assert!(result.contains("    12: let x = 1;"), "got {result}");
        assert!(result.contains("> src/b.rs (1 matches):"), "got {result}");
    }

    #[test]
    fn should_return_none_without_hits() {
        assert!(group_search_results("", 50).is_none());
        assert!(group_search_results("no colons here", 50).is_none());
    }

    #[test]
    fn should_cap_shown_results() {
        let mut output = String::new();
        for index in 0..10 {
            output.push_str(&format!("file{index}.rs:1:hit\n"));
        }
        let result = group_search_results(&output, 3).expect("应分组");
        assert!(result.contains("... +7 more"), "got {result}");
    }

    #[test]
    fn long_lines_should_be_trimmed_to_70_chars() {
        let long = "x".repeat(100);
        let output = format!("src/a.rs:1:{long}");
        let result = group_search_results(&output, 50).expect("应分组");
        assert!(result.contains(&format!("{}...", "x".repeat(67))), "got {result}");
    }

    #[test]
    fn missing_line_number_should_render_question_mark() {
        let result = group_search_results("src/a.rs::content", 50).expect("应分组");
        assert!(result.contains("    ?: content"), "got {result}");
    }
}