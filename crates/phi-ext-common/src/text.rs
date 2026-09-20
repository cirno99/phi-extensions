//! 文本格式化与截断工具。
//!
//! 这些函数被多个扩展共用：输出压缩需要安全截断（不能切断 UTF-8 或破坏
//! 行结构），statusline 需要 token / 时长格式化。

/// 截断结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Truncation {
    /// 截断后的文本。
    pub text: String,
    /// 是否发生了截断。
    pub truncated: bool,
    /// 被移除的字符数。
    pub removed_chars: usize,
}

impl Truncation {
    /// 未截断时的结果。
    fn unchanged(text: &str) -> Self {
        Self {
            text: text.to_string(),
            truncated: false,
            removed_chars: 0,
        }
    }
}

/// 按字符数截断（不是字节数），保证不切坏 UTF-8。
///
/// 截断时在末尾追加 `suffix`（通常是一个提示语），并且 `suffix` 的长度
/// 计入 `max_chars`，保证最终结果不超过上限。
pub fn truncate_chars(input: &str, max_chars: usize, suffix: &str) -> Truncation {
    let total = input.chars().count();
    if total <= max_chars {
        return Truncation::unchanged(input);
    }

    let suffix_chars = suffix.chars().count();
    // 后缀比上限还长时，退化为纯后缀，避免产出负长度。
    let keep = max_chars.saturating_sub(suffix_chars);
    let mut text: String = input.chars().take(keep).collect();
    text.push_str(suffix);

    Truncation {
        removed_chars: total - keep,
        text,
        truncated: true,
    }
}

/// 按行数截断，保留前 `max_lines` 行。
///
/// 末尾会追加 `suffix` 说明被省略的行数；`suffix` 单独占一行。
pub fn truncate_lines(input: &str, max_lines: usize, suffix: &str) -> Truncation {
    let total_lines = input.lines().count();
    if total_lines <= max_lines {
        return Truncation::unchanged(input);
    }

    let kept: Vec<&str> = input.lines().take(max_lines).collect();
    let removed_lines = total_lines - max_lines;
    let removed_chars = input.chars().count() - kept.iter().map(|l| l.chars().count()).sum::<usize>();

    let mut text = kept.join("\n");
    text.push('\n');
    text.push_str(&suffix.replace("{n}", &removed_lines.to_string()));

    Truncation {
        text,
        truncated: true,
        removed_chars,
    }
}

/// 文本行数（空串算 0 行）。
pub fn line_count(input: &str) -> usize {
    if input.is_empty() {
        0
    } else {
        input.lines().count()
    }
}

/// 文本字符数。
///
/// 纯 ASCII 走字节数快路径（`str::is_ascii` 按机器字 / SIMD 扫描），
/// 避免 `chars().count()` 对绝大多数构建 / 日志输出的全量 UTF-8 解码。
pub fn char_count(input: &str) -> usize {
    if input.is_ascii() {
        input.len()
    } else {
        input.chars().count()
    }
}

/// 判断一行是否是 `LINE:HASH` 锚点行（例如 `12:9f3ac1|code` 或 `12:9f3ac1`）。
///
/// RTK 的 read 压缩必须保留这类锚点行，否则后续编辑会失去定位依据。
pub fn is_anchor_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    let Some(colon) = trimmed.find(':') else {
        return false;
    };
    if colon == 0 {
        return false;
    }
    let (digits, rest) = trimmed.split_at(colon);
    if !digits.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    let rest = &rest[1..];
    // 只取长度，无需为十六进制前缀分配一个 String。
    let hash_len = rest
        .chars()
        .take_while(|c| c.is_ascii_hexdigit())
        .count();
    hash_len >= 4
}

/// 以人类可读形式格式化 token 数：`1.2B` / `3.4M` / `12K` / `999`。
///
/// 小于 1000 的整数不带小数；带单位时保留一位小数并去掉多余的 `.0`。
pub fn format_tokens(count: u64) -> String {
    const K: u64 = 1_000;
    const M: u64 = 1_000_000;
    const B: u64 = 1_000_000_000;

    match count {
        c if c >= B => format_scaled(c, B, "B"),
        c if c >= M => format_scaled(c, M, "M"),
        c if c >= K => format_scaled(c, K, "K"),
        c => c.to_string(),
    }
}

