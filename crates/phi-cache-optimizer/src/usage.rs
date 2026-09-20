// usage.rs — 会话用量读取：从宿主持久化的 JSONL 会话文件累计 token 用量，
// 再用 `phi-ext-common::stats` 计算缓存命中率与 token 速率。
//
// phi 不向扩展推送用量事件，但宿主会把每条 assistant 消息的 `usage` 写进
// `<phi_home>/session/<encoded-cwd>/<时间戳>_<session-id>.jsonl`
// （见宿主 `internal/session/entry.go` 的 `SessionMessageEntry.Usage`，其注释
// 明确说 token 计数持久化就是为了「diagnostics and session lifecycle
// extensions」）。因此扩展可以直接读该文件，无需宿主新增钩子。

use std::path::PathBuf;

use serde::Deserialize;

use phi_ext_common::paths;
use phi_ext_common::stats::{self, UsageStats};

/// 一次会话的用量报告。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UsageReport {
    /// 用量汇总（口径见 `phi-ext-common::stats` 模块文档）。
    pub stats: UsageStats,
    /// 会话跨度（毫秒）：首尾带时间戳条目之差；无法解析时为 0。
    pub elapsed_ms: u64,
}

impl UsageReport {
    /// 缓存命中率（百分比，`0.0..=100.0`）。
    pub fn cache_hit_rate(&self) -> f64 {
        self.stats.cache_hit_rate()
    }

    /// 会话平均 token 速率（输出 token / 秒）。
    pub fn tps(&self) -> f64 {
        stats::tokens_per_second(self.stats.output, self.elapsed_ms)
    }

    /// 以 `◆ 123tps` 形式格式化 token 速率。
    pub fn tps_text(&self) -> String {
        stats::format_rate(self.tps(), "◆")
    }

    /// 是否累计到任何用量。
    pub fn has_usage(&self) -> bool {
        self.stats.total_input() > 0 || self.stats.output > 0
    }
}

/// 会话目录名：与宿主 `internal/project.ProjectDirName` 一致
/// （`--<去掉前导分隔符、把 / \ : 换成 ->--`）。
pub fn project_dir_name(cwd: &str) -> String {
    let mut cleaned = cwd.trim();
    // 近似 filepath.Clean：去掉末尾分隔符（根路径除外）。
    while cleaned.len() > 1 && (cleaned.ends_with('/') || cleaned.ends_with('\\')) {
        cleaned = &cleaned[..cleaned.len() - 1];
    }
    if cleaned.is_empty() || cleaned == "." {
        return "--unknown--".to_string();
    }
    let trimmed = cleaned
        .strip_prefix(['/', '\\'])
        .unwrap_or(cleaned);
    let mut out = String::with_capacity(trimmed.len() + 4);
    for ch in trimmed.chars() {
        match ch {
            '/' | '\\' | ':' => out.push('-'),
            other => out.push(other),
        }
    }
    if out.is_empty() {
        out.push_str("unknown");
    }
    format!("--{out}--")
}

/// 当前会话文件路径：`<phi_home>/session/<encoded-cwd>/` 下文件名以
/// `_<session-id>.jsonl`（或恰为 `<session-id>.jsonl`）结尾者。
pub fn session_file_path(cwd: &str, session_id: &str) -> Option<PathBuf> {
    let session_id = session_id.trim();
    if session_id.is_empty() {
        return None;
    }
    let dir = paths::phi_home()
        .join("session")
        .join(project_dir_name(cwd));
    let exact = format!("{session_id}.jsonl");
    let suffix = format!("_{session_id}.jsonl");
    let entries = std::fs::read_dir(&dir).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name == exact || name.ends_with(&suffix) {
            return Some(entry.path());
        }
    }
    None
}

/// 读取当前会话的用量报告；文件缺失或不可读时返回 `None`。
pub fn load(cwd: &str, session_id: &str) -> Option<UsageReport> {
    let path = session_file_path(cwd, session_id)?;
    let contents = std::fs::read_to_string(&path).ok()?;
    Some(parse_session(&contents))
}

/// 汇总 JSONL 会话文本里的 usage（无法识别的行直接跳过）。
pub fn parse_session(contents: &str) -> UsageReport {
    let mut stats = UsageStats::default();
    let mut first_ms: Option<u64> = None;
    let mut last_ms: Option<u64> = None;

    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<EntryLine>(line) else {
            continue;
        };
        if let Some(ts) = entry.timestamp.as_deref().and_then(parse_rfc3339_ms) {
            if first_ms.is_none() {
                first_ms = Some(ts);
            }
            last_ms = Some(ts);
        }
        if let Some(usage) = entry.usage {
            stats.input = stats.input.saturating_add(usage.prompt_tokens);
            stats.output = stats.output.saturating_add(usage.completion_tokens);
            if let Some(details) = usage.prompt_tokens_details {
                stats.cache_read = stats.cache_read.saturating_add(details.cached_tokens);
                stats.cache_write = stats.cache_write.saturating_add(details.cache_write_tokens);
            }
        }
    }

    let elapsed_ms = match (first_ms, last_ms) {
        (Some(start), Some(end)) if end >= start => end - start,
        _ => 0,
    };
    UsageReport { stats, elapsed_ms }
}

