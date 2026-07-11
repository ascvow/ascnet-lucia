# 会话持久化

`agent-session` 为 Agent 会话提供版本化存储协议和本地实现。它依赖
`agent-core`，但不改变 ReAct 循环，也不保存模型配置、密钥或插件状态。

## Core 边界

Core 的 `Session` 只表示与模型服务商无关的完整会话：

- 可选的顶层 system 提示；
- 按顺序保存的 `ModelMessage`；
- serde 序列化与反序列化；
- `from_parts` 和 `into_parts` 所有权转换。

会话 ID、格式版本、修订号和存储位置不属于 Core。持久化恢复出的 `Session` 可以
直接传给 `Agent::run_continue` 或 `Agent::run_session`。

持久化与上下文加载是两个独立边界。存储应保留完整事实历史；模型每轮实际使用的
system 和 messages 仍由 `ContextLoader` 决定。

## SessionRecord

`SessionRecord` 是存储层的版本化包络，当前
`CURRENT_SESSION_SCHEMA_VERSION` 为 `1`。记录包含：

| 字段 | 含义 |
| --- | --- |
| `schema_version` | 持久化格式版本；读取不受支持的版本会失败 |
| `id` | 经过路径安全校验的稳定 `SessionId` |
| `revision` | 最近一次成功保存后的修订号 |
| `created_at_ms` / `updated_at_ms` | UNIX epoch 毫秒时间戳 |
| `title` | 可选的界面展示标题 |
| `metadata` | 与具体插件无关的 JSON 扩展数据 |
| `session` | Core 的完整 `Session` |

`SessionId::generate()` 会生成可直接使用的 UUID v4。手工创建 ID 时，
`SessionId::new` 只接受 1 到 128 个 ASCII 字母、数字、连字符或下划线，避免目录
穿越和文件名注入。反序列化同样执行该校验。

## Revision 与 CAS

`SessionStore::save(record, expected_revision)` 使用比较并交换语义：

- 新 `SessionRecord` 的 revision 为 `0`；传入 `None` 表示仅当记录不存在时创建，
  成功后 revision 为 `1`。
- 更新时传入 `Some(current_revision)`；只有存储中的 revision 完全匹配才会写入，
  成功后自动加一。
- 记录自身携带的 revision 也必须与保存条件一致。
- 过期写入返回 `SessionStoreError::RevisionConflict`，不会覆盖较新的会话。
- `delete` 同样要求提供完全匹配的 revision。

调用方应始终保存 `save` 返回的新记录，并使用其中的新 revision 进行下一次更新。

## 存储实现

`MemorySessionStore` 适合测试和短生命周期进程。它的克隆共享同一份内存状态，并按
会话 ID 排序返回列表。

`FileSessionStore` 在指定目录中为每个会话保存一个 `<session-id>.json` 文件：

1. 在目标文件的同一目录创建唯一临时文件；
2. 写入并同步临时文件；
3. 使用 rename 原子替换目标文件；
4. 同步存储目录。

存储打开时会固定规范化根目录，并拒绝根目录、会话文件、列表项或锁文件上的符号链接，
也会拒绝文件名 ID 与记录 ID 不一致的内容。指向相同规范化目录的所有实例先获取进程内
异步锁，再获取 `.lucia-session.lock` 跨进程独占锁；读取旧 revision、CAS 校验和原子替换
位于同一锁周期，因此多个 Lucia 进程协作写入时不会双写同一 revision。绕过
`FileSessionStore` 直接修改 JSON 的外部程序不受协作锁保护。临时文件与目标文件必须位于
同一文件系统，才能依赖 rename 的原子替换语义。

会话列表使用同目录下的 `.lucia-session-index` 摘要索引。正常的 `list_summaries` 只读取
这一个小文件，不扫描完整消息历史；`save` 与 `delete` 在同一跨进程锁周期内同步失效并
原子更新索引。旧目录首次读取、索引缺失或索引损坏时，会从现有 Session JSON 一次性
重建，因此不要求用户迁移已有会话。

## 最小示例

```rust
use anyhow::{Context, Result};
use agent_core::Agent;
use agent_session::{
    FileSessionStore, SessionId, SessionRecord, SessionStore,
};

async fn persist_and_resume(agent: &Agent) -> Result<()> {
    let store = FileSessionStore::open(".lucia/sessions").await?;
    let id = SessionId::generate();

    let first_run = agent.run("检查当前项目状态").await?;
    let record = SessionRecord::new(id.clone(), first_run.session)?;
    store.save(record, None).await?;

    let restored = store
        .load(&id)
        .await?
        .context("会话记录不存在")?;
    let expected_revision = restored.revision;
    let continued = agent
        .run_continue(restored.session.clone(), "继续并给出下一步建议")
        .await?;

    let mut updated = restored;
    updated.session = continued.session;
    let saved = store.save(updated, Some(expected_revision)).await?;
    println!("会话 {} 已保存为 revision {}", saved.id, saved.revision);
    Ok(())
}
```

`SessionStore` 是异步 trait，应用可以在不修改 Core 的情况下实现数据库、对象存储或
远程会话服务，并保留相同的 revision/CAS 合约。

## TUI 持久化闭环

`lucia` TUI 默认把项目会话根目录设为 `$LUCIA_HOME/projects`，未设置 `LUCIA_HOME` 时
使用 `$HOME/.lucia/projects`。启动目录经过规范化并映射为稳定 `project-id`，实际存储
目录为 `<会话根目录>/<project-id>/sessions`。配置文件 `[tui]` 与 `--sessions-dir <目录>`
覆盖的是根目录，而不是绕过项目隔离。完整启动规则见 [TUI 配置与会话](/guide/tui-configuration)。

普通启动只创建内存 Draft。首次普通消息到达后，TUI 先通过 CAS 保存包含用户输入的
记录，再把该 Session 交给 Agent；模型成功后再次保存助手与工具结果。因此一次完整轮次
通常推进两个 revision。第一次保存失败时，输入会恢复到编辑框且 Agent 不会启动；模型
运行失败时，已经确认的用户输入仍留在会话记录中，后续可以继续或重试。`/new` 和
`/clear` 都切换到新的空白 Draft，不会覆盖原会话文件。

若最终回复保存时发现其他进程已经推进同一 revision，TUI 会把包含完整助手与工具结果的
完成态保存为新的分叉会话并切换过去，避免覆盖并发进程或丢失回复。分叉也无法落盘时，
完整完成态会保留在当前进程内存中，启动队列暂停自动推进，下一次保存可以继续协调。
