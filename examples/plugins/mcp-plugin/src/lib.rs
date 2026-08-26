//! 通用 stdio MCP 插件。

use agent_plugin::{
    export_plugin, ActivationContext, AgentPlugin, EventPresentation, EventPresentationTone,
    ExtensionEvent, PluginHostApi, ProcessSpec, PromptContribution, ToolCall, ToolResult, ToolSpec,
};
use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashMap},
    path::Path,
};

const MCP_PROTOCOL_VERSION: &str = "2024-11-05";
const PROCESS_READ_TIMEOUT_MS: u64 = 30_000;
const MAX_MESSAGES_PER_RESPONSE: usize = 256;

#[derive(Default)]
struct McpPlugin {
    servers: HashMap<String, McpServer>,
    tools: HashMap<String, McpToolRoute>,
    next_request_id: u64,
}

struct McpServer {
    process_handle: u64,
}

#[derive(Clone)]
struct McpToolRoute {
    server_id: String,
    remote_name: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct McpServerConfig {
    #[serde(default)]
    id: Option<String>,
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: HashMap<String, String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default = "default_inherit_stderr")]
    inherit_stderr: bool,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum McpConfigDocument {
    Collection {
        #[serde(rename = "mcpServers")]
        mcp_servers: BTreeMap<String, McpServerConfig>,
    },
    Single(McpServerConfig),
}

impl AgentPlugin for McpPlugin {
    fn activate(&mut self, host: &dyn PluginHostApi, context: ActivationContext) -> Result<()> {
        let config_dir = context
            .metadata
            .get("config_dir")
            .map(String::as_str)
            .unwrap_or("config");
        let config_entries = host
            .list_dir(config_dir)
            .with_context(|| format!("扫描 MCP 配置目录失败：{config_dir}"))?;
        let config_paths = config_entries
            .into_iter()
            .filter(|entry| !entry.is_dir && is_mcp_config_path(&entry.path))
            .map(|entry| entry.path)
            .collect::<Vec<_>>();
        if config_paths.is_empty() {
            // An empty MCP directory is a valid not-yet-configured state for default loading.
            // 默认加载时，空 MCP 目录表示尚未配置，不应阻止 Agent 启动。
            host.set_state("connected_servers", &json!([]))?;
            host.emit_event(&ExtensionEvent {
                name: "mcp.servers.empty".into(),
                data: json!({
                    "servers": [],
                    "text": "MCP 插件等待配置",
                }),
                presentation: Some(EventPresentation::divider(
                    "MCP 插件等待配置",
                    EventPresentationTone::Muted,
                )),
            })?;
            return Ok(());
        }

        for config_path in config_paths {
            let content = host
                .read_file(&config_path)
                .with_context(|| format!("读取 MCP 配置失败：{config_path}"))?;
            for (server_id, config) in parse_config_document(&config_path, &content)? {
                self.connect_server(host, server_id, config)?;
            }
        }

        let mut server_ids = self.servers.keys().cloned().collect::<Vec<_>>();
        server_ids.sort();
        host.upsert_prompt(&PromptContribution {
            id: "connected-mcp-servers".into(),
            content: format!(
                "已连接以下 MCP 服务：{}。需要使用外部能力时应优先选择对应的 mcp__ 工具。",
                server_ids.join("、")
            ),
            priority: 100,
        })?;
        host.set_state("connected_servers", &json!(server_ids))?;
        host.emit_event(&ExtensionEvent {
            name: "mcp.servers.connected".into(),
            data: json!({
                "servers": server_ids,
                "text": "MCP 服务已连接",
            }),
            presentation: Some(EventPresentation::divider(
                "MCP 服务已连接",
                EventPresentationTone::Info,
            )),
        })?;
        Ok(())
    }

    fn list_tools(&self) -> Vec<ToolSpec> {
        Vec::new()
    }

    fn call_tool(&mut self, call: ToolCall) -> Result<ToolResult> {
        Err(anyhow!("工具 {} 需要通过宿主能力执行", call.name))
    }

