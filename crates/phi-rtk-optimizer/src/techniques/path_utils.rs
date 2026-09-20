// path_utils.rs — 路径压缩（超长路径折叠为 `…/父目录/末段`）。
//
// 由 pi 版 pi-rtk-optimizer 的 src/techniques/path-utils.ts 移植。

/// 路径分隔符：只有反斜杠、没有正斜杠时按 Windows 处理。
fn detect_path_separator(path: &str) -> char {
    if path.contains('\\') && !path.contains('/') {
        '\\'
    } else {
        '/'
    }
}

/// 提取路径前缀（盘符 / UNC 主机 / 根斜杠）。
fn detect_path_prefix(path: &str, separator: char) -> String {
    let bytes = path.as_bytes();
    if bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
    {
        return format!("{}{separator}", &path[..2]);
    }

    if path.starts_with("\\\\") || path.starts_with("//") {
        let parts: Vec<&str> = path
            .split(['\\', '/'])
            .filter(|part| !part.is_empty())
            .collect();
        if parts.len() >= 2 {
            return format!("{separator}{separator}{}{separator}{}{separator}", parts[0], parts[1]);
        }
        return format!("{separator}{separator}");
    }

    if path.starts_with('/') || path.starts_with('\\') {
        return separator.to_string();
    }

    String::new()
}

/// 用分隔符拼接前缀与段列表。
fn join_path_segments(prefix: &str, separator: char, segments: &[&str]) -> String {
    if segments.is_empty() {
        return prefix.to_string();
    }
    let joined = segments.join(&separator.to_string());
    if prefix.is_empty() {
        joined
    } else {
        format!("{prefix}{joined}")
    }
}

/// 把超长路径压到 `max_length` 个字符以内。
///
/// 优先保留「前缀 + … + 父目录 + 末段」，逐级降级到只保留末段。
pub fn compact_path(path: &str, max_length: usize) -> String {
    if path.chars().count() <= max_length {
        return path.to_string();
    }

    if max_length < 2 {
        return path.chars().take(max_length).collect();
    }

    let separator = detect_path_separator(path);
    let prefix = detect_path_prefix(path, separator);
    let rest = &path[prefix.len()..];
    let segments: Vec<&str> = rest.split(['\\', '/']).filter(|s| !s.is_empty()).collect();

    let last_segment = match segments.last() {
        Some(segment) => (*segment).to_string(),
        None => path
            .chars()
            .rev()
            .take(max_length - 1)
            .collect::<Vec<char>>()
            .into_iter()
            .rev()
            .collect(),
    };
    let previous_segment = if segments.len() >= 2 {
        Some(segments[segments.len() - 2])
    } else {
        None
    };

    let mut candidates = Vec::new();
    let with_previous: Vec<&str> = match previous_segment {
        Some(previous) => vec!["…", previous, &last_segment],
        None => vec!["…", &last_segment],
    };
    candidates.push(join_path_segments(&prefix, separator, &with_previous));
    candidates.push(join_path_segments("", separator, &with_previous));
    candidates.push(join_path_segments("", separator, &["…", &last_segment]));

    let tail: String = path
        .chars()
        .rev()
        .take(max_length - 1)
        .collect::<Vec<char>>()
        .into_iter()
        .rev()
        .collect();
    candidates.push(format!("…{tail}"));

    for candidate in &candidates {
        if candidate.chars().count() <= max_length {
            return candidate.clone();
        }
    }

    let last_tail: String = last_segment
        .chars()
        .rev()
        .take(max_length.saturating_sub(1))
        .collect::<Vec<char>>()
        .into_iter()
        .rev()
        .collect();
    format!("…{last_tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_path_should_pass_through() {
        assert_eq!(compact_path("src/main.rs", 50), "src/main.rs");
    }

    #[test]
    fn long_posix_path_should_keep_last_two_segments() {
        let path = "/home/user/projects/very-long-directory-name/src/components/widget.tsx";
        let compacted = compact_path(path, 40);
        assert!(compacted.chars().count() <= 40, "got {compacted}");
        assert!(compacted.starts_with('/'));
        assert!(compacted.ends_with("widget.tsx"));
        assert!(compacted.contains('…'));
    }

    #[test]
    fn long_relative_path_should_keep_last_segment() {
        let path = "a/very/long/chain/of/directories/and/more/deeply/nested/file.rs";
        let compacted = compact_path(path, 20);
        assert!(compacted.chars().count() <= 20, "got {compacted}");
        assert!(compacted.ends_with("file.rs"));
    }

    #[test]
    fn windows_drive_path_should_keep_prefix() {
        let path = r"C:\Users\someone\projects\app\src\very\deep\file.rs";
        let compacted = compact_path(path, 30);
        assert!(compacted.chars().count() <= 30, "got {compacted}");
        assert!(compacted.starts_with(r"C:\"));
        assert!(compacted.ends_with("file.rs"));
    }

    #[test]
    fn max_length_below_two_should_slice() {
        assert_eq!(compact_path("abcdef", 1), "a");
        assert_eq!(compact_path("abcdef", 0), "");
    }

    #[test]
    fn unicode_path_should_not_panic_on_char_boundaries() {
        let path = "/很/长的/中文/目录/名称/文件.rs";
        let compacted = compact_path(path, 12);
        assert!(compacted.chars().count() <= 12, "got {compacted}");
    }
}