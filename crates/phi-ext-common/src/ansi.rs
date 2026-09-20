//! ANSI 转义序列处理。
//!
//! 工具输出（尤其是构建、测试、git 输出）常带大量 ANSI 颜色码，去掉它们
//! 可以显著减少 token 消耗，同时不影响语义。

/// 去掉字符串中所有 ANSI 转义序列。
///
/// 覆盖三类序列：
/// - CSI：`ESC [ 参数 中间字节 终止字节`（颜色、光标移动、清屏）
/// - OSC：`ESC ] ... (BEL | ESC \\)`（终端标题、超链接）
/// - 单字符转义：`ESC` + 一个字节（如 `ESC ( B`）
///
/// 输入不含转义序列时返回原字符串的拷贝，避免不必要的分配可通过
/// [`strip_ansi_into`] 复用缓冲区。
pub fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    strip_ansi_into(input, &mut out);
    out
}

/// 文本输出目标。
///
/// 同时覆盖堆上的 [`String`] 与竞技场内的
/// `bumpalo::collections::String`，使剥离逻辑只写一份。
pub trait TextSink {
    /// 追加一段 UTF-8 文本。
    fn push_str(&mut self, s: &str);
    /// 追加一个字符。
    fn push_char(&mut self, c: char);
}

impl TextSink for String {
    fn push_str(&mut self, s: &str) {
        String::push_str(self, s);
    }
    fn push_char(&mut self, c: char) {
        self.push(c);
    }
}

#[cfg(feature = "arena")]
impl TextSink for bumpalo::collections::String<'_> {
    fn push_str(&mut self, s: &str) {
        bumpalo::collections::String::push_str(self, s);
    }
    fn push_char(&mut self, c: char) {
        self.push(c);
    }
}

/// 将去掉 ANSI 转义序列后的结果追加到 `out`。
///
/// 该函数按字节扫描，对 UTF-8 是安全的：只有 `0x1B` 会被识别为转义起始，
/// 而 `0x1B` 永远不会出现在 UTF-8 多字节序列的续字节中。
pub fn strip_ansi_into<S: TextSink + ?Sized>(input: &str, out: &mut S) {
    let bytes = input.as_bytes();
    let mut i = 0usize;
    let mut plain_start = 0usize;

    while i < bytes.len() {
        if bytes[i] != 0x1B {
            i += 1;
            continue;
        }

        // 先把转义序列之前的普通片段原样写出。
        if plain_start < i {
            out.push_str(&input[plain_start..i]);
        }

        i = skip_escape(bytes, i);
        plain_start = i;
    }

    if plain_start < bytes.len() {
        out.push_str(&input[plain_start..]);
    }
}

/// 从 `start`（指向 `ESC`）开始跳过整个转义序列，返回下一个普通字节下标。
fn skip_escape(bytes: &[u8], start: usize) -> usize {
    let after_esc = start + 1;
    if after_esc >= bytes.len() {
        return bytes.len();
    }

    match bytes[after_esc] {
        // CSI: ESC [ ... 终止字节 0x40..=0x7E
        b'[' => {
            let mut i = after_esc + 1;
            while i < bytes.len() {
                let b = bytes[i];
                if (0x40..=0x7E).contains(&b) {
                    return i + 1;
                }
                i += 1;
            }
            bytes.len()
        }
        // OSC: ESC ] ... 由 BEL(0x07) 或 ST(ESC \) 结束
        b']' => {
            let mut i = after_esc + 1;
            while i < bytes.len() {
                if bytes[i] == 0x07 {
                    return i + 1;
                }
                if bytes[i] == 0x1B && i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
                    return i + 2;
                }
                i += 1;
            }
            bytes.len()
        }
        // 其他单字符转义：ESC + 1 个字节（如 ESC ( B 只跳 ESC + '('）
        _ => after_esc + 1,
    }
}

/// 字符串是否包含 ANSI 转义序列。
pub fn has_ansi(input: &str) -> bool {
    input.as_bytes().contains(&0x1B)
}

/// [`strip_ansi`] 的快速版本：不含 `ESC` 时直接返回原串的拷贝。
///
/// 需要「无变化时零分配」的场景请直接用 [`has_ansi`] 做前置判断。
pub fn strip_ansi_fast(input: &str) -> String {
    if !has_ansi(input) {
        return input.to_string();
    }
    strip_ansi(input)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_ansi_should_remove_csi_color_codes() {
        let input = "\u{1b}[31mred\u{1b}[0m text";
        assert_eq!(strip_ansi(input), "red text");
    }

    #[test]
    fn strip_ansi_should_remove_csi_with_multiple_params() {
        let input = "\u{1b}[1;38;5;208mbold orange\u{1b}[0m";
        assert_eq!(strip_ansi(input), "bold orange");
    }

    #[test]
    fn strip_ansi_should_remove_osc_terminated_by_bel() {
        let input = "\u{1b}]0;title\u{7}body";
        assert_eq!(strip_ansi(input), "body");
    }

    #[test]
    fn strip_ansi_should_remove_osc_terminated_by_st() {
        let input = "\u{1b}]8;;https://example.com\u{1b}\\link";
        assert_eq!(strip_ansi(input), "link");
    }

    #[test]
    fn strip_ansi_should_keep_utf8_content_intact() {
        let input = "\u{1b}[32m中文 ✅\u{1b}[0m";
        assert_eq!(strip_ansi(input), "中文 ✅");
    }

    #[test]
    fn strip_ansi_should_return_identical_text_when_no_escape_present() {
        let input = "plain output\nsecond line";
        assert_eq!(strip_ansi(input), input);
    }

    #[test]
    fn strip_ansi_should_drop_unterminated_escape_at_end() {
        assert_eq!(strip_ansi("text\u{1b}[3"), "text");
    }

    #[test]
    fn strip_ansi_should_remove_csi_with_private_parameters() {
        // `ESC[?25l`（隐藏光标）的参数以 `?` 开头：正则式实现常在此漏剥。
        assert_eq!(strip_ansi("\u{1b}[?25lhidden\u{1b}[?25h"), "hidden");
        assert_eq!(strip_ansi("\u{1b}[2Jcleared"), "cleared");
        assert_eq!(strip_ansi("\u{1b}[1;2Hmoved"), "moved");
    }

    #[test]
    fn strip_ansi_fast_should_avoid_rebuilding_clean_text() {
        assert_eq!(strip_ansi_fast("plain"), "plain");
        assert_eq!(strip_ansi_fast("\u{1b}[31mred\u{1b}[0m"), "red");
    }

    #[test]
    fn has_ansi_should_detect_escape_byte() {
        assert!(has_ansi("\u{1b}[0m"));
        assert!(!has_ansi("plain"));
    }
}