    fn call_tool_with_host(
        &mut self,
        host: &dyn PluginHostApi,
        call: ToolCall,
    ) -> Result<ToolResult> {
        let route = self
            .tools
            .get(&call.name)
            .cloned()
            .ok_or_else(|| anyhow!("未知 MCP 本地工具 ID：{}", call.name))?;
        let process_handle = self
            .servers
            .get(&route.server_id)
            .ok_or_else(|| anyhow!("MCP 服务未连接：{}", route.server_id))?
            .process_handle;
        let result = self.rpc_request(
            host,
            process_handle,
            "tools/call",
            json!({
                "name": route.remote_name,
                "arguments": call.args,
            }),
        )?;
        let is_error = result
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        Ok(ToolResult {
            call_id: call.id,
            name: call.name,
            content: result,
            is_error,
            error_kind: None,
            details: None,
        })
    }
}

impl McpPlugin {
    fn connect_server(
        &mut self,
        host: &dyn PluginHostApi,
        server_id: String,
        config: McpServerConfig,
    ) -> Result<()> {
        if self.servers.contains_key(&server_id) {
            return Err(anyhow!("MCP 服务 ID 重复：{server_id}"));
        }
        let process_handle = host.spawn_process(&ProcessSpec {
            command: config.command,
            args: config.args,
            env: config.env,
            cwd: config.cwd,
            inherit_stderr: config.inherit_stderr,
        })?;

        let setup_result = (|| {
            let initialized = self.rpc_request(
                host,
                process_handle,
                "initialize",
                json!({
                    "protocolVersion": MCP_PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {
                        "name": "lucia",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                }),
            )?;
            if initialized.get("protocolVersion").is_none() {
                return Err(anyhow!("MCP initialize 响应缺少 protocolVersion"));
            }
            self.send_notification(host, process_handle, "notifications/initialized", json!({}))?;
            let tools = self.list_remote_tools(host, process_handle)?;
            for remote_tool in tools {
                self.register_remote_tool(host, &server_id, remote_tool)?;
            }
            Ok(())
        })();

        if let Err(error) = setup_result {
            let _ = host.kill_process(process_handle);
            return Err(error).with_context(|| format!("初始化 MCP 服务失败：{server_id}"));
        }
        self.servers.insert(server_id, McpServer { process_handle });
        Ok(())
    }

    fn list_remote_tools(
        &mut self,
        host: &dyn PluginHostApi,
        process_handle: u64,
    ) -> Result<Vec<Value>> {
        let mut tools = Vec::new();
        let mut cursor = None::<String>;
        loop {
            let params = cursor
                .as_ref()
                .map(|cursor| json!({"cursor": cursor}))
                .unwrap_or_else(|| json!({}));
            let result = self.rpc_request(host, process_handle, "tools/list", params)?;
            let page = result
                .get("tools")
                .and_then(Value::as_array)
                .ok_or_else(|| anyhow!("MCP tools/list 响应缺少 tools 数组"))?;
            tools.extend(page.iter().cloned());
            cursor = result
                .get("nextCursor")
                .and_then(Value::as_str)
                .map(str::to_string);
            if cursor.is_none() {
                return Ok(tools);
            }
        }
    }

    fn register_remote_tool(
        &mut self,
        host: &dyn PluginHostApi,
        server_id: &str,
        remote_tool: Value,
    ) -> Result<()> {
        let remote_name = remote_tool
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("MCP 工具缺少 name"))?;
        let local_name = format!("{server_id}/{remote_name}");
        if self.tools.contains_key(&local_name) {
            return Err(anyhow!("MCP 本地工具 ID 重复：{local_name}"));
        }
        let public_name = public_tool_name(server_id, remote_name);
        let description = remote_tool
            .get("description")
            .and_then(Value::as_str)
            .filter(|description| !description.trim().is_empty())
            .unwrap_or("调用 MCP 服务提供的工具");
        let input_schema = remote_tool
            .get("inputSchema")
            .cloned()
            .unwrap_or_else(ToolSpec::empty_object_schema);
        host.upsert_tool(
            &local_name,
            &ToolSpec::new(public_name, description, input_schema),
        )?;
        self.tools.insert(
            local_name,
            McpToolRoute {
                server_id: server_id.to_string(),
                remote_name: remote_name.to_string(),
            },
        );
        Ok(())
    }

