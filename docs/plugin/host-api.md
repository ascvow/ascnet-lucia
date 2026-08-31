# Guest Host API

`PluginHostApi` 是 WASM 插件访问宿主能力的唯一入口。Guest 视角是同步方法；Host 可以在背后执行异步 I/O，而不会把 future 暴露给 component。

## Agent 贡献

| 方法 | 返回 | 说明 |
| --- | --- | --- |
| `upsert_tool(local, spec)` | 公开工具名 | 注册或替换动态工具 |
| `remove_tool(public)` | `()` | 幂等删除公开工具 |
| `upsert_prompt(prompt)` | 提示 ID | 注册 developer 提示 |
| `remove_prompt(id)` | `()` | 幂等删除提示 |
| `emit_event(event)` | `()` | 发布结构化扩展事件 |

```rust
host.upsert_tool(
    "remote/get_item",
    &ToolSpec::new("remote_get_item", "读取项目", schema)
        .with_permission_scope(ToolPermissionScope::Network),
)?;

host.emit_event(&ExtensionEvent {
    name: "remote.ready".into(),
    data: json!({"count": 3}),
    presentation: Some(EventPresentation::divider(
        "远端服务已连接",
        EventPresentationTone::Success,
    )),
})?;
```

Host 会覆盖事件的 `source`，插件不能伪造其他 owner。静态 `list_tools` 与动态 `upsert_tool` 先使用 manifest 的 `[capabilities.tool_scopes]`；Guest 自报值不一致会拒绝加载，manifest 缺失映射时降级为 `Unknown`。Host 随后根据整个 component 已授予的进程、文件、模型、Agent 和宿主动作能力抬高最终 scope；多类资源合并为 `Unknown`，防止高权限插件用 `Control` 跳过逐调用审批。

## 实例状态

| 方法 | 说明 |
| --- | --- |
| `get_state(key)` | 读取 JSON 值，不存在返回 `None` |
| `set_state(key, value)` | 写入或替换 JSON 值 |
| `remove_state(key)` | 删除并返回旧值 |

状态按当前插件隔离。应用未注入持久目录时只保留到 component 卸载；注入后由 Host 按插件
ID、真实加载来源、Component 字节、版本和 capability profile 生成可信身份，事务性保存并只在
身份一致的插件重新加载前恢复，Guest 不接触也不能声明实际状态路径。

## 插件服务

| 方法 | 说明 |
| --- | --- |
| `upsert_service(spec)` | 注册或替换当前插件拥有的版本化服务 |
| `remove_service(name)` | 幂等删除当前插件服务 |
| `list_services(plugin_id)` | 查询当前 manifest 已声明依赖及自身拥有的服务目录 |
| `call_service(plugin_id, name, payload)` | 调用 manifest 已声明依赖的目标服务并取得 JSON 返回值 |

服务 owner、调用方 ID 和可调用依赖集合均由 Host 注入。Guest 不能发现或调用 manifest 未声明的插件服务；提供方仍需按业务资源 owner 校验请求。完整依赖模型和 command 复用示例见[依赖与服务](/plugin/dependencies-services)。

## 文件读取

```rust
let text = host.read_file("config/settings.json")?;
let entries = host.list_dir("config")?;
```

所有路径都经过 manifest `fs_read` 校验。`list_dir` 只列出一层，返回相对路径与 `is_dir`。

## 子进程 stdio

```rust
let handle = host.spawn_process(&ProcessSpec {
    command: "bun".into(),
    args: vec!["server.ts".into()],
    env: Default::default(),
    cwd: None,
    inherit_stderr: true,
})?;

host.write_process(handle, "request\n")?;
let line = host.read_process_line(handle, 30_000)?;
host.kill_process(handle)?;
```

该 API 不理解 JSON-RPC、MCP 或任何 framing。协议解析全部由插件负责。

## Agent Runtime

| 方法 | 说明 |
| --- | --- |
| `agent_identity()` | 返回 Host 分配给当前插件实例的 controller Agent ID |
| `spawn_agent(request)` | 使用已授权 profile 入队一个派生 Agent |
| `continue_agent(request)` | 基于可管理目标的成功终态私有会话入队后续运行 |
| `agent_status(target)` | 查询 controller 或其后代的状态、谱系和有效权限 |
| `agent_result(target)` | 非阻塞读取终态结果；未完成时返回 `None` |
| `cancel_agent(target)` | 幂等、级联取消指定后代 |

Guest 的 `AgentSpawnRequest` 只包含 profile 名称和任务输入；`AgentContinueRequest` 只包含目标 ID 和新增输入。模型、provider、工具权限与派生参数由应用注册的 profile 或目标运行决定，插件不能提交 owner、parent、sender、原始 Session、API key 或 provider options。

WASM Host API 不开放阻塞式 `wait`。插件应保存 `AgentHandle`，在后续工具调用或 UI 帧中查询状态和结果，避免同步 component 调用长期占用 store。teammate 邮箱由插件通过自身状态、持久 KV 或版本化 service 实现，不属于 Agent Runtime API。完整模型见 [Agent Runtime](/agent/agent-runtime)，可运行插件见 `examples/plugins/agent-runtime-plugin`。

## 受控模型完成

```rust
let response = host.complete_model(&ModelCompletionRequest {
    system: Some("生成结构化摘要".into()),
    messages,
    max_tokens: Some(20_000),
})?;
```

该能力要求 manifest 声明 `model_completion = true`，且应用通过 `PluginHostServices::with_model_completion` 注入模型网关、provider、model、流式模式和最大输出预算。Guest 不能提交模型路由、流式模式、工具、推理级别或 provider options；Host 固定这些设置并收窄输出预算。ExecutionPolicy 的累计调用次数和请求输入字节预算按单个插件激活隔离，同一激活内的并发 clone 共享原子账本，不会消耗其他插件实例的额度。调用失败或返回空文本时直接返回错误，不会静默退回本地裁剪。

## 错误信封

WIT import 返回 JSON 字符串：

```json
{"ok":true,"value":42}
```

```json
{"ok":false,"error":"插件 manifest 未声明 process_exec 能力"}
```

Rust SDK 自动把信封转为 `anyhow::Result<T>`。
