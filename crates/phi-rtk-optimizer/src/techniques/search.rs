// search.rs — grep 结果分组：按文件聚合命中行。
//
// 由 pi 版 pi-rtk-optimizer 的 src/techniques/search.ts 移植。
//
// 与 pi 版的差异（性能）：命中记录借用输入 `&str`，分组表与输出缓冲
// 放进竞技场；只有需要截断的长行才在竞技场里生成新字符串。

use std::collections::BTreeMap;
use std::fmt::Write;
use std::sync::LazyLock;

use bumpalo::collections::{String as ArenaString, Vec as ArenaVec};
use bumpalo::Bump;
use regex::Regex;

use phi_ext_common::arena::split_lines;

use super::command_detection::matches_normalized_patterns;
use super::path_utils::compact_path;

/// `path:line:content` 形式（line 可缺省）。
static RESULT_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(.+?):(\d+)?:(.+)$").expect("搜索结果正则应可编译")
});

/// ast-grep 搜索命令（CLI 名为 `ast-grep` 或 `sg`）。
static SEARCH_COMMAND_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [r"^(?:ast-grep|sg)\b"]
        .iter()
        .map(|p| Regex::new(p).expect("搜索命令正则应可编译"))
        .collect()
});

/// 判断归一化命令是否为 ast-grep 搜索命令（`sg` / `ast-grep`）。
pub fn is_search_command(normalized: Option<&str>) -> bool {
    matches_normalized_patterns(normalized, &SEARCH_COMMAND_PATTERNS)
}

/// 单条命中（字段借用输入）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SearchResult<'a> {
    file: &'a str,
    line_number: &'a str,
    content: &'a str,
}

/// 把 grep 输出按文件分组；无法识别出任何命中时返回 `None`。
pub fn group_search_results(arena: &Bump, output: &str, max_results: usize) -> Option<String> {
    let lines = split_lines(arena, output);
    let mut results: ArenaVec<SearchResult<'_>> = ArenaVec::new_in(arena);

    for line in lines.iter().copied() {
        if line.trim().is_empty() {
            continue;
        }
        // 结果行必然形如 `path:line:content`，先做零成本快速排除。
        if !line.contains(':') {
            continue;
        }
        let Some(captures) = RESULT_PATTERN.captures(line) else {
            continue;
        };
        results.push(SearchResult {
            file: captures.get(1).map_or("unknown", |m| m.as_str()),
            line_number: captures.get(2).map_or("?", |m| m.as_str()),
            content: captures.get(3).map_or("", |m| m.as_str()),
        });
    }

    if results.is_empty() {
        return None;
    }

    let mut by_file: BTreeMap<&str, ArenaVec<&SearchResult<'_>>> = BTreeMap::new();
    for result in results.iter() {
        by_file
            .entry(result.file)
            .or_insert_with(|| ArenaVec::new_in(arena))
            .push(result);
    }

    let mut out = ArenaString::new_in(arena);
    let _ = write!(out, "{} matches in {} files:", results.len(), by_file.len());
    out.push_str("\n\n");

    let mut shown = 0usize;
    for (file, matches) in &by_file {
        if shown >= max_results {
            break;
        }
        let _ = writeln!(
            out,
            "> {} ({} matches):",
            compact_path(arena, file, 50),
            matches.len()
        );
        for hit in matches.iter().take(10) {
            let cleaned = clean_line(arena, hit.content);
            let _ = writeln!(out, "    {}: {cleaned}", hit.line_number);
            shown += 1;
        }
        if matches.len() > 10 {
            let _ = writeln!(out, "  +{} more", matches.len() - 10);
        }
        out.push('\n');
    }

    if results.len() > shown {
        let _ = writeln!(out, "... +{} more", results.len() - shown);
    }

    Some(out.into_bump_str().to_string())
}

/// 去掉首尾空白；超过 70 字符时截到 67 字符并追加 `...`。
fn clean_line<'a>(arena: &'a Bump, content: &'a str) -> &'a str {
    let trimmed = content.trim();
    for (taken, (index, _)) in trimmed.char_indices().enumerate() {
        if taken == 70 {
            let mut out = ArenaString::new_in(arena);
            out.push_str(&trimmed[..index]);
            out.push_str("...");
            return out.into_bump_str();
        }
    }
    trimmed
}

#[cfg(test)]
mod tests {
    use super::*;
    use phi_ext_common::arena::Scratch;

    fn group(output: &str, max: usize) -> Option<String> {
        let scratch = Scratch::with_capacity(1024);
        group_search_results(scratch.arena(), output, max)
    }

    #[test]
    fn should_group_hits_by_file() {
        let output = "\
src/a.rs:12:let x = 1;\n\
src/a.rs:30:let y = 2;\n\
src/b.rs:5:fn main() {}\n\
some noise line without colon-separated hits";
        let result = group(output, 50).expect("应分组");
        assert!(result.starts_with("3 matches in 2 files:"), "got {result}");
        assert!(result.contains("> src/a.rs (2 matches):"), "got {result}");
        assert!(result.contains("    12: let x = 1;"), "got {result}");
        assert!(result.contains("> src/b.rs (1 matches):"), "got {result}");
    }

    #[test]
    fn should_return_none_without_hits() {
        assert!(group("", 50).is_none());
        assert!(group("no colons here", 50).is_none());
    }

    #[test]
    fn should_cap_shown_results() {
        let mut output = String::new();
        for index in 0..10 {
            output.push_str(&format!("file{index}.rs:1:hit\n"));
        }
        let result = group(&output, 3).expect("应分组");
        assert!(result.contains("... +7 more"), "got {result}");
    }

    #[test]
    fn long_lines_should_be_trimmed_to_70_chars() {
        let long = "x".repeat(100);
        let output = format!("src/a.rs:1:{long}");
        let result = group(&output, 50).expect("应分组");
        assert!(
            result.contains(&format!("{}...", "x".repeat(67))),
            "got {result}"
        );
    }

    #[test]
    fn missing_line_number_should_render_question_mark() {
        let result = group("src/a.rs::content", 50).expect("应分组");
        assert!(result.contains("    ?: content"), "got {result}");
    }

    #[test]
    fn clean_line_should_borrow_when_short() {
        let scratch = Scratch::with_capacity(64);
        assert_eq!(clean_line(scratch.arena(), "  short  "), "short");
        assert_eq!(clean_line(scratch.arena(), "中文"), "中文");
    }
}