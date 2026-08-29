# ADR-0002：插件环境在 Session 内固定

## 状态

已接受，2026-08-27。

## 决策

Lucia 不会自主修改、生成、组合、升级或重新配置插件。插件系统、WASM Host、SDK、Manager、权限强制、人工安装更新、签名验证、Smoke Test、日志和 Incident 由人工流程维护。

每个 Session 在启动时绑定一份插件环境：插件集合、版本、Bundle Digest、Manifest Digest、配置 Digest、Capability Profile Digest、加载顺序、Hook 顺序和 Capability Owner。人工插件写操作只影响后续新 Session，活动 Session 与已实例化的 Host 保持不变。

## 制品边界

Prompt、Skill 内容和第一方稳定 Schema 的 Policy 由人工维护并随配置生效。承载这些制品的插件 Runtime、插件提供的正式 Tool Contract 和不透明 Plugin Config 在 Session 生命周期内保持固定。

## 安全后果

受限执行平面没有文件系统、进程、网络或 Secret 能力。插件自身实现失败路由到 PluginMaintenance，权限或沙箱异常路由到 SecurityIncident。

Plugin Host 从真实 WASM 执行边界识别 Trap、Fuel、内存限制、Capability Denied 与契约违规，并在 ToolResult 的 UI 细节中写入 Host 专用标记。Guest 返回前会被删除同名标记，Guest 自报的权限或边界错误也会降为普通执行失败。