fn format_scaled(count: u64, unit: u64, suffix: &str) -> String {
    let tenths = (count * 10 + unit / 2) / unit;
    let whole = tenths / 10;
    let frac = tenths % 10;
    if frac == 0 {
        format!("{whole}{suffix}")
    } else {
        format!("{whole}.{frac}{suffix}")
    }
}

/// 以人类可读形式格式化毫秒时长。
///
/// - `< 1000ms` → `123ms`
/// - `< 60s` → `4.5s`
/// - 其他 → `2m 5s`
pub fn format_duration_ms(ms: u64) -> String {
    if ms < 1_000 {
        return format!("{ms}ms");
    }
    if ms < 60_000 {
        let tenths = (ms + 50) / 100;
        let whole = tenths / 10;
        let frac = tenths % 10;
        if frac == 0 {
            return format!("{whole}s");
        }
        return format!("{whole}.{frac}s");
    }
    let total_secs = (ms + 500) / 1_000;
    format!("{}m {}s", total_secs / 60, total_secs % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_chars_should_keep_input_when_within_limit() {
        let result = truncate_chars("abc", 10, "...");
        assert!(!result.truncated);
        assert_eq!(result.text, "abc");
        assert_eq!(result.removed_chars, 0);
    }

    #[test]
    fn truncate_chars_should_include_suffix_within_limit() {
        let result = truncate_chars("abcdefghij", 6, "...");
        assert!(result.truncated);
        assert_eq!(result.text, "abc...");
        assert_eq!(result.text.chars().count(), 6);
        assert_eq!(result.removed_chars, 7);
    }

    #[test]
    fn truncate_chars_should_not_split_multibyte_chars() {
        // 5 个字符截到 4 个字符，后缀占 1 个字符，因此保留前 3 个字符。
        let result = truncate_chars("中文字符串", 4, "…");
        assert_eq!(result.text, "中文字…");
        assert_eq!(result.text.chars().count(), 4);
    }

    #[test]
    fn truncate_chars_should_degrade_to_suffix_when_suffix_exceeds_limit() {
        let result = truncate_chars("abcdef", 2, "....");
        assert_eq!(result.text, "....");
        assert!(result.truncated);
    }

    #[test]
    fn truncate_lines_should_keep_all_lines_when_within_limit() {
        let result = truncate_lines("a\nb\nc", 5, "[+{n} lines]");
        assert!(!result.truncated);
        assert_eq!(result.text, "a\nb\nc");
    }

    #[test]
    fn truncate_lines_should_report_removed_line_count() {
        let result = truncate_lines("a\nb\nc\nd", 2, "[+{n} lines]");
        assert!(result.truncated);
        assert_eq!(result.text, "a\nb\n[+2 lines]");
    }

    #[test]
    fn line_count_should_treat_empty_input_as_zero() {
        assert_eq!(line_count(""), 0);
        assert_eq!(line_count("a\n"), 1);
        assert_eq!(line_count("a\nb"), 2);
    }

    #[test]
    fn is_anchor_line_should_accept_line_hash_pairs() {
        assert!(is_anchor_line("12:9f3ac1|fn main() {"));
        assert!(is_anchor_line("  1:abcdef"));
    }

    #[test]
    fn is_anchor_line_should_reject_plain_and_prose_lines() {
        assert!(!is_anchor_line("fn main() {"));
        assert!(!is_anchor_line("error: something"));
        assert!(!is_anchor_line(":abc"));
        assert!(!is_anchor_line("12:ab"));
    }

    #[test]
    fn format_tokens_should_use_units_without_trailing_zero() {
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(12_000), "12K");
        assert_eq!(format_tokens(1_200_000), "1.2M");
        assert_eq!(format_tokens(1_250_000), "1.3M");
        assert_eq!(format_tokens(3_000_000_000), "3B");
    }

    #[test]
    fn format_duration_ms_should_switch_units_at_thresholds() {
        assert_eq!(format_duration_ms(0), "0ms");
        assert_eq!(format_duration_ms(999), "999ms");
        assert_eq!(format_duration_ms(1_000), "1s");
        assert_eq!(format_duration_ms(4_500), "4.5s");
        assert_eq!(format_duration_ms(60_000), "1m 0s");
        assert_eq!(format_duration_ms(125_000), "2m 5s");
    }
}
