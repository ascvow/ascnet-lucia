//! 对外接口层：CLI 与 MCP。
//!
//! 两者都只是 [`crate::engine::Engine`] 的薄封装，检索与索引规则不在这里重复实现。

pub mod cli;
pub mod mcp;
