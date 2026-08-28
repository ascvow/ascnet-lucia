# ADR-0002：插件环境不属于 Evolution Mutation Surface

## 状态

已接受，2026-08-27。

## 决策

Lucia 不会自主修改、生成、组合、升级或重新配置插件。插件系统、WASM Host、SDK、Manager、权限强制、人工安装更新、签名验证、Smoke Test、日志和 Incident 保持不变，但 Evolution Engine 只能读取固定插件环境，不能调用 Plugin Manager 写接口或访问插件源码。

每个 Evolution Cycle 绑定一个 PluginEnvironmentSnapshot。快照覆盖插件集合、版本、Bundle Digest、Manifest Digest、配置 Digest、Capability Profile Digest、加载顺序、Hook 顺序和 Capability Owner。Parent 与 Candidate 摘要必须相同；差异属于冻结依赖变化，不得进入 Evaluation、Commit Gate 或 Promotion。

历史 PluginMutationProposal、Plugin Candidate、评测与发布记录保持原字节和 Hash 可读。对应协议类型仅承担归档兼容，执行模块已移除；旧 MutationSurface::Plugin 不能进入新 Policy、不能作为新 Cycle 的父变异，也不能重新 Promotion。历史 Stable 引用的插件从本决策起视为固定 Bundle，只有人工 Plugin Management 可以替换并建立新基线。

## 制品边界

Prompt、Skill 内容和第一方稳定 Schema 的 Context、Planning、Routing 等 Policy 仍可进化。承载这些制品的插件 Runtime、插件提供的正式 Tool Contract 和不透明 Plugin Config 保持固定。人工插件变化的修订来源为 RevisionOrigin::PluginManagement，不生成 Evolution Certificate。

## 安全后果

Mutation Profile 没有文件系统、进程、网络或 Secret 能力。Evaluation 只使用只读 Fixture 和固定 Bundle 摘要。插件自身实现失败路由到 PluginMaintenance，权限或沙箱异常仍路由到 SecurityIncident；Agent 对固定工具的选择或参数错误可以进入 Agent 侧行为策略进化。

Plugin Host 从真实 WASM 执行边界识别 Trap、Fuel、内存限制、Capability Denied 与契约违规，并在 ToolResult 的 UI 细节中写入 Host 专用标记。Guest 返回前会被删除同名标记，Guest 自报的权限或边界错误也会降为普通执行失败。Evolution Outbox 只接受 Agent 行为 `EvolutionCandidate`；PluginMaintenance、SecurityIncident 等记录进入独立 Intervention Queue，Commit Gate 不得执行这些人工请求。
