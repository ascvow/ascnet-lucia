# 创建 WASM 插件

## Cargo crate

插件是独立 `cdylib` crate。仓库内插件应加入根 Cargo workspace，但不要加入 `default-members`；仓库外的第三方插件可以使用自己的 workspace：

```toml
[package]
name = "hello-plugin"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]

[dependencies]
anyhow = "1"
agent-plugin = { path = "../../../crates/agent-plugin" }
serde_json = "1"
wit-bindgen = "0.59"
```

## 最小实现

```rust
use anyhow::Result;
use agent_plugin::{
    export_plugin, AgentPlugin, ToolCall, ToolResult, ToolSpec,
};
use serde_json::json;

#[derive(Default)]
struct HelloPlugin;

impl AgentPlugin for HelloPlugin {
    fn list_tools(&self) -> Vec<ToolSpec> {
        vec![ToolSpec::new(
            "hello",
            "返回问候语",
            json!({
                "type": "object",
                "properties": { "name": { "type": "string" } },
                "required": ["name"]
            }),
        )]
    }

    fn call_tool(&mut self, call: ToolCall) -> Result<ToolResult> {
        let name = call.args["name"].as_str().unwrap_or("Lucia");
        Ok(ToolResult::success(
            call.id,
            call.name,
            json!({"message": format!("你好，{name}")}),
        ))
    }
}

export_plugin!(HelloPlugin);
```

## Manifest

```toml
[plugin]
id = "hello"
name = "Hello Plugin"
version = "0.1.0"
api_version = "0.6.0"
wasm = "target/wasm32-wasip2/release/hello_plugin.wasm"

[capabilities]
```

## 构建

```bash
cargo build --offline \
  --manifest-path examples/plugins/hello-plugin/Cargo.toml \
  --lib \
  --release \
  --target wasm32-wasip2
```

然后通过 `--plugin-manifest` 或 `load_wasm_plugins()` 加载。下一步阅读[生命周期](/plugin/lifecycle)和 [Host API](/plugin/host-api)。