/// 渲染 `/cache-optimizer stats` 的展示文本。
pub fn render(report: Option<&UsageReport>) -> String {
    match report {
        Some(report) if report.has_usage() => {
            let usage = &report.stats;
            format!(
                "\u{1F4CA} 缓存用量统计（本会话）\n\
                 缓存命中率：{}\n\
                 token 速率：{}\n\
                 输入 {} · 输出 {} · 缓存读 {} · 缓存写 {}",
                stats::format_hit_rate(report.cache_hit_rate()),
                report.tps_text(),
                usage.input,
                usage.output,
                usage.cache_read,
                usage.cache_write,
            )
        }
        _ => "\u{1F4CA} 缓存用量统计\n\
              暂无会话用量数据（未找到当前会话文件，或尚无 provider 上报的 usage）。\n\
              口径：命中率 = cacheRead / (cacheRead + cacheWrite + input)，上限 100%；\
              速率 = 输出 token / 会话时长。"
            .to_string(),
    }
}

/// 供 footer 状态行（`ctx.set_status`）展示的紧凑文本。
///
/// 无数据时返回空串——`set_status("")` 会清除状态行。
pub fn footer_status(report: Option<&UsageReport>) -> String {
    match report {
        Some(report) if report.has_usage() => format!(
            "缓存命中 {} · {}",
            stats::format_hit_rate(report.cache_hit_rate()),
            report.tps_text(),
        ),
        _ => String::new(),
    }
}

/// 会话文件里我们关心的字段（其余忽略）。
#[derive(Debug, Deserialize)]
struct EntryLine {
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(default)]
    usage: Option<UsageLine>,
}

#[derive(Debug, Deserialize)]
struct UsageLine {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
    #[serde(default)]
    prompt_tokens_details: Option<DetailsLine>,
}

#[derive(Debug, Deserialize)]
struct DetailsLine {
    #[serde(default)]
    cached_tokens: u64,
    #[serde(default)]
    cache_write_tokens: u64,
}

/// 解析 RFC 3339 时间戳为 Unix 毫秒；解析失败返回 `None`。
///
/// 接受 `YYYY-MM-DDTHH:MM:SS[.fraction][Z|±HH:MM]`（宿主用 Go `time.Time`
/// 默认的 RFC3339Nano 序列化；小数秒只取毫秒精度）。
pub fn parse_rfc3339_ms(value: &str) -> Option<u64> {
    let bytes = value.as_bytes();
    if bytes.len() < 19 {
        return None;
    }
    if bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' {
        return None;
    }
    if bytes[13] != b':' || bytes[16] != b':' {
        return None;
    }
    let year: i64 = value.get(0..4)?.parse().ok()?;
    let month: i64 = value.get(5..7)?.parse().ok()?;
    let day: i64 = value.get(8..10)?.parse().ok()?;
    let hour: i64 = value.get(11..13)?.parse().ok()?;
    let minute: i64 = value.get(14..16)?.parse().ok()?;
    let second: i64 = value.get(17..19)?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    if hour > 23 || minute > 59 || second > 60 {
        return None;
    }

    let mut idx = 19;
    let mut millis: i64 = 0;
    if bytes.get(idx) == Some(&b'.') {
        idx += 1;
        let start = idx;
        while idx < bytes.len() && bytes[idx].is_ascii_digit() {
            idx += 1;
        }
        let frac = &value[start..idx];
        let mut ms = String::with_capacity(3);
        for i in 0..3 {
            ms.push(frac.as_bytes().get(i).copied().map(char::from).unwrap_or('0'));
        }
        millis = ms.parse().ok()?;
    }

    let offset_min: i64 = match bytes.get(idx) {
        Some(b'Z') | None => 0,
        Some(sign @ (b'+' | b'-')) => {
            if value.get(idx + 3..idx + 4) != Some(":") {
                return None;
            }
            let hh: i64 = value.get(idx + 1..idx + 3)?.parse().ok()?;
            let mm: i64 = value.get(idx + 4..idx + 6)?.parse().ok()?;
            let signed = hh * 60 + mm;
            if *sign == b'-' {
                -signed
            } else {
                signed
            }
        }
        _ => return None,
    };

    let days = days_from_civil(year, month, day);
    let secs = days * 86_400 + hour * 3_600 + minute * 60 + second - offset_min * 60;
    if secs < 0 {
        return None;
    }
    Some(secs as u64 * 1000 + millis as u64)
}

