# 插件管理

插件管理只在 Lucia TUI 中提供。启动插件版 `lucia`，输入 `/plugins` 打开人工管理页。页面显示
受管理插件的固定版本、启用状态、Bundle SHA-256 和 Host 校验后的完整授权 profile。

常用按键：

- `g`：从 Registry 安装 `name[@semver]`；
- `h`：从 GitHub Release 安装预构建 Bundle；
- `i`：从本地 Bundle 目录安装；
- `t` / `u`：从 Registry 或本地 Bundle 更新高亮插件；
- `b`：输入插件 ID 和历史版本，从 Installed Version History 人工回滚；
- `e`：禁用，或在查看 capability 后连续两次确认启用；
- `o` / `w`：选择或清除 Capability Owner；禁用目标会先记录意图，再要求连续两次 `e` 确认启用；
- `x`：二次确认后移除；
- `r`：刷新锁文件。

本地安装可以选择已经构建过的插件 crate 目录，但受管理目录只保存 `plugin.toml` 和其中指定的
WASM。源码、Cargo `target`、Smoke Test 及其他开发文件不会被复制，也不会参与安装摘要。

新安装和普通替换默认保持禁用。Registry 安装不会隐式启用新增依赖；用户需逐个查看能力并确认。
直接选择 Capability Owner 只负责路由，不能隐式启用插件。用户对禁用目标完成两次 `e` 确认后，
Manager 才在同一事务中启用插件并写入 Owner selection；任一校验失败都不提交半状态。已启用插件
的普通更新与人工回滚都会生成新的精确 Bundle 身份，并固定保持禁用，禁止旧授权覆盖新内容。

唯一受限例外是从 Lucia 固定默认 Registry 发起、且 Registry 将包标记为 `official` 的平台升级。
此链路只有在旧插件原本启用、GitHub 来源主体未变化且完整授权 profile 未变化时才迁移启用状态；
manifest 能力、插件服务依赖、ABI 固有权限或来源主体任一变化都回到禁用态。第三方 Registry 包
不能使用该例外。

授权 profile 同时包含跨插件服务依赖。页面以“插件服务依赖”展示依赖插件 ID、SemVer 范围以及
是否可选；新增、删除或改变这些字段都会触发重新确认，不能沿用旧版本的启用状态。

Host 把基础插件 ABI 的固有权限纳入同一稳定授权 profile，启用审查页逐项渲染该 profile，而不是维护
另一份固定文案。即使 manifest 没有申请额外 Host capability，启用后的插件仍能注册模型可见工具和
developer prompt，读取并阻止或重写进入插件链的工具调用，取消当前 Agent 运行，读取模型可见工具
结果和脱敏生命周期事件，向应用事件流发布可更新主事件列表、Footer 或插件视图导航的扩展事件，
以及提供插件 UI、隔离状态和受依赖 ACL 约束的插件服务。文件、进程、模型完成、Agent Runtime
和执行宿主界面动作仍必须由 manifest 单独申请。

锁文件 schema v2 会为每个已启用插件保存完整授权 profile 的摘要。读取旧 schema、缺少摘要或摘要
与当前 Host profile 不一致时，Manager 的有效视图会把插件降级为禁用并清除其 Owner selection；
下一次写事务会以当前 schema 持久化该安全状态，不会把旧启用位当成授权证据。

第一次 `e` 会把确认绑定到当时展示的来源、版本和 Bundle SHA-256。第二次 `e` 提交时，Manager
会在修改锁文件的同一事务内重新比较该身份；若另一进程已替换 Bundle，本次确认失效，插件保持
禁用，页面刷新后必须重新审查。

人工更新、移除和回滚会保留受 SHA-256 约束的旧 Bundle，并在页面显示为
`Installed Version History`，只用于人工回滚。

Plugin Manager 在跨进程锁内串行校验和提交。单插件操作以原子锁文件替换作为状态提交点；Registry
多包安装则逐项原子提交，并在后续项失败时按逆序补偿本次新增项。补偿本身失败时会返回同时包含原始
错误和补偿错误的诊断，此时不得声称整个批次从未暴露中间状态。应用层不会用操作前快照执行补偿写入，
避免覆盖其他进程已经提交的禁用或撤权。当前 Session 保持启动时的插件环境，新 Session 使用变更后
的环境。

Lucia 不会自主修改、生成、组合、升级、启停或重新配置插件。插件源码开发、Component 构建、
签名、发布和 smoke test 仍由用户、开发者与 CI 人工完成。完整的安全、优先级和事务说明见
[插件管理指南](/guide/plugin-management)。
