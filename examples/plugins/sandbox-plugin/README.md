# Sandbox Plugin

Sandbox 插件在 Agent 工具执行前应用保守策略，并通过模态 TUI 审批具有副作用的调用。

- `read_file`、`list_directory`、`search_files` 仅允许工作区内的普通相对路径。
- `.env`、私钥、凭据目录和 `.git` 内容始终拒绝读取或写入。
- `write_file`、`shell` 和未知插件工具默认逐次审批。
- 插件不声明文件、进程、HTTP、secret 或 Agent Runtime 能力，激活阶段不会联网。

构建与测试：

```bash
bun run test:plugin:sandbox
```

审批对话框出现后，按 `Enter` 允许一次，按 `Esc` 或 `D` 拒绝。Shell 仍是完整宿主进程能力；审批表示用户明确接受该次命令风险，不等价于操作系统级隔离。
