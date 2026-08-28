# 插件管理

插件管理只在 Lucia TUI 中提供。启动插件版 `lucia`，输入 `/plugins` 打开人工管理页。页面显示
受管理插件的固定版本、启用状态、Bundle SHA-256，以及当前 Stable Revision 和
`PluginEnvironmentDigest`。

常用按键：

- `g`：从 Registry 安装 `name[@semver]`；
- `h`：从 GitHub Release 安装预构建 Bundle；
- `i`：从本地 Bundle 目录安装；
- `t` / `u`：从 Registry 或本地 Bundle 更新高亮插件；
- `b`：输入插件 ID 和历史版本，从 Installed Version History 人工回滚；
- `e`：启用或禁用；
- `o` / `w`：选择或清除 Capability Owner；
- `x`：二次确认后移除；
- `r`：刷新锁文件和冻结环境。

人工更新、移除和回滚会保留受 SHA-256 约束的旧 Bundle，并在页面显示为
`Installed Version History`。它只用于 Replay 和人工回滚，不是 Evolution Lineage，Evolver
不能选择这些版本。

插件写操作要求 TUI 已通过 `[genome] stable` 绑定可发布的 Stable Genome。每次真实变化创建
`RevisionOrigin::PluginManagement` 人工基线，不生成 EvolutionCertificate，不显示为
`EVOLVED`。当前 Session 保持启动时的精确插件快照，新 Session 使用新基线；旧 Candidate
不能跨插件环境 Promotion。提交失败时 Plugin Manager 恢复到操作前的锁文件和 Bundle 状态。

Lucia 不会自主修改、生成、组合、升级、启停或重新配置插件。插件源码开发、Component 构建、
签名、发布和 smoke test 仍由用户、开发者与 CI 人工完成。完整的安全、优先级和事务说明见
[插件管理指南](/guide/plugin-management)。