/// Howard Hinnant 的 `days_from_civil`：公历日期 → 距 1970-01-01 的天数。
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_dir_name_should_match_host_encoding() {
        assert_eq!(
            project_dir_name("/Users/foo/Phi/"),
            "--Users-foo-Phi--"
        );
        assert_eq!(project_dir_name("/home/me/proj"), "--home-me-proj--");
        assert_eq!(project_dir_name("."), "--unknown--");
    }

    #[test]
    fn parse_rfc3339_should_handle_utc_and_fraction_and_offset() {
        // 2021-01-01T00:00:00Z == 1609459200000
        assert_eq!(parse_rfc3339_ms("2021-01-01T00:00:00Z"), Some(1_609_459_200_000));
        // 小数秒只取毫秒精度
        assert_eq!(
            parse_rfc3339_ms("2021-01-01T00:00:00.123456789Z"),
            Some(1_609_459_200_123)
        );
        // +08:00 比 UTC 早 8 小时 → 对应 UTC 前一天 16:00
        assert_eq!(
            parse_rfc3339_ms("2021-01-01T08:00:00+08:00"),
            Some(1_609_459_200_000)
        );
        assert_eq!(parse_rfc3339_ms("not-a-time"), None);
    }

    #[test]
    fn parse_session_should_sum_usage_and_elapsed() {
        let jsonl = concat!(
            r#"{"type":"session","id":"abc","timestamp":"2021-01-01T00:00:00Z","cwd":"/x"}"#,
            "\n",
            r#"{"type":"EntryMessage","timestamp":"2021-01-01T00:00:10Z","message":{"role":"assistant"},"usage":{"prompt_tokens":100,"completion_tokens":50,"total_tokens":150,"prompt_tokens_details":{"cached_tokens":800,"cache_write_tokens":100}}}"#,
            "\n",
            r#"{"type":"EntryMessage","timestamp":"2021-01-01T00:00:20Z","message":{"role":"assistant"},"usage":{"prompt_tokens":100,"completion_tokens":50,"total_tokens":150,"prompt_tokens_details":{"cached_tokens":900,"cache_write_tokens":0}}}"#,
            "\n",
        );
        let report = parse_session(jsonl);
        assert_eq!(report.stats.input, 200);
        assert_eq!(report.stats.output, 100);
        assert_eq!(report.stats.cache_read, 1700);
        assert_eq!(report.stats.cache_write, 100);
        assert_eq!(report.elapsed_ms, 20_000);
        // 命中率 = 1700 / (1700 + 100 + 200) = 85%
        assert!((report.cache_hit_rate() - 85.0).abs() < 1e-9);
        // 速率 = 100 token / 20s = 5 tps
        assert!((report.tps() - 5.0).abs() < 1e-9);
        assert_eq!(report.tps_text(), "◆ 5tps");
    }

    #[test]
    fn parse_session_should_skip_malformed_lines() {
        let jsonl = "not json\n\n{\"type\":\"EntryMessage\",\"usage\":{\"prompt_tokens\":10}}";
        let report = parse_session(jsonl);
        assert_eq!(report.stats.input, 10);
        assert_eq!(report.elapsed_ms, 0);
        assert_eq!(report.tps(), 0.0);
    }

    #[test]
    fn render_should_report_when_no_usage() {
        assert!(render(None).contains("暂无会话用量数据"));
        let empty = parse_session("");
        assert!(render(Some(&empty)).contains("暂无会话用量数据"));
    }

    #[test]
    fn render_should_show_hit_rate_and_tps() {
        let report = parse_session(concat!(
            r#"{"timestamp":"2021-01-01T00:00:00Z"}"#,
            "\n",
            r#"{"timestamp":"2021-01-01T00:00:10Z","usage":{"prompt_tokens":100,"completion_tokens":50,"prompt_tokens_details":{"cached_tokens":800,"cache_write_tokens":100}}}"#,
            "\n",
        ));
        let text = render(Some(&report));
        assert!(text.contains("缓存命中率：80.00%c"), "got {text}");
        assert!(text.contains("token 速率："), "got {text}");
    }

    #[test]
    fn footer_status_should_be_empty_without_usage() {
        assert_eq!(footer_status(None), "");
        let empty = parse_session("");
        assert_eq!(footer_status(Some(&empty)), "");
    }

    #[test]
    fn footer_status_should_show_hit_rate_and_tps() {
        let report = parse_session(concat!(
            r#"{"timestamp":"2021-01-01T00:00:00Z"}"#,
            "\n",
            r#"{"timestamp":"2021-01-01T00:00:10Z","usage":{"prompt_tokens":100,"completion_tokens":50,"prompt_tokens_details":{"cached_tokens":800,"cache_write_tokens":100}}}"#,
            "\n",
        ));
        let text = footer_status(Some(&report));
        assert!(text.contains("缓存命中 80.00%c"), "got {text}");
        assert!(text.contains("◆ 5tps"), "got {text}");
    }
}