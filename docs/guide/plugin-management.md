# 插件管理

`agent-plugin` 管理本地插件 bundle、启用状态、独占能力选择和完整性锁文件。它不加载 WASM，也不实现 MCP、Skill 或上下文压缩；应用通过 `PluginManager::runtime_config()` 把校验后的 manifest 路径和能力选择交给 Plugin Host。

## 安装命令

先安装管理器并指定受管理根目录：

```bash
cargo install --path crates/ascnet-lucia-plugin-manager --locked
export LUCIA_PLUGIN_ROOT="$HOME/.lucia"
```

安装源必须是本地目录，根目录包含 `plugin.toml`，manifest 中的 WASM 文件也必须已经存在：

```bash
agent-plugin install ./dist/example-plugin
agent-plugin list
```

默认安装后立即启用。需要先准备依赖或解决能力冲突时，先以禁用状态安装：

```bash
agent-plugin install ./dist/context-plugin-b --disabled
agent-plugin enable context-plugin-b
agent-plugin disable context-plugin-b
agent-plugin remove context-plugin-b
```

也可以不设置环境变量，在每条命令中传入 `--root /path/to/root`。

仓库开发时可由 Bun 统一启动同一个 CLI，例如：

```bash
bun run plugin -- --root /path/to/root list
```

管理器把 bundle 复制到 `<root>/plugins/<id>/<version>/`，并原子更新 `<root>/plugins.lock.toml`。锁文件记录插件身份、启用状态、manifest 相对路径和 bundle SHA-256；不要手工移动受管理目录或修改锁文件。

## 依赖约束

插件依赖由 manifest 的 `[[dependencies]]` 声明：

```toml
[[dependencies]]
id = "command"
version = "^1.2"
optional = false
```

启用插件前，管理器会验证必选依赖是否已安装并启用、版本是否符合 SemVer 约束，以及依赖图是否存在循环。仍被启用插件依赖的 provider 不能禁用或移除。

建议按以下顺序安装一组互相依赖的插件：

1. 以禁用状态安装 provider 和 dependent。
2. 先启用 provider。
3. 再启用 dependent。
4. 运行 `agent-plugin doctor`。

依赖只表达安装和加载顺序；实际跨插件调用仍使用[插件服务 API](/plugin/dependencies-services)。

## 选择独占能力

多个插件可以声明同一个 `exclusive` 能力，但 Host 不会按安装顺序覆盖。先安装候选插件，再显式选择 owner：

```bash
agent-plugin select agent.context-loader context-plugin-b
```

`select` 会启用目标插件，并把选择写入锁文件。目标插件必须声明对应的独占能力；为 `multi` 能力选择 owner 会失败。

清除选择使用：

```bash
agent-plugin unselect agent.context-loader
```

清除后若仍有多个已启用 provider，操作会被拒绝。应先禁用多余 provider，再执行 `unselect`。

## 完整性诊断

```bash
agent-plugin doctor
```

诊断会检查：

- 锁文件版本、重复插件 ID 和受管理路径。
- manifest 与 WASM 是否存在且有效。
- bundle 当前 SHA-256 是否与安装记录一致。
- 已启用插件的依赖、版本约束和循环依赖。
- 独占能力冲突及 owner 选择。

`list` 只读取锁文件，不重新计算完整性；发布、升级或启动插件版应用前应运行 `doctor`。诊断失败会返回非零退出码。

安装过程拒绝符号链接和特殊文件，避免 bundle 越过受管理根目录。凭据仍应放在应用忽略的本地配置或环境变量中，不应打入 bundle。
