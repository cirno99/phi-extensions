// truncate.rs — 硬字符截断。
//
// 由 pi 版 pi-rtk-optimizer 的 src/techniques/truncate.ts 移植。
//
// 与 pi 版的差异：pi 按 UTF-16 码元计数，这里按 Unicode 字符计数，
// 因此不会在多字节字符中间切断（对中文输出更安全）。

/// 把文本截到 `max_length` 个字符以内，末尾以 `...` 标记被截断。
pub fn truncate(text: &str, max_length: usize) -> String {
    let length = text.chars().count();
    if length <= max_length {
        return text.to_string();
    }
    if max_length < 3 {
        return "...".to_string();
    }
    let kept: String = text.chars().take(max_length - 3).collect();
    format!("{kept}...")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_should_pass_through() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello", 5), "hello");
    }

    #[test]
    fn long_text_should_be_cut_with_suffix() {
        let result = truncate("hello world", 8);
        assert_eq!(result, "hello...");
        assert_eq!(result.chars().count(), 8);
    }

    #[test]
    fn tiny_limit_should_degrade_to_ellipsis() {
        assert_eq!(truncate("hello", 2), "...");
        assert_eq!(truncate("hello", 0), "...");
    }

    #[test]
    fn multibyte_text_should_not_be_split() {
        let text = "中文测试文本内容";
        let result = truncate(text, 5);
        assert_eq!(result, "中文...");
        assert_eq!(result.chars().count(), 5);
    }

    #[test]
    fn empty_input_should_return_empty() {
        assert_eq!(truncate("", 10), "");
    }
}