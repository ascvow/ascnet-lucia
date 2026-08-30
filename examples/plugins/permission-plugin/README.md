# Permission Plugin

权限插件在 Agent 工具执行前应用保守策略，并通过模态 TUI 审批具有副作用的调用。

- `read_file`、`list_directory`、`search_files` 仅允许工作区内的普通相对路径。
- `.env`、私钥、凭据目录和 `.git` 内容始终拒绝读取或写入。
- `write_file`、`shell` 和未知插件工具默认逐次审批。
- 默认使用“帮我审批”。`/permission` 打开权限模式选择，可切换“手动审批、帮我审批、全部放行”；也支持 `/permission manual`、`/permission assist`、`/permission allow-all`。模式在 Host 启用持久状态时跨重启保存，写入失败不会改变当前模式，当前值常驻显示在 TUI 底栏。
- “帮我审批”通过 Host 固定的无工具小模型路由审核脱敏摘要，只接受 `ALLOW` 或 `DENY`；调用失败或响应无效时退回手动审批。
- 插件不申请文件、进程、HTTP、secret 或 Agent Runtime 能力；模型代审仅申请受控 `model_completion`，命令面板仅用 `surface_actions` 清空已消费输入。

构建与测试：

```bash
bun run test:plugin:permission
```

审批提示会替换主输入区，以带边框的纵向卡片直接展示脱敏操作对象、原因、作用范围和风险。使用上下方向键选择后按 `Enter` 确认，也可以直接按 `A` 允许本次、按 `S` 在本会话允许同类调用、按 `D` 或 `Esc` 拒绝。拒绝会结束当前 Agent 运行并保留会话，相似 Shell 调用按命令族匹配，写文件按父目录匹配；全部放行只通过 `/permission` 模式切换，且与模型代审一样不会绕过敏感路径和工作区边界。Shell 仍是完整宿主进程能力，审批不等价于操作系统级隔离。
