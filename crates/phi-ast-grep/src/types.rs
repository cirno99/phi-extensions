//! 共享类型与常量。
//!
//! 由 pi 版 pi-ast-grep 的 `src/ast-grep/types.ts` 与 `src/ast-grep/languages.ts`
//! 合并移植：语言清单、超时/截断上限、sg 的 compact JSON 匹配结构。

use serde::Deserialize;

/// ast-grep CLI 支持的 25 种语言名（与 pi 版 `CLI_LANGUAGES` 逐字一致）。
pub const CLI_LANGUAGES: [&str; 25] = [
    "bash",
    "c",
    "cpp",
    "csharp",
    "css",
    "elixir",
    "go",
    "haskell",
    "html",
    "java",
    "javascript",
    "json",
    "kotlin",
    "lua",
    "nix",
    "php",
    "python",
    "ruby",
    "rust",
    "scala",
    "solidity",
    "swift",
    "typescript",
    "tsx",
    "yaml",
];

/// 判定语言名是否在支持清单内。
pub fn is_cli_language(value: &str) -> bool {
    CLI_LANGUAGES.contains(&value)
}

/// 默认超时：5 分钟（对应 pi 版 `DEFAULT_TIMEOUT_MS`）。
pub const DEFAULT_TIMEOUT_MS: u64 = 300_000;

/// 输出超过该字节数即视为截断（对应 pi 版 `DEFAULT_MAX_OUTPUT_BYTES`）。
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 1024 * 1024;

/// 单次返回的最大匹配数（对应 pi 版 `DEFAULT_MAX_MATCHES`）。
pub const DEFAULT_MAX_MATCHES: usize = 500;

/// 源码位置（1-based 由调用方 +1 得到）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Position {
    pub line: i64,
    pub column: i64,
}

/// 字节偏移区间。
// 这些字段仅用于**反序列化校验**（缺字段即整体解析失败），运行时不一定读取。
#[allow(dead_code)]
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ByteOffset {
    pub start: i64,
    pub end: i64,
}

/// 匹配区间。
#[allow(dead_code)]
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Range {
    pub start: Position,
    pub end: Position,
    #[serde(rename = "byteOffset")]
    pub byte_offset: ByteOffset,
}

/// 行内首尾字符计数。
#[allow(dead_code)]
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CharCount {
    pub leading: i64,
    pub trailing: i64,
}

/// sg `--json=compact` 返回的单条匹配。
///
/// 字段与 pi 版 `isCliMatch` 的校验集合一致；额外字段（`replacement`、
/// `metaVariables` 等）由 serde 默认忽略。缺任一必填字段即整体解析失败，
/// 与 pi 的「一条不合法则整数组作废」语义对齐。
#[allow(dead_code)]
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CliMatch {
    pub text: String,
    pub range: Range,
    pub file: String,
    pub lines: String,
    #[serde(rename = "charCount")]
    pub char_count: CharCount,
    pub language: String,
}

/// 截断原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SgTruncationReason {
    /// 匹配数超过上限。
    MaxMatches,
    /// 输出超过 1MB。
    MaxOutputBytes,
    /// 搜索超时。
    Timeout,
}


/// sg 调用的归一化结果。
#[derive(Debug, Clone, Default)]
pub struct SgResult {
    pub matches: Vec<CliMatch>,
    pub total_matches: usize,
    pub truncated: bool,
    pub truncated_reason: Option<SgTruncationReason>,
    pub error: Option<String>,
}

/// 一次 `sg run` 的入参。
#[derive(Debug, Clone, Default)]
pub struct RunSgOptions {
    pub pattern: String,
    pub lang: String,
    pub paths: Vec<String>,
    pub globs: Vec<String>,
    pub rewrite: Option<String>,
    pub context: Option<i64>,
    /// 是否真正落盘（对应 pi 版 `updateAll`；`--update-all`）。
    pub update_all: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_languages_has_25_entries() {
        assert_eq!(CLI_LANGUAGES.len(), 25);
    }

    #[test]
    fn is_cli_language_accepts_known_and_rejects_unknown() {
        assert!(is_cli_language("rust"));
        assert!(is_cli_language("typescript"));
        assert!(!is_cli_language("Rust"));
        assert!(!is_cli_language("brainfuck"));
    }
}