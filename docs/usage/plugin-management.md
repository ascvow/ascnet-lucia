# 插件管理

插件版 `lucia` 使用 `agent-plugin-manager` 管理下载、完整性、SemVer 依赖、启用状态和独占能力。Plugin Host 只加载已经验证的运行时配置，不参与安装。

## 搜索

```bash
lucia plugin search
lucia plugin search context
```

`query` 是可选的位置参数，按插件名称和说明过滤 Registry。搜索只返回与当前插件 ABI 兼容的版本，不安装任何内容。

## 安装

### 从 Registry 安装

```bash
lucia plugin install context
lucia plugin install context@^0.1
lucia plugin install context --disabled
```

`source` 使用 `name[@semver]`。未指定版本时选择当前 ABI 下最高兼容版本，并先求解必需依赖。`--disabled` 只完成安装和锁文件更新，不让插件进入运行时组合。

### 从 GitHub Release 安装

```bash
lucia plugin install owner/example-plugin --github
lucia plugin install owner/example-plugin --github --tag v1.2.0
lucia plugin install owner/example-plugin --github --asset lucia-plugin-example.zip
```

- `--github`：把 `source` 解释为 `owner/repository` 或完整 GitHub URL，绕过 Registry 依赖求解。
- `--tag <tag>`：指定 Release tag；省略时使用 latest release。
- `--asset <name>`：指定 Release 中的 ZIP asset；省略时按 bundle 规则选择。

私有仓库可通过 `GITHUB_TOKEN` 授权。安装器不会克隆或构建源码，也不会执行插件代码；它只接受包含一个 `plugin.toml` 的预构建 ZIP，并拒绝符号链接、特殊文件和路径穿越。

### 从本地 bundle 安装

```bash
lucia plugin install ./dist/example-plugin --local
```

`--local` 要求 `source` 是本地目录，目录根必须包含 `plugin.toml` 和 manifest 指向的 WASM 文件。安装器复制并校验 bundle，不引用原目录继续运行，因此原目录后续修改不会自动生效。

## 查看与更新

```bash
lucia plugin list
lucia plugin outdated
lucia plugin update context
lucia plugin update
```

- `list`：显示全部受管理插件的版本、来源和启用状态。
- `outdated`：列出 Registry 中存在更高兼容版本的插件。
- `update <id>`：更新一个插件。
- `update`：更新全部存在兼容新版本的插件。

更新会先完成 ZIP、SHA-256、manifest、依赖和能力校验，再原子切换锁记录；失败时旧版本保持可用。

## 启用、禁用与删除

```bash
lucia plugin enable example-plugin
lucia plugin disable example-plugin
lucia plugin remove example-plugin
```

`id` 是 manifest 中的稳定插件 ID，不是 Registry 搜索名称。启用前会验证必需依赖已经安装并启用、版本满足约束且依赖图无循环。删除会检查依赖关系，不能静默破坏仍启用插件的必需依赖。

## 选择独占能力

```bash
lucia plugin select agent.context-loader context-plugin-b
lucia plugin unselect agent.context-loader
```

`capability` 是 manifest 声明的稳定能力 ID，`plugin` 是提供该能力的插件 ID。多个已启用插件声明同一 `exclusive` 能力时，必须用 `select` 指定 owner；选择操作同时确保目标插件已启用。`unselect` 清除选择，之后若仍有多个候选，运行时检查会要求重新选择。

## 安装位置与运行时优先级

受管理插件安装到：

```text
$LUCIA_HOME/plugins/<id>/<version>/
```

锁文件是 `$LUCIA_HOME/plugins.lock.toml`，记录来源、启用状态、manifest 路径和 bundle SHA-256。普通启动按以下优先级合并插件：

1. `--plugin-manifest` 和配置文件 `[[plugins]]`。
2. `lucia plugin install` 管理的已启用插件。
3. `$LUCIA_HOME/official-plugins` 中的官方插件。

同 ID 的高优先级来源覆盖低优先级来源。诊断最终组合使用：

```bash
lucia doctor
lucia doctor --network
```

## 安全与版本边界

插件服务版本、manifest `api_version`、插件 crate 版本和 `plugins.lock.toml` schema 是独立维度。升级其中一个不代表其他版本自动变化。

manifest 权限决定 Host 能否为插件开放文件读取、进程或 Agent Runtime 能力。`process_exec` 是完整原生进程信任，不是 WASM 沙箱内的弱权限；安装第三方插件前应检查其 [Manifest 与权限](/host/manifest-capabilities)。
