# Lucia 开发约束

本文件适用于整个仓库。任何 Codex 变更都必须保持模块职责、依赖方向和协议兼容性，不得以减少局部代码量为理由把复杂度转移到错误模块。

## 模块职责

- `agent-core` 只负责 Agent 通用消息、上下文、模型网关、ReAct、工具调用、事件和扩展契约。禁止依赖 Plugin Host、WASM ABI、manifest、插件 UI 或具体插件协议。
- `agent-tool` 只负责通用工具类型与原生注册表。禁止包含 Agent 循环或插件加载逻辑。
- `agent-session` 只负责版本化会话记录、CAS 和存储。禁止持有模型配置、Agent 调度或插件状态。
- `agent-runtime` 只负责 Agent 身份、派生、生命周期、权限收缩、私有会话续跑和资源限额。workflow、multi-agent、teammate 的编排与消息协议必须位于插件。
- `agent-plugin-host` 只负责 ABI、生命周期、能力鉴权、贡献注册、owner 路由和宿主无关 UI 协议。禁止解析 MCP、Skill、Command、上下文压缩或其他具体业务协议。
- `agent-plugin` 只负责 Guest SDK、共享协议类型、WIT 绑定和导出宏。禁止依赖 Host 实现或终端渲染。
- `ascnet-lucia-tui` 只负责应用组装、配置、输入和渲染。具体插件协议必须通过版本化服务或通用 Host API 交互，禁止在 TUI 复制插件业务规则。
- 官方与第三方插件必须保持独立插件 crate；端到端协议测试归插件所有，不得为了复用把协议实现移入 Core 或 Host。

## 依赖方向

- 允许 `application -> agent-core -> agent-tool`、`agent-session -> agent-core`、`agent-runtime -> agent-core`、`agent-plugin-host -> agent-core/agent-runtime`、`WASM guest -> agent-plugin -> agent-tool`。
- Core 不得依赖 Plugin Host；Guest SDK 不得依赖 Host；具体插件不得加入原生 workspace 以规避 component 导出目标冲突。
- 新增跨 crate 依赖前必须说明所有权理由。仅为调用一个辅助函数而反转依赖方向时，应把通用数据契约下沉到已有中立 crate，或保留边界两侧的小型适配代码。

## 复杂度控制

- 先确定行为所有者，再修改代码。通用机制进入 Core/Host；某类扩展如何工作的规则进入对应插件。
- 一个变更跨越多个 crate 时，必须明确唯一协议边界，并在边界两侧分别验证；禁止在多个模块复制判断分支形成隐式协议。
- 优先扩展现有类型和模块。只有新职责无法由现有所有者合理承载时才创建模块、trait、适配器或依赖。
- 禁止顺手重构、目录搬迁、批量重命名、公共 API 扩张和无关格式化。发现邻近问题时记录，不得混入当前提交。
- 公开类型和函数必须使用简体中文文档注释说明目的、非显然参数、返回值、错误和副作用；私有复杂逻辑也必须有简短的边界说明。
- 不以文件行数或抽象层数作为单独重构依据。只有职责混合、重复协议、错误依赖或无法独立测试时才拆分。

## 协议与安全

- WIT 使用 JSON 字符串是稳定 ABI 的设计选择。新增 JSON 字段必须为可选字段并提供 serde 默认值；删除、改名、改变含义或收紧枚举前必须升级对应协议版本。
- 修改 `wit/plugin.wit`、Guest 内嵌 WIT、Host 绑定或公共 JSON 类型时，必须同步所有副本，并新增旧请求与加法字段兼容测试。
- 插件服务版本、manifest `api_version`、Workspace crate 版本和 Plugin Manager lock schema 是四个独立版本维度，禁止联动修改或混用语义。
- `process_exec` 是完整原生进程信任，不是 WASM 沙箱内能力。禁止弱化权限说明、绕过 manifest 检查或扩大继承环境；任何能力扩张必须同时更新威胁模型、结构限制和回归测试。
- 服务端已掌握的身份、owner、stage、权限和资源上限必须由 Host 注入或收窄，禁止信任模型或 Guest 自行声明真实值。

## 验证与提交

- 修改单个 crate 时至少运行该 crate 的测试；修改 ABI、Host 或 Guest SDK 时同时运行 `agent-plugin` 与 `agent-plugin-host` 测试。
- 修改 TUI feature 或分发行为时同时验证默认插件版与 `--no-default-features` 纯 Core 版。
- 修改官方插件时运行对应独立 workspace 测试；跨插件公共协议还必须运行其 provider 与 consumer 测试。
- 文档中的默认值、feature 和版本必须来自代码或构建配置；行为变化时同步文档，禁止保留互相冲突的描述。
- 工作区已有改动视为用户所有。只暂存本任务产生的文件或补丁，不得覆盖、还原或提交无关改动。
- 每个独立功能完成并通过检查后，使用简体中文提交信息提交；禁止 push、merge、rebase 或修改历史。
