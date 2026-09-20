// ansi.rs — 去除 ANSI 转义序列。
//
// 由 pi 版 pi-rtk-optimizer 的 src/techniques/ansi.ts 移植。
// 覆盖 CSI（`ESC [ ... 字母`）与 OSC（`ESC ] ... BEL` 或 `ESC \`）两类。

use std::sync::LazyLock;

use regex::Regex;

static ANSI_CSI: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\x1b\[[0-9;]*[a-zA-Z]").expect("CSI 正则应可编译")
});
static ANSI_OSC_NUMERIC: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\x1b\][0-9;]*(?:\x07|\x1b\\)").expect("OSC 正则应可编译")
});
static ANSI_OSC_TEXT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)").expect("OSC 正则应可编译")
});

/// 去除全部 ANSI 转义序列。
pub fn strip_ansi(text: &str) -> String {
    let text = ANSI_CSI.replace_all(text, "");
    let text = ANSI_OSC_NUMERIC.replace_all(&text, "");
    ANSI_OSC_TEXT.replace_all(&text, "").into_owned()
}

/// 快速版本：不含 `ESC` 时直接返回原串。
pub fn strip_ansi_fast(text: &str) -> String {
    if !text.contains('\x1b') {
        return text.to_string();
    }
    strip_ansi(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_strip_csi_color_codes() {
        assert_eq!(strip_ansi("\x1b[31merror\x1b[0m"), "error");
        assert_eq!(strip_ansi("\x1b[1;38;2;1;2;3mbold\x1b[0m"), "bold");
    }

    #[test]
    fn should_strip_osc_with_bel_terminator() {
        assert_eq!(strip_ansi("\x1b]0;title\x07after"), "after");
    }

    #[test]
    fn should_strip_osc_with_st_terminator() {
        assert_eq!(strip_ansi("\x1b]8;;http://x\x1b\\link"), "link");
    }

    #[test]
    fn plain_text_should_be_unchanged() {
        assert_eq!(strip_ansi("hello world"), "hello world");
        assert_eq!(strip_ansi_fast("hello world"), "hello world");
    }

    #[test]
    fn fast_path_should_still_strip_when_escape_present() {
        assert_eq!(strip_ansi_fast("\x1b[32mok\x1b[0m"), "ok");
    }

    #[test]
    fn should_preserve_multiline_structure() {
        let input = "\x1b[31mline1\x1b[0m\nline2\n";
        assert_eq!(strip_ansi(input), "line1\nline2\n");
    }
}