# 离线检查

Lucia 的标准验证全部可以离线完成，不依赖网络，也不依赖真实模型。提交前应保证这套检查通过。

## 统一入口

```bash
bun run check
```

该命令按固定顺序执行下列步骤，任意一步失败立即中止，并以该步骤的退出码退出：

| 步骤 | 命令 | 说明 |
| --- | --- | --- |
| `fmt` | `cargo fmt --all -- --check` | 检查代码格式，不自动改写 |
| `clippy` | `cargo clippy --workspace --all-targets --offline -- -D warnings` | warning 视为错误 |
| `build:plugin:official` | `bun run build:plugin:official` | 构建官方插件 WASM |
| `build:plugin:all` | `bun run build:plugin:all` | 构建全部示例插件 WASM |
| `test` | `cargo test --workspace --offline` | workspace 单元与集成测试 |
| `build:tui:core` | `cargo build --offline -p lucia --no-default-features` | 无插件 TUI |
| `build:tui:plugins` | `cargo build --offline -p lucia --features plugins` | 启用插件的 TUI |

格式与静态检查排在最前，失败最快。插件 WASM 必须排在 `test` 之前：插件冒烟测试直接加载
`target/wasm32-wasip2/release` 下的 component，干净的源码 checkout 里没有这些产物，顺序颠倒
会让测试因缺少文件而失败。TUI 的两种构建变体最慢，放在最后。

工具链由 `rust-toolchain.toml` 固定到具体补丁版本。`clippy` 用 `-D warnings`，浮动工具链新增的
lint 会与改动内容无关地把检查变红；确有必要放行的 lint 只能写进根 `Cargo.toml` 的
`[workspace.lints]`，并注明为什么无法在源码中消除。

## 局部执行

完整检查耗时较长。修复某一步后，不必从头再跑：

```bash
bun run check --list              # 只打印将要执行的命令
bun run check --only fmt,clippy   # 只执行指定步骤
bun run check --from test         # 从指定步骤开始，执行其后全部步骤
```

`--only` 优先于 `--from`。引用不存在的步骤 id 会直接报错，避免检查被静默跳过。

## 不包含的内容

以下测试需要显式触发，不属于标准离线检查：

- 需要 `--ignored` 的插件冒烟测试，见 `package.json` 中对应的 `test:plugin:*` 脚本。
