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
| `test` | `cargo test --workspace --offline` | workspace 单元与集成测试 |
| `build:plugin:official` | `bun run build:plugin:official` | 构建官方插件 WASM |
| `build:plugin:all` | `bun run build:plugin:all` | 构建全部示例插件 WASM |
| `build:tui:core` | `cargo build --offline -p lucia --no-default-features` | 无插件 TUI |
| `build:tui:plugins` | `cargo build --offline -p lucia --features plugins` | 启用插件的 TUI |

顺序按耗时递增排列：格式和静态检查最先失败，WASM 与 TUI 构建放在最后。

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

- 真实模型测试：`bun run test:live`，需要有效的模型凭据。
- WASM 性能门禁：`bun run perf:plugin:gate`。
- 需要 `--ignored` 的插件冒烟测试，见 `package.json` 中对应的 `test:plugin:*` 脚本。

真实模型测试不会进入普通离线 CI。
