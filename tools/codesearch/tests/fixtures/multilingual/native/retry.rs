use std::time::Duration;

/// 每次失败使等待时间翻倍，但不得超过三十秒。
pub fn next_delay(attempt: u32) -> Duration {
    Duration::from_secs(2u64.saturating_pow(attempt).min(30))
}
