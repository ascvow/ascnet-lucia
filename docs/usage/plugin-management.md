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
- `a`：二次确认后把当前磁盘插件环境采用为新的人工基线；
- `e`：启用或禁用；
- `o` / `w`：选择或清除 Capability Owner；
- `x`：二次确认后移除；
- `r`：刷新锁文件和冻结环境。

本地安装可以选择已经构建过的插件 crate 目录，但受管理目录只保存 `plugin.toml` 和其中指定的
WASM。源码、Cargo `target`、Smoke Test 及其他开发文件不会被复制，也不会参与安装摘要。

人工更新、移除和回滚会保留受 SHA-256 约束的旧 Bundle，并在页面显示为
`Installed Version History`。它只用于 Replay 和人工回滚，不是 Evolution Lineage，Evolver
不能选择这些版本。

插件写操作要求 TUI 已绑定可发布的 Stable Genome；普通首次启动会自动创建默认人工 Stable，
无需额外填写 `[genome]`。每次真实变化创建 `RevisionOrigin::PluginManagement` 人工基线，
不生成 EvolutionCertificate，不显示为 `EVOLVED`。当前 Session 保持启动时的精确插件快照，
新 Session 使用新基线；旧 Candidate 不能跨插件环境 Promotion。提交失败时 Plugin Manager
恢复到操作前的锁文件和 Bundle 状态。

安装新 Lucia 二进制后，未绑定的新 Session 会创建 `RevisionOrigin::PlatformUpgrade` 基线；
它只更新 Kernel 运行身份并完整继承冻结插件环境，不会生成 EvolutionCertificate。已有 Session
和显式 Revision 不会被自动迁移。

Lucia 不会自主修改、生成、组合、升级、启停或重新配置插件。插件源码开发、Component 构建、
签名、发布和 smoke test 仍由用户、开发者与 CI 人工完成。完整的安全、优先级和事务说明见
[插件管理指南](/guide/plugin-management)。
