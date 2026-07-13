# 插件管理

插件版 `lucia` 内置插件安装和状态管理命令。应用层只处理命令参数和输出；GitHub 获取、
安全解包、manifest 校验、依赖解析、完整性锁和原子安装由 `agent-plugin-manager` 负责。
Plugin Host 只消费验证后的运行时配置，不参与下载或安装。

## 从 GitHub 安装

裸名称默认解析为 `ascvow/<name>`，第三方插件使用 `owner/repository` 或完整 GitHub URL：

```bash
lucia plugin install example-plugin
lucia plugin install owner/example-plugin
lucia plugin install https://github.com/owner/example-plugin
```

安装器读取 latest GitHub Release。指定版本或包含多个 ZIP 时使用：

```bash
lucia plugin install owner/example-plugin --tag v1.2.0
lucia plugin install owner/example-plugin --asset lucia-plugin-example.zip
```

Release 必须包含预构建 ZIP bundle，且 ZIP 内只能有一个 `plugin.toml`。安装器不会克隆仓库、
构建源码或执行插件代码；它会限制下载和解压大小，拒绝符号链接、特殊文件及路径穿越。
Release 可以同时提供 `<asset>.sha256` 或同名 `.sha256` 文件，存在时安装前必须校验通过。
私有仓库可通过 `GITHUB_TOKEN` 授权，命令不会展示或保存 token。

默认安装后立即启用。需要先处理依赖或独占能力冲突时使用：

```bash
lucia plugin install owner/example-plugin --disabled
lucia plugin enable example-plugin
lucia plugin disable example-plugin
lucia plugin remove example-plugin
```

## 本地 bundle

开发中的 bundle 可以从本地目录安装，根目录必须包含 `plugin.toml` 和 manifest 指向的 WASM：

```bash
lucia plugin install ./dist/example-plugin --local
```

本地与 GitHub 安装进入同一个 `$LUCIA_HOME/plugins/<id>/<version>/` 目录，并原子更新
`$LUCIA_HOME/plugins.lock.toml`。锁文件记录来源、启用状态、manifest 路径和 bundle SHA-256。

## 列举与能力选择

```bash
lucia plugin list
lucia plugin select agent.context-loader context-plugin-b
lucia plugin unselect agent.context-loader
```

依赖由 manifest 的 `[[dependencies]]` 声明。启用插件前会验证必选依赖是否已经安装并启用、
版本是否符合 SemVer 约束，以及依赖图是否存在循环。多个插件声明同一个 `exclusive` 能力时，
必须通过 `select` 明确 owner。

## 运行时加载

普通 `lucia` 启动按以下优先级合并插件：

1. `--plugin-manifest` 和配置文件 `[[plugins]]`。
2. `lucia plugin install` 管理的已启用插件。
3. 安装器同步到 `$LUCIA_HOME/official-plugins` 的官方插件。

同 ID 的高优先级声明覆盖低优先级来源。受管理插件在进入 Host 前必须通过锁文件、完整性、
依赖和能力检查；诊断所有来源的实际启动组合使用 [`lucia doctor`](/guide/doctor)。
