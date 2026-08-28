# 插件管理

Lucia 只通过 TUI 暴露人工插件管理。启动插件版 `lucia` 后输入 `/plugins`，可以查看固定版本、
Bundle SHA-256、启用状态、当前 Stable Revision、代数和 `PluginEnvironmentDigest`。应用服务把
用户操作委托给 `agent-plugin-manager`；Plugin Host 只消费验证后的运行时配置，不参与安装。

插件不是 Evolution Mutation Surface。Lucia 不会自主修改、生成、组合、升级、启停或重新配置
插件。Prompt、Skill 内容和第一方 Policy Artifact 仍可进化，承载或读取它们的插件 Runtime
保持固定。

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
| `e` | 启用或禁用高亮插件 |
| `o` / `w` | 选择或清除 Capability Owner |
| `x` | 两次确认后移除高亮插件 |
| `r` | 重新读取插件锁和冻结环境 |

Registry、GitHub 和本地安装都只接受已经构建的 Bundle。安装器不会克隆源码、构建插件、修改
Manifest 或扩大权限；它会检查路径、大小、ZIP、SHA-256、Manifest、WASM、SemVer 依赖和
Capability 冲突。私有 GitHub 仓库可由运行 Lucia 的环境提供 `GITHUB_TOKEN`，TUI 不展示或
持久化 token。

人工更新或移除时，原 Bundle 及其安装摘要进入 `Installed Version History`，不会删除。
人工回滚只切换锁文件绑定并重新校验 Bundle、依赖和 Capability Owner；被替换版本继续保留。
该历史用于 Replay 与人工恢复，不称为 Evolution Lineage，也不向 Evolver 暴露版本选择能力。

## 人工基线事务

插件写操作只在 TUI 绑定了可发布的 Stable Genome 时启用。每次真实变化执行同一个事务：

1. 捕获当前 `plugins.lock.toml` 和受管理 Bundle 的确定性恢复快照；
2. 执行用户明确选择的 Plugin Manager 操作；
3. 从真实 Manifest、Bundle、加载顺序和 Capability Owner 构造冻结插件环境；
4. 创建 `RevisionOrigin::PluginManagement` Genome Revision；
5. 通过 expected-current CAS 发布新 Stable 基线。

人工基线不绑定 Release、EvaluationReport 或 EvolutionCertificate，也不会显示为 `EVOLVED`。
Stable 已被其他控制面移动时，提交失败并恢复操作前的 Plugin Manager 状态。Manager 写入后的
快照、Genome 登记或 Stable 提交失败也执行同一补偿恢复，避免锁文件与 Stable Genome 分叉。

人工变更后，当前 Session 和已经实例化的 Host 继续使用启动时的精确 Revision；新 Session
使用新 Stable。基于旧插件环境的 Candidate 因 expected-current 或插件环境摘要不一致而不能
Promotion。Lucia 不在活动 Run 中热替换插件。

## 安装位置与运行时优先级

受管理插件安装到 `$LUCIA_HOME/plugins/<id>/<version>/`，锁文件是
`$LUCIA_HOME/plugins.lock.toml`。普通启动按以下优先级合并插件：

1. `--plugin-manifest` 和配置文件 `[[plugins]]`；
2. `$LUCIA_HOME/plugins/<bundle>/plugin.toml` 的人工开发 Bundle；
3. Plugin Manager 锁定且启用的 Bundle；
4. `$LUCIA_HOME/official-plugins/<bundle>/plugin.toml`。

同 ID 的高优先级声明覆盖低优先级来源，配置中的 `disabled_plugins` 最后生效。最终组合进入
Host 前重新校验 Bundle、Manifest、依赖、权限、Capability Owner 和 Genome 冻结摘要。

## 开发与诊断

官方和第三方插件继续使用独立 crate、独立插件版本和独立 `api_version`。开发者更新
`plugin.toml` 后应运行对应 workspace 测试、Component 构建和真实 WASM Host smoke test，再
把预构建 Bundle 交给 TUI 人工安装或更新。`bun run package:plugin:official` 继续生成 Registry、
ZIP 和 `.sha256`；这些开发和 CI 流程不是插件进化。

`lucia doctor` 保留只读诊断；`lucia doctor --network` 额外检查 GitHub 连通性。插件服务版本、
Manifest `api_version`、插件 crate 版本和锁文件 Schema 是独立版本维度，不得相互替代。
