# 插件管理

Lucia 通过 TUI 暴露完整人工插件管理。启动插件版 `lucia` 后输入 `/plugins`，可以查看固定版本、
Bundle SHA-256 和启用状态。若旧 ABI 或无效受管理插件阻止 TUI 启动，可先使用
`lucia plugins list` 和 `lucia plugins disable <plugin-id>` 执行最小恢复。应用服务把用户操作委托给
`agent-plugin-manager`；Plugin Host 只消费验证后的运行时配置，不参与安装。

Lucia 不会自主修改、生成、组合、升级、启停或重新配置插件；全部插件变更都由用户显式发起。

已禁用插件和 Installed Version History 不参与运行时加载。它们仍校验路径、Manifest 身份和
Bundle SHA-256；与当前 Host ABI 不兼容时报告警告，并在重新启用或回滚时严格拒绝。完整性损坏
仍是诊断错误。macOS Finder 生成的 `.DS_Store` 不属于插件状态，目录诊断会忽略。

## TUI 操作

`/plugins` 页面提供以下人工操作：

| 按键 | 操作 |
| --- | --- |
| `g` | 输入 Registry `name[@semver]` 并安装最高兼容版本 |
| `h` | 输入 GitHub `owner/repository` 或 URL，从 Release 安装预构建 Bundle |
| `i` | 输入本地 Bundle 目录并安装 |
| `t` | 更新高亮的 Registry 插件 |
| `u` | 使用本地 Bundle 替换高亮插件 |
| `b` | 输入插件 ID 和历史版本，从 Installed Version History 人工回滚 |
| `e` | 禁用，或在完整授权审查后连续两次确认启用高亮插件 |
| `o` / `w` | 选择或清除 Capability Owner；禁用目标仍需完成启用确认 |
| `x` | 两次确认后移除高亮插件 |
| `r` | 重新读取插件锁 |

Registry、GitHub 和本地安装都只接受已经构建的 Bundle。受管理目录只持久化 `plugin.toml`
和 Manifest 指向的 WASM；即使用户选择插件开发 crate，源码、Cargo `target`、Smoke Test 与
其他未声明文件也不会被复制或计入安装摘要。安装器不会克隆源码、构建插件、修改 Manifest 或
扩大权限；它会检查路径、大小、ZIP、SHA-256、Manifest、WASM、SemVer 依赖和 Capability
冲突。私有 GitHub 仓库可由运行 Lucia 的环境提供 `GITHUB_TOKEN`，TUI 不展示或持久化 token。

人工更新或移除时，原 Bundle 及其安装摘要进入 `Installed Version History`，不会删除。
人工回滚只切换锁文件绑定并重新校验 Bundle、依赖和 Capability Owner；被替换版本继续保留。
该历史用于人工恢复。

## 写操作事务

每次单插件写操作都由 Plugin Manager 在跨进程锁内完成：

1. 在不可变 staging 副本上校验 Bundle、Manifest、摘要、依赖和 Capability Owner；
2. 重新读取锁状态，并复核用户确认所绑定的来源、版本和 Bundle SHA-256；
3. 提交 Bundle 后，以原子替换 `plugins.lock.toml` 作为状态提交点；
4. 读取提交后的插件列表和运行时配置，判断插件环境是否真的发生变化。

Registry 多包安装不是一个文件系统原子事务：每个新增包逐项原子提交，后续项失败时 Manager 会在
同一跨进程锁内按逆序补偿本批次新增项，且不会触碰安装前已有插件。若补偿也失败，错误会同时披露
原始失败与补偿失败，调用方必须按锁文件和 Bundle 的实际状态处理，不能假定整个批次从未提交。

人工变更后，当前 Session 和已经实例化的 Host 继续使用启动时的插件环境；新 Session 使用新
环境。Lucia 不在活动 Run 中热替换插件。

## 安装位置与运行时优先级

受管理插件安装到 `$LUCIA_HOME/plugins/<id>/<version>/`，锁文件是
`$LUCIA_HOME/plugins.lock.toml`。普通启动按以下优先级合并插件：

1. `--plugin-manifest` 和配置文件 `[[plugins]]`；
2. `$LUCIA_HOME/plugins/<bundle>/plugin.toml` 的人工开发 Bundle；
3. Plugin Manager 锁定且启用的 Bundle；
4. `$LUCIA_HOME/official-plugins/<bundle>/plugin.toml`。

同 ID 的高优先级声明覆盖低优先级来源，配置中的 `disabled_plugins` 最后生效。最终组合进入
Host 前重新校验 Bundle、Manifest、依赖、权限和 Capability Owner。

## 开发与诊断

官方和第三方插件继续使用独立 crate、独立插件版本和独立 `api_version`。开发者更新
`plugin.toml` 后应运行对应 workspace 测试、Component 构建和真实 WASM Host smoke test，再
把预构建 Bundle 交给 TUI 人工安装或更新。`bun run package:plugin:official` 继续生成 Registry、
ZIP 和 `.sha256`。

`lucia doctor` 保留只读诊断；`lucia doctor --network` 额外检查 GitHub 连通性。插件服务版本、
Manifest `api_version`、插件 crate 版本和锁文件 Schema 是独立版本维度，不得相互替代。
