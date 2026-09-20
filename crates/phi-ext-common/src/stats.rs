//! 用量统计计算：缓存命中率与 token 速率。
//!
//! 这两项原本由 pi 版 statusline 扩展在渲染层计算；phi 宿主已自带状态栏，
//! 因此这里只保留纯计算与格式化，供任意扩展复用。
//!
//! 口径与 pi 版一致：
//! - 命中率 = `cacheRead / (cacheRead + cacheWrite + input)`，上限 100%。
//!   分母为 0 时返回 0，避免出现 `NaN` / `Infinity`。
//! - 速率 = `数量 / (毫秒 / 1000)`，即每秒数量；毫秒为 0 时返回 0。

/// 一次会话的用量汇总（口径见模块文档）。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct UsageStats {
    /// 最后一次请求的真实输入 token（不含缓存）。
    pub input: u64,
    /// 会话累计输出 token。
    pub output: u64,
    /// 缓存读取 token（命中部分）。
    pub cache_read: u64,
    /// 缓存写入 token。
    pub cache_write: u64,
    /// 会话累计成本（美元）。
    pub cost: f64,
}

impl UsageStats {
    /// 参与命中率计算的总输入 token。
    pub fn total_input(&self) -> u64 {
        self.cache_read
            .saturating_add(self.cache_write)
            .saturating_add(self.input)
    }

    /// 缓存命中率（百分比，`0.0..=100.0`）。
    pub fn cache_hit_rate(&self) -> f64 {
        cache_hit_rate(self.cache_read, self.cache_write, self.input)
    }

    /// 以 `12.34%c` 形式格式化命中率。
    pub fn format_cache_hit_rate(&self) -> String {
        format_hit_rate(self.cache_hit_rate())
    }
}

/// 缓存命中率（百分比，上限 100）。
///
/// 命中率 = `cache_read / (cache_read + cache_write + input)`。
/// 分母为 0 时返回 `0.0`。
pub fn cache_hit_rate(cache_read: u64, cache_write: u64, input: u64) -> f64 {
    let total = cache_read
        .saturating_add(cache_write)
        .saturating_add(input);
    if total == 0 {
        return 0.0;
    }
    (cache_read as f64 / total as f64 * 100.0).min(100.0)
}

/// 以 `12.34%c` 形式格式化命中率（与 pi 版 footer 一致）。
pub fn format_hit_rate(rate_pct: f64) -> String {
    format!("{rate_pct:.2}%c")
}

/// 每秒速率：`count / (elapsed_ms / 1000)`。
///
/// `elapsed_ms == 0` 时返回 `0.0`（pi 版的 `safeDivide` 语义）。
pub fn rate_per_second(count: u64, elapsed_ms: u64) -> f64 {
    if elapsed_ms == 0 {
        return 0.0;
    }
    count as f64 / (elapsed_ms as f64 / 1000.0)
}

/// token 速率（token/s）。
pub fn tokens_per_second(tokens: u64, elapsed_ms: u64) -> f64 {
    rate_per_second(tokens, elapsed_ms)
}

/// 以 `◆ 123.4tps` 形式格式化速率（tps = token/s）。
///
/// 与 pi 版一致：小于 5 保留一位小数，否则四舍五入为整数。
pub fn format_rate(rate: f64, icon: &str) -> String {
    let display = if rate < 5.0 {
        format!("{rate:.1}")
    } else {
        format!("{}", rate.round() as i64)
    };
    format!("{icon} {display}tps")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_hit_rate_should_use_pi_formula() {
        // 800 / (800 + 100 + 100) = 80%
        assert!((cache_hit_rate(800, 100, 100) - 80.0).abs() < 1e-9);
    }

    #[test]
    fn cache_hit_rate_should_be_zero_when_no_input() {
        assert_eq!(cache_hit_rate(0, 0, 0), 0.0);
    }

    #[test]
    fn cache_hit_rate_should_be_capped_at_100() {
        // 只有 cache_read 时命中率正好 100%，不会超过。
        assert_eq!(cache_hit_rate(500, 0, 0), 100.0);
    }

    #[test]
    fn cache_hit_rate_should_be_zero_without_any_cache_read() {
        assert_eq!(cache_hit_rate(0, 300, 700), 0.0);
    }

    #[test]
    fn format_hit_rate_should_keep_two_decimals() {
        // 避免二进制浮点的四舍五入歧义，用无争议的值断言。
        assert_eq!(format_hit_rate(12.3), "12.30%c");
        assert_eq!(format_hit_rate(100.0), "100.00%c");
    }

    #[test]
    fn rate_per_second_should_handle_zero_elapsed() {
        assert_eq!(rate_per_second(100, 0), 0.0);
        assert!((rate_per_second(100, 500) - 200.0).abs() < 1e-9);
    }

    #[test]
    fn tokens_per_second_should_match_rate_per_second() {
        assert_eq!(tokens_per_second(50, 1000), 50.0);
    }

    #[test]
    fn format_rate_should_switch_precision_at_five() {
        assert_eq!(format_rate(4.94, "◆"), "◆ 4.9tps");
        assert_eq!(format_rate(123.4, "◆"), "◆ 123tps");
        assert_eq!(format_rate(0.0, "≡"), "≡ 0.0tps");
    }

    #[test]
    fn usage_stats_should_expose_total_and_hit_rate() {
        let usage = UsageStats {
            input: 100,
            output: 250,
            cache_read: 800,
            cache_write: 100,
            cost: 0.5,
        };
        assert_eq!(usage.total_input(), 1000);
        assert!((usage.cache_hit_rate() - 80.0).abs() < 1e-9);
        assert_eq!(usage.format_cache_hit_rate(), "80.00%c");
    }

    #[test]
    fn usage_stats_default_should_be_empty() {
        let usage = UsageStats::default();
        assert_eq!(usage.total_input(), 0);
        assert_eq!(usage.cache_hit_rate(), 0.0);
    }
}