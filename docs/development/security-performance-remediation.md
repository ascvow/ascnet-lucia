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

## 尚未闭环（不宣称全部解决）

1. **超长时间线逻辑坐标**：总高度与滚动/选区仍有 u16 边界。需要主会话、原生 Agent 视图、
   热区和选区复制一起迁移到大逻辑坐标，终端物理坐标保持 u16。本轮存在并发 TUI 改动，未混入迁移。
2. **附件完全异步化**：读取量已受限，但慢文件系统和编码仍可能占 UI 线程；需要请求 ID、会话/编辑代次、
   并发/总量限制及取消后的结果丢弃，不能简单后台修改草稿。
3. **布局全历史扫描/首帧测量**：保留完整内容指纹失效检查，未完成所有 Msg 修改入口的 revision 管理；
   插件移动事件也未合并，因为不能未经协议确认丢失 hover 路由。
4. **Context 剩余 O(n) 工作**：前缀校验、编码和缓存取用仍有成本；本轮消除了部分副本，不是所有路径零拷贝。
5. **存储进一步优化**：CAS 仍读取旧记录，索引及 fsync 保留。进一步减少需要崩溃注入和格式设计，不能只删同步。
6. **启动完整性扫描**：编译已离开 async worker；历史 Bundle 全量 hash 尚未缓存，不能只凭 mtime 跳过校验。
7. **全能力运行审计**：服务观察器仍不是覆盖 exec/fs/socket/http 的统一持久审计系统，需明确脱敏、轮转和证据契约。
8. **制品发布者签名**：未部署信任根、签名、轮换、撤销体系；仅修正文档，SHA-256 不等于来源认证。
9. **依赖漏洞数据库扫描**：本机没有 cargo-audit，未安装或联网获取数据库；不将编译成功当成漏洞扫描通过。
10. **路径 TOCTOU/旧权限**：末级文件防护不等于全路径句柄锚定；已有会话目录/备份不自动迁移，Windows ACL 未加固。
11. **Core 最小特性测试缺口**：额外运行 `cargo test -p agent-core --no-default-features` 时有三条既有
    provider 测试没有 feature guard（OpenAI/Google factory 未注册），记录为后续测试修正；各 provider 单特性编译通过。

## 验证记录

- `cargo test --workspace --offline`：通过（包含已构建的官方真实 WASM smoke）。
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

现有 `context_load_passthrough` benchmark 实际复用了 8000 token 窗口，较大输入会压缩，
因此不能据其名字声称测得纯透传加速；后续应修正基准分组并增加分配计数。
