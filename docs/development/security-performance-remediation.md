# 安全与性能审查修复闭环

## 范围与方法

依据四份团队审查及二次源码复核实施。原始审查基于 `42659c3`，实施起点为 `d2f5462`；
实施期间其他会话继续修改 TUI，本任务没有覆盖或提交这些无关改动。

成员使用只读 profile，负责精确补丁与独立审查；controller 应用、测试和提交。Core 网络和
Context 原成员发生模型流失败，网络由接替成员交付，Context 由 controller 继续实现。
审查中的估算耗时不是基准；未经复现的结论不按已确认漏洞处理。

## 已完成

| 问题 | 实施及主要验证 |
| --- | --- |
| 初始化服务 ACL 空窗 | Guest 与应用调用入口分离；Guest 代码前登记可信身份；RAII 回滚；真实 WAT 导入调用测试 |
| 授权摘要漏项 | 加入 socket/env/data_dir/collect；官方升级逐项扩权回归；不改变 WIT 或 lock schema |
| 插件无限等待 | 调用及锁等待 deadline；被中断 Store 销毁并隔离；嵌套调用继承父 deadline；取消与真实异步 Host import 测试 |
| 取消卸载遗漏清理 | 独立卸载任务收敛，正常退出也销毁 Store；已有 Permission 取消 smoke 仍通过 |
| WASM 内存数量放大 | 限制 memory/instance/table 数量及表元素；真实多内存 component 回归 |
| Host 文件/目录无界与同步读取 | 常规文件、实际读取限额、目录条目/字节边界；正文读取与遍历移至阻塞池 |
| 动态贡献无界 | 工具/提示/事件条目及字节配额；删除、排空归还；明确错误不静默截断 |
| UI 帧无界 | 解码前字节限制，解码后行/span 数量限制；旧帧与加法字段兼容测试 |
| 插件私有数据目录 | 拒绝最终目录和数据父目录符号链接，Unix 0700；不限制索引数据总容量 |
| Host HTTP 分块读后限额 | 逐块扩容前检查；假服务器不结束响应也能及时拒绝；模型补全复核网络策略 |
| 子进程长行读后限额 | 逐块检查，超限回收句柄；超时保留前缀；stdin 写入有等待上限 |
| 模型传输 | 共享 client，连接/读取空闲超时，禁重定向；JSON/SSE 累计字节边界、错误前缀短读 |
| 模型流无界积压 | 保留同步发送 API，待处理条目/字节预算，超限终止并取消网络任务；正常消费归还预算 |
| SSE 局部平方级拷贝 | 共用线性分块解帧，工具参数原位追加；UTF-8/CRLF/多行/尾部测试；稀疏工具索引扩容保护 |
| 会话文件私密性 | Unix 新目录 0700、新记录/索引/锁 0600；不递归 chmod 已有目录；旧 JSON/CAS 回归 |
| 会话写放大 | 紧凑 JSON；保留原子 rename、索引失效顺序、全部 fsync 和输入先行保存 |
| 原生 read/list 无界 | 8 MiB 文件、256 KiB 单行、1 MiB 页、4096 目录项；饱和分页计算、超限错误 |
| Core 快照重复复制 | 可信追加确认点只同步新增消息；开始/终态完整快照仍保留；事件点前缀与正文分配回归 |
| 观测额外序列化 | 计数 writer 代替 schema/call/result 临时 JSON 字符串；旧估算一致性测试 |
| JSONL 阻塞 | 取得顺序锁后提交 blocking 写入，返回前写完整行，保留轮转语义；新日志 0600 |
| Context 热路径 | 无缓存低水位强类型计数透传；缓存 Arc 快照、锁外复制/销毁；回填移动 JSON 所有权 |
| 压缩临时分配 | 已清理微压缩不重建整树；首尾截断用字符索引而非 Vec<char> |
| 路由转换重复拷贝 | 明确无需清洗的普通路径移动消息；其他路径仍用原规则，私有推理/孤立调用不绕过 |
| Host 事件扇出 | 冻结 tools/prompts 共享正文，记录事件直接取强类型 run_id |
| TUI 无效绘制 | 普通移动按实际悬停/选区变化标脏，所有输入仍路由；插件及原生子视图保守处理 |
| TUI 文本深克隆 | Paragraph 借用行文本，保留样式、对齐；每条变化消息指纹只计算一次 |
| 附件先读后验 | 统一普通文件打开、FIFO 防护、实际有界读取，含类型嗅探入口 |
| 终端控制符 | 探针确认控制符会到达 Cell；最终显示缓冲清洗，不修改原始内容、ZWJ、组合符、RTL |
| 权限及明文传输提示 | TUI 显示未配置审批策略和 HTTP 提示；owner 来源为通用 Host 查询，不硬编码插件 ID |
| 安全说明不一致 | 校正 HTTP 已开放、进程硬上限、socket 连接预算；明确 checksum 不是发布者签名 |

