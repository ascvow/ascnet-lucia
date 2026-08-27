# 插件基础设施

本目录集中存放插件系统的原生基础 crate，减少 `crates/` 顶层的平铺项；Cargo 包名和公开 API 保持不变。

- `protocol`：Host、Guest 与应用共享的宿主无关协议类型。
- `sdk`：WASM Guest SDK、WIT 绑定和导出宏，对应 `agent-plugin`。
- `host`：WASM ABI、生命周期、权限和贡献路由，对应 `agent-plugin-host`。
- `manager`：安装、完整性锁、依赖求解、启停和诊断，对应 `agent-plugin-manager`。

这四个 crate 只在目录层级上归组，不合并职责。Host 不承担安装状态，Manager 不实现 ABI，SDK 不依赖 Host 实现。
