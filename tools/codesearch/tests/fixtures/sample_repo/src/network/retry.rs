use std::time::Duration;

use crate::config::settings::Settings;

/// 单次请求允许的最大退避时长。
pub const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// 请求失败后的重试策略。
///
/// 采用指数退避，避免在服务端不稳定时形成重试风暴。
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// 最大尝试次数，包含首次请求。
    pub max_attempts: u32,
    /// 首次退避时长。
    pub base_delay: Duration,
}

impl RetryPolicy {
    /// 从配置构造重试策略。
    pub fn from_settings(settings: &Settings) -> Self {
        Self {
            max_attempts: settings.max_attempts,
            base_delay: Duration::from_millis(settings.base_delay_ms),
        }
    }

    /// 执行带退避的重试，直到成功或耗尽尝试次数。
    ///
    /// 每次失败后按 2 的幂次放大退避时长，并以 [`MAX_BACKOFF`] 为上限。
    pub fn execute<F, T>(&self, mut operation: F) -> Result<T, RetryError>
    where
        F: FnMut(u32) -> Result<T, String>,
    {
        let mut last_error = String::new();
        for attempt in 1..=self.max_attempts {
            match operation(attempt) {
                Ok(value) => return Ok(value),
                Err(error) => {
                    last_error = error;
                    if attempt == self.max_attempts {
                        break;
                    }
                    std::thread::sleep(self.backoff_for(attempt));
                }
            }
        }
        Err(RetryError {
            attempts: self.max_attempts,
            last_error,
        })
    }

    /// 计算第 `attempt` 次失败后的退避时长。
    pub fn backoff_for(&self, attempt: u32) -> Duration {
        let scaled = self.base_delay * 2u32.saturating_pow(attempt.saturating_sub(1));
        scaled.min(MAX_BACKOFF)
    }
}

/// 重试耗尽后的错误。
#[derive(Debug, Clone)]
pub struct RetryError {
    /// 实际尝试次数。
    pub attempts: u32,
    /// 最后一次失败原因。
    pub last_error: String,
}

impl std::fmt::Display for RetryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "connection reset by peer after {} attempts: {}",
            self.attempts, self.last_error
        )
    }
}
