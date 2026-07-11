# 插件性能分析

Lucia 把性能验证分成编译边界、Host 微基准和真实 WASM 探针。测试目标不是证明任意第三方插件都足够快，而是持续量化“启用插件系统后新增了多少开销”，并让超出预算的真实 component 阻止合入。

## 纯 Core 边界

默认 TUI 构建不启用 `plugins` feature，因此不会编译或链接 Plugin Host、Wasmtime、WASI 和插件 UI：

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

## 真实 WASM 探针

```bash
bun run perf:plugin:wasm
```

该命令使用真实上下文替换 component，分别输出：

- `wasm_component_load`：component 编译、实例化和激活耗时。
- `core_context_passthrough`：Core 直通 ContextLoader 基线。
- `wasm_context_compression`：经过 Host、WIT 和 guest 的上下文压缩耗时。

每组调用结果使用纳秒输出，包含 `total_ns`、`p50_ns`、`p95_ns` 和 `max_ns`。`LUCIA_PERF_ITERATIONS` 可调整正式样本数，默认值是 200，且至少执行 10 次。

## 性能门禁

```bash
bun run perf:plugin:gate
```

门禁同时检查两项预算：`wasm_context_compression` 的 p95 默认不超过 500 微秒，`wasm_component_load` 的冷启动默认不超过 250 毫秒。输出仍使用纳秒，环境变量分别使用微秒和毫秒，避免门禁配置出现过长数值。

可以按稳定测试机的历史基线收紧两项预算：

```bash
LUCIA_PLUGIN_CONTEXT_P95_US=300 \
LUCIA_PLUGIN_LOAD_MAX_MS=200 \
bun run perf:plugin:gate
```

超过预算时命令返回非零退出码。门禁应运行在固定资源的 CI runner；本地开发机适合定位趋势，不适合作为跨机器的统一绝对基线。

Host 默认通过 Wasmtime fuel 限制单次 guest 计算，并将单个线性内存限制为 `64 MiB`。性能结果只覆盖框架路由和测试插件；插件作者仍需为网络、进程、文件扫描、上下文压缩算法和 TUI 渲染分别设置预算，并避免在 Agent 同步调用路径中执行无界工作。
