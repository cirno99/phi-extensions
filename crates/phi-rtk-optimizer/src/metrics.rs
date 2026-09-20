// metrics.rs — 输出压缩收益统计。
//
// 由 pi 版 pi-rtk-optimizer 的 src/output-metrics.ts 移植。
//
// 与 pi 版的差异：pi 记录 ISO8601 字符串时间戳，这里记录 Unix 毫秒，
// 避免为格式化引入日期库；统计摘要本身不使用时间戳。

use std::collections::BTreeMap;

use phi_ext_common::time::now_ms;

/// 一条压缩记录。
#[derive(Debug, Clone, PartialEq)]
pub struct OutputMetricRecord {
    /// 记录时间（Unix 毫秒）。
    pub timestamp_ms: u64,
    /// 工具名。
    pub tool: String,
    /// 生效的技术（逗号分隔，无则 `none`）。
    pub techniques: String,
    /// 压缩前字符数。
    pub original_chars: usize,
    /// 压缩后字符数。
    pub filtered_chars: usize,
    /// 节省百分比（保留两位小数）。
    pub savings_percent: f64,
}

/// 压缩收益统计器。
#[derive(Debug, Default)]
pub struct OutputMetrics {
    records: Vec<OutputMetricRecord>,
}

impl OutputMetrics {
    /// 记录一次压缩。
    pub fn track(
        &mut self,
        original: &str,
        filtered: &str,
        tool: &str,
        techniques: &[String],
    ) -> OutputMetricRecord {
        let original_chars = original.chars().count();
        let filtered_chars = filtered.chars().count();
        let savings_percent = if original_chars > 0 {
            let raw = (original_chars - filtered_chars) as f64 / original_chars as f64 * 100.0;
            (raw * 100.0).round() / 100.0
        } else {
            0.0
        };

        let record = OutputMetricRecord {
            timestamp_ms: now_ms(),
            tool: tool.to_string(),
            techniques: if techniques.is_empty() {
                "none".to_string()
            } else {
                techniques.join(",")
            },
            original_chars,
            filtered_chars,
            savings_percent,
        };
        self.records.push(record.clone());
        record
    }

    /// 清空统计。
    pub fn clear(&mut self) {
        self.records.clear();
    }

    /// 记录条数。
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// 生成人类可读的统计摘要。
    pub fn summary(&self) -> String {
        if self.records.is_empty() {
            return "RTK output compaction metrics: no data yet.".to_string();
        }

        let total_original: usize = self.records.iter().map(|r| r.original_chars).sum();
        let total_filtered: usize = self.records.iter().map(|r| r.filtered_chars).sum();
        let total_saved = total_original.saturating_sub(total_filtered);
        let savings_percent = if total_original > 0 {
            total_saved as f64 / total_original as f64 * 100.0
        } else {
            0.0
        };

        let mut by_tool: BTreeMap<&str, (usize, usize, usize)> = BTreeMap::new();
        for record in &self.records {
            let entry = by_tool.entry(record.tool.as_str()).or_insert((0, 0, 0));
            entry.0 += 1;
            entry.1 += record.original_chars;
            entry.2 += record.filtered_chars;
        }

        let mut result = String::from("RTK output compaction metrics\n");
        result.push_str(&format!(
            "calls={}, saved={} chars ({:.1}%)\n",
            self.records.len(),
            format_thousands(total_saved),
            savings_percent
        ));
        for (tool, (count, original, filtered)) in by_tool {
            let saved = original.saturating_sub(filtered);
            let percent = if original > 0 {
                saved as f64 / original as f64 * 100.0
            } else {
                0.0
            };
            result.push_str(&format!(
                "- {tool}: {count} calls, saved {} chars ({percent:.1}%)\n",
                format_thousands(saved)
            ));
        }

        result.trim_end().to_string()
    }
}

/// 千分位分隔（对应 pi 的 `toLocaleString()`）。
pub fn format_thousands(value: usize) -> String {
    let digits = value.to_string();
    let mut out = String::new();
    for (index, character) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            out.push(',');
        }
        out.push(character);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn track_should_compute_savings() {
        let mut metrics = OutputMetrics::default();
        let record = metrics.track("a".repeat(100).as_str(), "a".repeat(25).as_str(), "bash", &["ansi".to_string()]);
        assert_eq!(record.original_chars, 100);
        assert_eq!(record.filtered_chars, 25);
        assert_eq!(record.savings_percent, 75.0);
        assert_eq!(record.techniques, "ansi");
        assert_eq!(metrics.len(), 1);
    }

    #[test]
    fn track_should_mark_missing_techniques() {
        let mut metrics = OutputMetrics::default();
        let record = metrics.track("abc", "abc", "read", &[]);
        assert_eq!(record.techniques, "none");
        assert_eq!(record.savings_percent, 0.0);
    }

    #[test]
    fn summary_should_report_empty_state() {
        let metrics = OutputMetrics::default();
        assert_eq!(metrics.summary(), "RTK output compaction metrics: no data yet.");
        assert!(metrics.is_empty());
    }

    #[test]
    fn summary_should_group_by_tool() {
        let mut metrics = OutputMetrics::default();
        metrics.track(&"x".repeat(1000), &"x".repeat(100), "bash", &["truncate".to_string()]);
        metrics.track(&"y".repeat(500), &"y".repeat(500), "read", &[]);
        let summary = metrics.summary();
        assert!(summary.contains("calls=2"), "got {summary}");
        assert!(summary.contains("saved=900 chars"), "got {summary}");
        assert!(summary.contains("- bash: 1 calls, saved 900 chars (90.0%)"), "got {summary}");
        assert!(summary.contains("- read: 1 calls, saved 0 chars (0.0%)"), "got {summary}");
    }

    #[test]
    fn clear_should_reset_records() {
        let mut metrics = OutputMetrics::default();
        metrics.track("a", "b", "bash", &[]);
        metrics.clear();
        assert!(metrics.is_empty());
    }

    #[test]
    fn format_thousands_should_group_digits() {
        assert_eq!(format_thousands(0), "0");
        assert_eq!(format_thousands(999), "999");
        assert_eq!(format_thousands(1_000), "1,000");
        assert_eq!(format_thousands(1_234_567), "1,234,567");
    }
}