运行时具体限额以代码及 `docs/host/manifest-capabilities.md`、`docs/agent/tools-events.md` 为准。

## 明确保留的设计约束

- `process_exec` / Shell 是完整原生进程信任，不以命令字符串黑名单冒充 OS 沙箱。
- Serve 的累计模型配额仍为 None，不给 teammate 注入步数上限。
- 前缀哈希仍检测历史改写，不能只因消息数增加就信任旧前缀。
- `AgentState.session`、流的混合消费、会话 CAS、崩溃恢复语义不删减。
- 不因文件行数重构、不改 workspace/release 优化档位、不移除 JSON preserve_order。
- deadline 是协作式约束，不能硬终止已开始的阻塞系统调用或撤回外部副作用。

## 第二批续修（基于 `ebd379f`）

- **长时间线已迁移**：主会话、原生 Agent 视图、工具热区、选区使用 `usize` 逻辑行号；
  终端矩形保持 u16。70,000 行尾部、超长单行、跨范围复制和极大滑块比例回归通过。
  可见消息保存逻辑行的视觉终点索引，二分定位需要绘制的段；复制分页使用局部原点。
- **附件已后台化**：生产 TUI 的 `/attach`、粘贴文件识别/类型嗅探、文件读取、编码及提交前正文路径识别
  在阻塞线程批次完成。单 App 一个待处理批次、全局两个许可、单批最多 16 文件、待附加编码预算 32 MiB；
  许可随结果保留至 UI 消费，取消不会提前释放真实任务的许可。会话/草稿/光标/模型/编辑代次不符丢弃结果。
  Esc/Ctrl+C 可取消结果提交；失败批次保留草稿、不自动发送缺附件的内容。测试覆盖立即 Enter 与原生命令顺序。
- **启动校验去重**：`runtime_config` 同一次操作只对每个当前/历史 Bundle 完整 hash 一次，复用同一锁快照；
  manifest 解析使用实际参与摘要的字节。下一次调用重新校验，保留相同 mtime 篡改回归。
- **CAS 热路径收敛**：完整正文解码移到阻塞线程、准备新记录后尽早释放旧正文；不跳过正文类型/schema/ID/绑定校验。
  新测试证明损坏记录不会被写入或导致摘要索引失效。
- **能力审计已接通**：全部自定义 `host-*` import 在真实 linker 边界记录可信插件 ID、固定入口名与结果；
  取消记为未知副作用 `interrupted`。可选 `--capability-audit-dir` 写入私密轮转文件，详见 Host 能力文档。
  不包含 WASI 的逐文件系统调用，也不是 Shell 子进程内部行为或同 UID 防篡改审计。
- **Unix 读取路径加固**：通用原生文件打开机制放在既有中立 `agent-tool::workspace`，Host/TUI 通过既有依赖复用；
  按已授权规范路径逐段 `openat`，每段拒绝符号链接，测试覆盖校验后父目录替换。未增加跨 crate 依赖。
