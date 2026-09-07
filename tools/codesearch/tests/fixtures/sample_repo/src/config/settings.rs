/// 运行时配置。
#[derive(Debug, Clone)]
pub struct Settings {
    /// 最大尝试次数。
    pub max_attempts: u32,
    /// 首次退避毫秒数。
    pub base_delay_ms: u64,
    /// 服务地址。
    pub endpoint: String,
}

/// 默认最大尝试次数。
pub const DEFAULT_MAX_ATTEMPTS: u32 = 3;

/// 默认退避毫秒数。
pub const DEFAULT_BASE_DELAY_MS: u64 = 200;

impl Default for Settings {
    fn default() -> Self {
        Self {
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            base_delay_ms: DEFAULT_BASE_DELAY_MS,
            endpoint: "http://127.0.0.1:8080".to_string(),
        }
    }
}

impl Settings {
    /// 从环境变量覆盖默认配置。
    ///
    /// 非法数值会被忽略并保留默认值，不会让进程启动失败。
    pub fn from_env() -> Self {
        let mut settings = Self::default();
        if let Ok(value) = std::env::var("APP_MAX_ATTEMPTS") {
            if let Ok(parsed) = value.parse() {
                settings.max_attempts = parsed;
            }
        }
        if let Ok(endpoint) = std::env::var("APP_ENDPOINT") {
            settings.endpoint = endpoint;
        }
        settings
    }
}
