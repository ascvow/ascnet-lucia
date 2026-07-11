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

审批提示会替换主输入区，并纵向显示“允许一次、允许相似调用、全部放行、取消并暂停 Agent”。使用方向键选择后按 `Enter` 确认，也可以直接按 `Y`、`S`、`C`；`Cmd+A` 开启全部放行。取消会结束当前 Agent 运行并保留会话，相似 Shell 调用按命令族匹配，写文件按父目录匹配；全部放行仍不会绕过敏感路径和工作区边界。Shell 仍是完整宿主进程能力，审批不等价于操作系统级隔离。