- **Core 最小特性已修正**：协议专属测试按 feature 编译，配置解析测试不依赖 provider；无默认特性测试和 Clippy 通过。
- **基准场景已分离**：透传窗口 1,000,000 token，断言零摘要调用；缓存增量窗口 8,000，断言摘要真实发生。
- **RustSec 已执行并修复**：cargo-audit 0.22.2 实测，数据库提交 `21d909e4c8bd4e14c7aea58bf980c881a0053ab1`
  （2026-10-24）。更新前主锁文件有 38 条漏洞匹配，以及 lru/paste 等警告；更新后全部 13 份已跟踪锁文件
  均为 **0 漏洞、0 警告**。不能以先前“依赖不旧”判断替代扫描结果。
  Wasmtime/WASI 48.0.7、Rustls 0.23.46、WebPKI 0.103.16、ZIP 9.1.0 等定向升级；
  Ratatui 0.30.2 / Markdown 0.3.12 / Crossterm 0.29 移除旧 lru 和未维护 paste。
  WIT、manifest api_version、workspace crate 版本和 lock schema 均未联动变更。
  CI 每周、依赖 PR 和手动执行所有锁文件扫描并拒绝 warning；工具安装在 ignored target，不替换用户 lucia。

## 尚未闭环及刻意保留的边界

1. **制品发布者签名**：仍无项目维护者指定的可信公钥、签名发布流程、轮换/撤销策略。不能由 Agent 生成
   一个自签根然后宣称供应链可信；SHA-256 校验仍不等于发布者身份认证。
2. **布局全历史内容扫描**：消息字段目前可由多处修改，保留完整指纹检测以覆盖同长度改写；未引入全局 revision
   管理。宽度变化需要重新测量；插件移动事件不丢弃。已减少窗口内排版和复制，不宣称整帧 O(视口)。
3. **Context/存储的剩余 O(n)**：前缀改写校验、完整 CAS 旧记录读取、索引和 fsync 仍存在；不引入未经故障注入
   验证的新格式或以删同步换性能。启动只去除同次重复 hash，不能跨启动只按 mtime 缓存。
4. **文件系统完整隔离**：Unix 普通读取已逐段拒绝符号链接；目录枚举、写路径、硬链接/恶意重命名/挂载替换
   和 Windows ACL 不因此获得完整隔离。旧会话目录/备份不自动 chmod，需用户选择迁移范围。
5. **审计可靠性等级**：可选本地旁路、fail-open、有界丢弃且 UI 报告；不是每次副作用前持久化的 fail-closed
   证据链，不覆盖外部原生进程内部行为，系统崩溃可能丢失队尾记录。
6. **后台 I/O 取消**：取消丢弃结果并保留并发许可，不硬杀 OS 文件操作；32 MiB 是待附加内容预算，
   不是包括原始字节和编码工作区在内的进程峰值内存上限。

## 验证记录

- `cargo test --workspace --offline --locked`：通过，第二批最终 **1292 passed / 7 ignored**（包含已构建的官方真实 WASM smoke）。
- `cargo test -p agent-core --no-default-features --offline --locked`：97 passed / 2 ignored；纯 Core TUI 402 passed / 2 ignored。
- 全部 13 份已跟踪锁文件 `cargo-audit audit --deny warnings`：0 漏洞 / 0 警告。
- `cargo clippy --workspace --all-targets --offline -- -D warnings`：通过。
- `cargo fmt --all -- --check`：通过。
- `cargo test --offline -p lucia --features plugins` 与 `--no-default-features`：通过。
- `agent-plugin` / `agent-plugin-host` / `agent-plugin-manager` 对应测试：通过。
- Permission / Teammate / Plan / Eden smoke：通过；MCP 的 ignored stdio smoke 显式执行通过。
- 插件版及纯 Core 版 build 后运行 `python3 scripts/pty-render-check.py`：均通过。
- 未运行真实付费模型请求；PTY 验证字节协议，不替代所有终端图形后端人工验收。

### 基准的限制与观察

本次 release 小样本、同机固定模型基准：1024 条历史的一轮 Core 循环从约 924 µs 到 515 µs；
Context 增量场景约 2682 µs 到 2636 µs，基本持平。负载、编译和其他并发任务会影响结果，
这不是端到端模型延迟保证，不用小样本百分比作为 CI 门槛。

第一批的 `context_load_passthrough` benchmark 复用了 8000 token 窗口，较大输入会压缩，
这些历史数字不能作为纯透传收益。第二批已修正窗口和调用断言；尚未增加分配字节计数。