    fn rpc_request(
        &mut self,
        host: &dyn PluginHostApi,
        process_handle: u64,
        method: &str,
        params: Value,
    ) -> Result<Value> {
        let id = self.next_request_id;
        self.next_request_id = self.next_request_id.saturating_add(1);
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        host.write_process(process_handle, &format!("{request}\n"))?;

        for _ in 0..MAX_MESSAGES_PER_RESPONSE {
            let line = host
                .read_process_line(process_handle, PROCESS_READ_TIMEOUT_MS)?
                .ok_or_else(|| anyhow!("MCP 服务在响应 {method} 前关闭了 stdout"))?;
            let Ok(message) = serde_json::from_str::<Value>(line.trim()) else {
                continue;
            };
            if message.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = message.get("error") {
                return Err(anyhow!("MCP {method} 返回错误：{error}"));
            }
            return message
                .get("result")
                .cloned()
                .ok_or_else(|| anyhow!("MCP {method} 响应缺少 result"));
        }
        Err(anyhow!("MCP {method} 响应前收到过多无关消息"))
    }

    fn send_notification(
        &self,
        host: &dyn PluginHostApi,
        process_handle: u64,
        method: &str,
        params: Value,
    ) -> Result<()> {
        let notification = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        host.write_process(process_handle, &format!("{notification}\n"))
    }
}

fn parse_config_document(path: &str, content: &str) -> Result<Vec<(String, McpServerConfig)>> {
    let document: McpConfigDocument =
        serde_json::from_str(content).with_context(|| format!("解析 MCP 配置失败：{path}"))?;
    match document {
        McpConfigDocument::Collection { mcp_servers } => Ok(mcp_servers.into_iter().collect()),
        McpConfigDocument::Single(mut config) => {
            let id = config.id.take().unwrap_or_else(|| {
                Path::new(path)
                    .file_stem()
                    .and_then(|name| name.to_str())
                    .unwrap_or("mcp")
                    .to_string()
            });
            Ok(vec![(id, config)])
        }
    }
}

fn public_tool_name(server_id: &str, remote_name: &str) -> String {
    let raw = format!("mcp__{server_id}__{remote_name}");
    let mut sanitized = raw
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' || character == '-' {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    if sanitized.len() <= 64 {
        return sanitized;
    }
    let hash = fnv1a64(raw.as_bytes());
    sanitized.truncate(47);
    format!("{sanitized}_{hash:016x}")
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
    })
}

fn default_inherit_stderr() -> bool {
    true
}

/// 判断目录项是否为应实际启动的 MCP 配置，并排除随插件分发的示例文件。
fn is_mcp_config_path(path: &str) -> bool {
    path.ends_with(".json") && !path.ends_with(".example.json")
}

export_plugin!(McpPlugin);

#[cfg(test)]
mod tests {
    use super::*;

    /// 验证用户给出的单 server 配置可以直接解析。
    #[test]
    fn parses_single_server_config() {
        let config = r#"{
            "command": "bunx",
            "args": ["@mastergo/magic-mcp", "--url=https://mastergo.com"],
            "env": {"NPM_CONFIG_REGISTRY": "https://registry.npmjs.org/"}
        }"#;
        let servers =
            parse_config_document("config/mastergo.json", config).expect("单 server 配置应可解析");
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].0, "mastergo");
    }

    /// 验证超长或包含特殊字符的 MCP 工具名仍满足模型工具名限制。
    #[test]
    fn public_tool_names_are_provider_portable() {
        let name = public_tool_name(
            "包含空格的服务",
            "读取/一个/名称/非常/非常/非常/非常/非常/非常/长的节点",
        );
        assert!(name.len() <= 64);
        assert!(name
            .chars()
            .all(|character| character.is_ascii_alphanumeric()
                || character == '_'
                || character == '-'));
    }

    /// 验证官方插件分发的示例配置不会被当成真实 Server 启动。
    #[test]
    fn example_configs_are_not_loaded() {
        assert!(is_mcp_config_path("config/mastergo.json"));
        assert!(!is_mcp_config_path("config/mastergo.example.json"));
        assert!(!is_mcp_config_path("config/readme.md"));
    }
}
