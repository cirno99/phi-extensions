// path_utils.rs — 路径压缩（超长路径折叠为 `…/父目录/末段`）。
//
// 由 pi 版 pi-rtk-optimizer 的 src/techniques/path-utils.ts 移植。
//
// 与 pi 版的差异（性能）：结果直接写进竞技场并借用返回，
// 不再为每个候选/每行路径都向全局分配器申请 `String`。

use bumpalo::collections::String as ArenaString;
use bumpalo::Bump;

/// 路径分隔符：只有反斜杠、没有正斜杠时按 Windows 处理。
fn detect_path_separator(path: &str) -> char {
    if path.contains('\\') && !path.contains('/') {
        '\\'
    } else {
        '/'
    }
}

/// 提取路径前缀（盘符 / UNC 主机 / 根斜杠），**包含**尾部的分隔符。
fn detect_path_prefix(path: &str) -> &str {
    let bytes = path.as_bytes();
    if bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
    {
        // `C:\`：盘符 + 冒号 + 分隔符（三者均为 ASCII，按字节切片安全）。
        return &path[..3];
    }

    if path.starts_with("\\\\") || path.starts_with("//") {
        // UNC：跳过两个前导分隔符，再跳过「主机」与「共享名」后的两个分隔符。
        let mut separators = 0usize;
        for (index, character) in path.char_indices() {
            if character == '/' || character == '\\' {
                separators += 1;
                if separators == 4 {
                    return &path[..index + 1];
                }
            }
        }
        return &path[..2.min(path.len())];
    }

    if path.starts_with('/') || path.starts_with('\\') {
        return &path[..1];
    }

    ""
}

/// 在竞技场里拼接 `prefix + … + [prev + sep] + last`。
fn build_candidate<'a>(
    arena: &'a Bump,
    prefix: &str,
    separator: char,
    previous: Option<&str>,
    last: &str,
) -> ArenaString<'a> {
    let mut out = ArenaString::new_in(arena);
    out.push_str(prefix);
    out.push('…');
    if let Some(previous) = previous {
        out.push(separator);
        out.push_str(previous);
    }
    out.push(separator);
    out.push_str(last);
    out
}

/// 把超长路径压到 `max_length` 个字符以内，结果借用竞技场。
///
/// 优先保留「前缀 + … + 父目录 + 末段」，逐级降级到只保留末段。
pub fn compact_path<'a>(arena: &'a Bump, path: &'a str, max_length: usize) -> &'a str {
    if path.chars().count() <= max_length {
        return path;
    }

    if max_length < 2 {
        let mut out = ArenaString::new_in(arena);
        out.extend(path.chars().take(max_length));
        return out.into_bump_str();
    }

    let separator = detect_path_separator(path);
    let prefix = detect_path_prefix(path);
    let rest = &path[prefix.len()..];
    let segments: Vec<&str> = rest.split(['\\', '/']).filter(|s| !s.is_empty()).collect();

    let last_segment = match segments.last() {
        Some(segment) => *segment,
        None => tail_chars(arena, path, max_length - 1),
    };
    let previous_segment = if segments.len() >= 2 {
        Some(segments[segments.len() - 2])
    } else {
        None
    };

    // `into_bump_str` 把竞技场字符串转成 `&'a str`，因此可以安全地放进数组返回。
    let candidates = [
        build_candidate(arena, prefix, separator, previous_segment, last_segment).into_bump_str(),
        build_candidate(arena, "", separator, previous_segment, last_segment).into_bump_str(),
        build_candidate(arena, "", separator, None, last_segment).into_bump_str(),
    ];
    for candidate in candidates {
        if candidate.chars().count() <= max_length {
            return candidate;
        }
    }

    let tail = tail_chars(arena, path, max_length - 1);
    let mut out = ArenaString::new_in(arena);
    out.push('…');
    out.push_str(tail);
    out.into_bump_str()
}

/// 取 `text` 末尾 `count` 个字符，写入竞技场。
fn tail_chars<'a>(arena: &'a Bump, text: &str, count: usize) -> &'a str {
    let total = text.chars().count();
    let skip = total.saturating_sub(count);
    let start = text
        .char_indices()
        .nth(skip)
        .map_or(text.len(), |(index, _)| index);
    let mut out = ArenaString::new_in(arena);
    out.push_str(&text[start..]);
    out.into_bump_str()
}

#[cfg(test)]
mod tests {
    use super::*;
    use phi_ext_common::arena::Scratch;

    fn compact(path: &str, max: usize) -> String {
        let scratch = Scratch::with_capacity(256);
        compact_path(scratch.arena(), path, max).to_string()
    }

    #[test]
    fn short_path_should_pass_through() {
        assert_eq!(compact("src/main.rs", 50), "src/main.rs");
    }

    #[test]
    fn long_posix_path_should_keep_last_two_segments() {
        let path = "/home/user/projects/very-long-directory-name/src/components/widget.tsx";
        let compacted = compact(path, 40);
        assert!(compacted.chars().count() <= 40, "got {compacted}");
        assert!(compacted.starts_with('/'));
        assert!(compacted.ends_with("widget.tsx"));
        assert!(compacted.contains('…'));
    }

    #[test]
    fn long_relative_path_should_keep_last_segment() {
        let path = "a/very/long/chain/of/directories/and/more/deeply/nested/file.rs";
        let compacted = compact(path, 20);
        assert!(compacted.chars().count() <= 20, "got {compacted}");
        assert!(compacted.ends_with("file.rs"));
    }

    #[test]
    fn windows_drive_path_should_keep_prefix() {
        let path = r"C:\Users\someone\projects\app\src\very\deep\file.rs";
        let compacted = compact(path, 30);
        assert!(compacted.chars().count() <= 30, "got {compacted}");
        assert!(compacted.starts_with(r"C:\"), "got {compacted}");
        assert!(compacted.ends_with("file.rs"));
    }

    #[test]
    fn max_length_below_two_should_slice() {
        assert_eq!(compact("abcdef", 1), "a");
        assert_eq!(compact("abcdef", 0), "");
    }

    #[test]
    fn unicode_path_should_not_panic_on_char_boundaries() {
        let path = "/很/长的/中文/目录/名称/文件.rs";
        let compacted = compact(path, 12);
        assert!(compacted.chars().count() <= 12, "got {compacted}");
    }

    #[test]
    fn unc_path_should_keep_host_and_share_prefix() {
        let path = r"\\server\share\deeply\nested\path\to\file.rs";
        let compacted = compact(path, 40);
        assert!(compacted.chars().count() <= 40, "got {compacted}");
        assert!(compacted.starts_with(r"\\server\share\"), "got {compacted}");
        assert!(compacted.ends_with("file.rs"), "got {compacted}");
    }

    #[test]
    fn extreme_limit_should_fall_back_to_tail() {
        // 末段本身比上限还长 → 走兜底分支。
        let path = "aaaaaaaaaa/bbbbbbbbbbbbbbbbbbbbbbbbbb";
        let compacted = compact(path, 10);
        assert!(compacted.chars().count() <= 10, "got {compacted}");
        assert!(compacted.starts_with('…'));
    }
}