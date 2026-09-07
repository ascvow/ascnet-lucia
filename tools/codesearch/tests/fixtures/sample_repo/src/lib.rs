//! 示例仓库：供 codesearch 集成测试使用的最小 Rust crate。

pub mod config {
    pub mod settings;
}
pub mod json {
    pub mod lexer;
}
pub mod network;

pub use network::retry::{RetryError, RetryPolicy};
