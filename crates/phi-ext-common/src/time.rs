//! 时间工具。
//!
//! `now_ms` 原先在 3 个扩展里各写了一份（asymptotic-thinking 的 store、
//! sleep-continue 的 state、rtk-optimizer 的 runtime/metrics），这里收敛成
//! 单一实现。

use std::time::{SystemTime, UNIX_EPOCH};

/// 当前 Unix 毫秒时间戳；系统时钟早于 epoch 时返回 0。
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_ms_should_be_a_plausible_epoch_millis() {
        // 2020-09-13 之后（本项目写于其后），且不是 0。
        assert!(now_ms() > 1_600_000_000_000);
    }

    #[test]
    fn now_ms_should_be_monotonic_enough() {
        let first = now_ms();
        let second = now_ms();
        assert!(second >= first);
    }
}