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
