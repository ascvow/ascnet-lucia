# 插件性能分析

Lucia 把插件性能验证分成编译边界和 Host 微基准。测试目标不是证明任意第三方插件都足够快，而是持续量化“启用插件系统后新增了多少开销”。

## 纯 Core 边界

常规 TUI 构建默认启用 `plugins` feature。需要验证纯 Core 边界时，必须显式使用 `--no-default-features`，该构建不会编译或链接 Plugin Host、Wasmtime、WASI 和插件 UI：

```bash
bun run build:tui:core
cargo tree -p lucia --no-default-features -e normal
```

依赖树中不应出现 `agent-plugin-host`、`wasmtime` 或 `wasmtime-wasi`。这条边界保证未启用插件系统的用户不承担 WASM 运行时的启动、内存和二进制体积成本。

## Host 微基准

```bash
bun run perf:plugin:host
```

微基准覆盖空组合宿主、八插件 prompt 和工具列表聚合、工具 owner 查询、工具调用、TUI 渲染与输入 owner 路由，以及上下文 owner 派发。结果按行输出 JSON，包含 `ns_per_op`、基线名称和相对倍率。

可以扩大预热和采样次数：

```bash
LUCIA_BENCH_WARMUP=2000 \
LUCIA_BENCH_ITERATIONS=50000 \
bun run perf:plugin:host
```

Host 微基准标记为 `informational_only`，不会自行判定回归。共享 CI 的调度和频率变化容易让纳秒级结果抖动，应在相同机器和 release 配置下保存历史结果，再比较趋势。

插件版 TUI 在首次创建 Wasmtime Engine 前把持久化编译缓存配置为
`$LUCIA_HOME/cache/wasmtime`。缓存目录不可用时自动退回无缓存模式，不阻止启动。
渐进加载状态会记录每个插件从 component 编译到 Ready 的总毫秒数，用于定位需要继续
拆分 `activate` 或外部服务初始化的长尾插件。

Host 默认通过 Wasmtime fuel 限制单次 guest 计算，并将单个线性内存限制为 `64 MiB`。性能结果只覆盖框架路由；插件作者仍需为网络、进程、文件扫描和 TUI 渲染分别设置预算，并避免在 Agent 同步调用路径中执行无界工作。

## 构建缓存

仓库内插件与宿主共享根 `target` 目录；插件构建脚本只把最终 WASM 文件复制到对应插件目录，不会为每个插件重复保存依赖缓存。开发和测试 profile 关闭增量编译，只为工作区代码保留行号，并关闭第三方依赖调试符号，避免频繁切换 feature、测试目标和历史 crate 图后持续积累完整调试产物。

长期开发后可以先查看 debug profile 将删除的内容，再清理可重新生成的缓存：

```bash
bun run cache:clean:debug:dry-run
bun run cache:clean:debug
```

该命令只清理 debug profile，不删除 release TUI 或 `wasm32-wasip2` release 插件产物。清理后的第一次开发构建需要重新编译依赖。
