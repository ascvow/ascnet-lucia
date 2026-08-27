# 插件管理

插件版 `lucia` 内置 Registry 安装、搜索、更新和状态管理命令。应用层只处理命令参数和输出；
Registry、GitHub 获取、安全解包、SemVer 依赖求解、完整性锁和原子安装由
`agent-plugin-manager` 负责。
Plugin Host 只消费验证后的运行时配置，不参与下载或安装。

## 从 Registry 安装

默认从 `ascvow/lucia-plugins` 最新 Release 的 `registry.json` 解析名称。支持 npm 风格的
`name@semver` 请求；未指定版本时选择当前插件 ABI 下的最高兼容版本，并先安装必需依赖：

```bash
lucia plugin search
lucia plugin search mcp
lucia plugin install mcp
lucia plugin install mcp@^0.1
lucia plugin outdated
lucia plugin update mcp
lucia plugin update
```

已安装版本会作为固定依赖约束复用；版本冲突不会静默替换。更新时先完成新 ZIP、SHA-256、
manifest、依赖与能力校验，再原子切换锁记录，失败时旧版本保持可用。

## 从任意 GitHub Release 安装

第三方插件可以绕过 Registry，使用 `owner/repository` 或完整 GitHub URL。此模式不自动求解
Registry 依赖：

```bash
lucia plugin install owner/example-plugin --github
lucia plugin install https://github.com/owner/example-plugin --github
lucia plugin install owner/example-plugin --github --tag v1.2.0
lucia plugin install owner/example-plugin --github --asset lucia-plugin-example.zip
```

Release 必须包含预构建 ZIP bundle，且 ZIP 内只能有一个 `plugin.toml`。安装器不会克隆仓库、
构建源码或执行插件代码；它会限制下载和解压大小，拒绝符号链接、特殊文件及路径穿越。
Release 可以同时提供 `<asset>.sha256` 或同名 `.sha256` 文件，存在时安装前必须校验通过。
私有仓库可通过 `GITHUB_TOKEN` 授权，命令不会展示或保存 token。

默认安装后立即启用。需要先处理依赖或独占能力冲突时使用：

```bash
lucia plugin install mcp --disabled
lucia plugin enable example-plugin
lucia plugin disable example-plugin
lucia plugin remove example-plugin
```

## 本地 bundle

开发中的 bundle 可以从本地目录安装，根目录必须包含 `plugin.toml` 和 manifest 指向的 WASM：

```bash
lucia plugin install ./dist/example-plugin --local
```

Registry、本地与 GitHub 安装进入同一个 `$LUCIA_HOME/plugins/<id>/<version>/` 目录，并原子更新
`$LUCIA_HOME/plugins.lock.toml`。锁文件记录来源、启用状态、manifest 路径和 bundle SHA-256。

## 维护官方插件

官方插件继续使用独立 crate、独立插件版本和独立 `api_version`。版本、ABI、说明和依赖只维护在
各插件的 `plugin.toml`；构建和发布打包共享
`registry/official-plugins.json`，不会把 Lucia 程序版本与插件版本联动。

发布步骤：

1. 在插件 `plugin.toml` 更新该插件自身版本，并完成独立 workspace 与真实 WASM Host smoke test。
2. 新增插件或改变 bundle 文件时更新 `registry/official-plugins.json`；每批发布使用新的 Release tag。
3. 运行 `bun run package:plugin:official`，生成 `dist/plugin-release/registry.json`、各插件 ZIP 和 `.sha256`。
4. 将该目录全部文件上传到清单指定的 GitHub Release，并把该 Release 设为 latest。
5. 使用 `lucia plugin search`、`lucia plugin install <name>` 和 `lucia doctor --network` 验证发布结果。

`registry.json` 的 schema、插件 manifest `api_version`、各 crate 版本和 `plugins.lock.toml` schema
是四个独立版本维度，不得相互替代。

## 列举与能力选择

```bash
lucia plugin list
lucia plugin select agent.tool-policy permission
lucia plugin unselect agent.tool-policy
```

依赖由 manifest 的 `[[dependencies]]` 声明。启用插件前会验证必选依赖是否已经安装并启用、
版本是否符合 SemVer 约束，以及依赖图是否存在循环。多个插件声明同一个 `exclusive` 能力时，
必须通过 `select` 明确 owner。

## 运行时加载

普通 `lucia` 启动只按以下优先级合并用户明确选择的插件：

1. `--plugin-manifest` 和配置文件 `[[plugins]]`。
2. `lucia plugin install` 管理的已启用插件。
同 ID 的高优先级声明覆盖低优先级来源。配置文件中的 `disabled_plugins` 最后生效，按插件
ID 从最终组合中剔除任意来源的插件：

```toml
disabled_plugins = ["teammate", "plan"]
```

受管理插件在进入 Host 前必须通过锁文件、完整性、
依赖和能力检查；诊断所有来源的实际启动组合使用 [`lucia doctor`](/guide/doctor)。
