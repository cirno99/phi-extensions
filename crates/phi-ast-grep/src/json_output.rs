//! sg `--json=compact` 输出解析。
//!
//! 由 pi 版 pi-ast-grep 的 `src/ast-grep/json-output.ts` 移植：处理空输出、
//! 1MB 截断，以及被截断的 JSON 数组「抢救」（找到最后一个 `},` 后补 `]`）。
//!
//! 与 pi 的差异：pi 用 JS `substring`（按 UTF-16 码元切），可能切裂代理对；
//! 这里按字节切并把切点回退到 UTF-8 字符边界，避免 panic。

use crate::types::{
    CliMatch, SgResult, SgTruncationReason, DEFAULT_MAX_MATCHES, DEFAULT_MAX_OUTPUT_BYTES,
};

/// 从 sg 的 stdout 解析出归一化结果。
pub fn create_sg_result_from_stdout(stdout: &str) -> SgResult {
    if stdout.trim().is_empty() {
        return SgResult::default();
    }

    let output_truncated = stdout.len() >= DEFAULT_MAX_OUTPUT_BYTES;
    let output_to_process = if output_truncated {
        floor_char_boundary(stdout, DEFAULT_MAX_OUTPUT_BYTES)
    } else {
        stdout
    };

    let matches: Vec<CliMatch> = match parse_matches(output_to_process) {
        Some(matches) => matches,
        None => {
            if !output_truncated {
                return SgResult::default();
            }
            match salvage_truncated_json(output_to_process) {
                Some(matches) => matches,
                None => {
                    return SgResult {
                        matches: Vec::new(),
                        total_matches: 0,
                        truncated: true,
                        truncated_reason: Some(SgTruncationReason::MaxOutputBytes),
                        error: Some("Output too large and could not be parsed".to_string()),
                    };
                }
            }
        }
    };

    let total_matches = matches.len();
    let matches_truncated = total_matches > DEFAULT_MAX_MATCHES;
    let final_matches = if matches_truncated {
        matches[..DEFAULT_MAX_MATCHES].to_vec()
    } else {
        matches
    };

    let truncated_reason = if output_truncated {
        Some(SgTruncationReason::MaxOutputBytes)
    } else if matches_truncated {
        Some(SgTruncationReason::MaxMatches)
    } else {
        None
    };

    SgResult {
        matches: final_matches,
        total_matches,
        truncated: output_truncated || matches_truncated,
        truncated_reason,
        error: None,
    }
}

/// 解析匹配数组；任一元素不合法则整体失败（对齐 pi 的 `isCliMatchArray`）。
fn parse_matches(text: &str) -> Option<Vec<CliMatch>> {
    phi_ext_common::json::parse::<Vec<CliMatch>>(text.as_bytes()).ok()
}

/// 被截断的 JSON 数组抢救：定位最后一个 `},`，截到其 `}` 并补 `]` 再解析。
fn salvage_truncated_json(text: &str) -> Option<Vec<CliMatch>> {
    let last_valid_index = text.rfind('}')?;
    if last_valid_index == 0 {
        return None;
    }
    // 搜索范围需覆盖到 `last_valid_index` 处的 `},`（匹配起点即该 `}`）；
    // 用 floor_char_boundary 保证切片落在 UTF-8 字符边界上。
    let search_end = (last_valid_index + 2).min(text.len());
    let bracket_index = floor_char_boundary(text, search_end).rfind("},")?;
    if bracket_index == 0 {
        return None;
    }
    let mut truncated = String::with_capacity(bracket_index + 2);
    truncated.push_str(&text[..bracket_index + 1]);
    truncated.push(']');
    parse_matches(&truncated)
}

/// 把字节下标回退到最近的 UTF-8 字符边界。
fn floor_char_boundary(text: &str, index: usize) -> &str {
    if index >= text.len() {
        return text;
    }
    let mut end = index;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"[{"text":"console.log(\"a\")","range":{"byteOffset":{"start":0,"end":16},"start":{"line":0,"column":0},"end":{"line":0,"column":16}},"file":"s.ts","lines":"console.log(\"a\");","charCount":{"leading":0,"trailing":1},"language":"TypeScript"}]"#;

    #[test]
    fn empty_output_yields_empty_result() {
        let result = create_sg_result_from_stdout("   \n");
        assert!(result.matches.is_empty());
        assert_eq!(result.total_matches, 0);
        assert!(!result.truncated);
    }

    #[test]
    fn parses_valid_array() {
        let result = create_sg_result_from_stdout(SAMPLE);
        assert_eq!(result.matches.len(), 1);
        assert_eq!(result.total_matches, 1);
        assert_eq!(result.matches[0].file, "s.ts");
        assert_eq!(result.matches[0].range.start.line, 0);
        assert!(!result.truncated);
    }

    #[test]
    fn invalid_json_without_truncation_is_empty() {
        let result = create_sg_result_from_stdout("{not json}");
        assert!(result.matches.is_empty());
        assert!(!result.truncated);
        assert!(result.error.is_none());
    }

    #[test]
    fn salvages_truncated_array() {
        // 截断在第二条匹配中间：应抢救出第一条。
        // 注意：抢救只在输出 >1MB（`output_truncated`）时触发，故这里直接测抢救函数。
        let truncated = format!("{},{{\"text\":\"partial", &SAMPLE[..SAMPLE.len() - 1]);
        let matches = salvage_truncated_json(&truncated).expect("应抢救出第一条");
        assert_eq!(matches.len(), 1);
    }

    #[test]
    fn oversized_unparseable_output_reports_error() {
        // 超过 1MB 且无法解析：应返回带 error 的截断结果。
        let big = "x".repeat(DEFAULT_MAX_OUTPUT_BYTES + 10);
        let result = create_sg_result_from_stdout(&big);
        assert!(result.truncated);
        assert_eq!(result.truncated_reason, Some(SgTruncationReason::MaxOutputBytes));
        assert!(result.error.is_some());
    }

    #[test]
    fn caps_matches_at_max() {
        let one = &SAMPLE[1..SAMPLE.len() - 1];
        let many = format!(
            "[{}]",
            std::iter::repeat(one)
                .take(DEFAULT_MAX_MATCHES + 10)
                .collect::<Vec<_>>()
                .join(",")
        );
        let result = create_sg_result_from_stdout(&many);
        assert_eq!(result.matches.len(), DEFAULT_MAX_MATCHES);
        assert_eq!(result.total_matches, DEFAULT_MAX_MATCHES + 10);
        assert_eq!(result.truncated_reason, Some(SgTruncationReason::MaxMatches));
    }

    #[test]
    fn floor_char_boundary_never_panics_on_multibyte() {
        let text = "aé漢";
        let cut = floor_char_boundary(text, 2); // 落在 'é' 中间
        assert!(text.starts_with(cut));
    }
}