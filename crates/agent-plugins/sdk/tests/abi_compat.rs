//! 插件 JSON ABI 的向后与向前兼容契约测试。

use agent_plugin::{decode_host_response, from_json_string, ProcessSpec};

/// 旧 Guest 必须忽略 Host 响应新增的版本和扩展字段。
#[test]
fn host_response_accepts_additive_fields() {
    let value: u64 = decode_host_response(
        r#"{"schema_version":1,"ok":true,"value":7,"future_field":"ignored"}"#,
    )
    .expect("带新增字段的响应应保持兼容");

    assert_eq!(value, 7);
}

/// 旧进程请求省略可选字段时必须继续使用默认值。
#[test]
fn process_spec_defaults_optional_fields() {
    let spec: ProcessSpec =
        from_json_string(r#"{"command":"bun"}"#).expect("旧版最小进程请求应保持兼容");

    assert_eq!(spec.command, "bun");
    assert!(spec.args.is_empty());
    assert!(spec.env.is_empty());
    assert_eq!(spec.cwd, None);
    assert!(!spec.inherit_stderr);
}

/// 当前请求类型必须忽略未来协议增加的可选字段。
#[test]
fn process_spec_accepts_additive_fields() {
    let spec: ProcessSpec =
        from_json_string(r#"{"command":"bun","future_policy":{"mode":"restricted"}}"#)
            .expect("新增字段不应破坏现有 Guest 类型");

    assert_eq!(spec.command, "bun");
}

/// socket 读取结果的三种状态必须能被旧 Guest 正确解码。
#[test]
fn socket_recv_states_are_decoded() {
    use agent_plugin::{decode_host_response, SocketRecv};

    let data: SocketRecv = decode_host_response(
        r#"{"schema_version":1,"ok":true,"value":{"state":"data","data_base64":"aGVsbG8="},"future":0}"#,
    )
    .expect("数据状态应可解析");
    assert_eq!(data, SocketRecv::Data(b"hello".to_vec()));

    let timeout: SocketRecv = decode_host_response(r#"{"ok":true,"value":{"state":"timeout"}}"#)
        .expect("超时状态应可解析");
    assert_eq!(timeout, SocketRecv::Timeout);

    let closed: SocketRecv = decode_host_response(r#"{"ok":true,"value":{"state":"closed"}}"#)
        .expect("关闭状态应可解析");
    assert_eq!(closed, SocketRecv::Closed);
}

/// 未知读取状态必须明确失败，不能被当成超时静默重试。
#[test]
fn unknown_socket_state_is_rejected() {
    use agent_plugin::{decode_host_response, SocketRecv};

    let error = decode_host_response::<SocketRecv>(r#"{"ok":true,"value":{"state":"draining"}}"#)
        .expect_err("未知状态应失败");
    assert!(
        format!("{error:#}").contains("未知 socket 读取状态"),
        "{error:#}"
    );
}

/// socket 字节跨 ABI 传输必须无损，含非 UTF-8 数据。
#[test]
fn socket_bytes_round_trip() {
    use agent_plugin::{decode_base64, encode_base64};

    let bytes = [0u8, 159, 146, 150, b'\n', 0xFF];
    let encoded = encode_base64(&bytes);
    assert_eq!(decode_base64(&encoded).expect("应可解码"), bytes);

    let error = decode_base64("not base64").expect_err("非法 base64 应失败");
    assert!(format!("{error:#}").contains("base64"), "{error:#}");
}
