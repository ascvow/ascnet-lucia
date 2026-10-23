//! 非 Unix 平台上的 socket 能力占位实现。
//!
//! Windows 没有 `AF_UNIX` 的通用替代，因此整条能力在该平台关闭。保留同名方法与相同的
//! 错误信息，可以让 ABI、manifest 校验和 Guest 代码只维护一份；插件在这里拿到的是明确的
//! 「平台不支持」，而不是加载期缺符号。

use crate::capability::{capability_denied, CapabilityState};
use anyhow::Result;
use serde_json::Value;

impl CapabilityState {
    /// 非 Unix 平台拒绝连接请求。
    pub(crate) async fn socket_connect(&mut self, _request_json: &str) -> Result<Value> {
        Err(capability_denied("当前平台不支持 Unix domain socket 能力"))
    }

    /// 非 Unix 平台拒绝写入请求。
    pub(crate) async fn socket_send(&mut self, _request_json: &str) -> Result<Value> {
        Err(capability_denied("当前平台不支持 Unix domain socket 能力"))
    }

    /// 非 Unix 平台拒绝读取请求。
    pub(crate) async fn socket_recv(&mut self, _request_json: &str) -> Result<Value> {
        Err(capability_denied("当前平台不支持 Unix domain socket 能力"))
    }

    /// 非 Unix 平台拒绝关闭请求。
    pub(crate) async fn socket_close(&mut self, _request_json: &str) -> Result<Value> {
        Err(capability_denied("当前平台不支持 Unix domain socket 能力"))
    }
}
