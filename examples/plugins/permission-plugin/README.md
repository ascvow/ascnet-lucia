# Permission Plugin

权限插件在 Agent 工具执行前按 Host 从 manifest 绑定的可信 `ToolPermissionScope` 应用保守策略；手动模式通过模态 TUI 审批触达宿主资源的调用。

- `Control` 只表示 Lucia 内部控制面操作，直接放行；权限插件不维护官方工具名白名单。
- `Read`、`Write`、`Shell`、`Network` 和旧插件默认的 `Unknown` 进入当前审批模式。
- `Read`、`Write` 调用携带 `path` 时只允许工作区内普通相对路径；`.env`、私钥、凭据目录和 `.git` 内容始终拒绝。
- 默认使用“帮我审批”。`/permission` 打开权限模式选择，可切换“手动审批、帮我审批、全部放行”；也支持 `/permission manual`、`/permission assist`、`/permission allow-all`。TUI 按项目摘要隔离并跨重启保存模式，写入失败不会改变当前模式，当前值常驻显示在底栏。
- “帮我审批”直接在工具前置链中通过 Host 固定的无工具小模型路由审核有界脱敏参数，不依赖 TUI 渲染；写入与网络正文只保留规模，URL 去除 userinfo、查询参数和 fragment，带凭据 URL 的 Shell 命令整体隐藏。只接受 `{"decision":"allow"}` 或带非空字符串 `reason` 的 `{"decision":"deny",...}`；纯文本、重复字段、`null` 和额外字段都视为无效结论。允许时执行；拒绝、调用失败或响应无效时只阻止当前工具，不取消 Agent。
- 详细失败原因只进入工具事件和界面；下一次模型请求只接收按可信 scope 生成的行动建议，不包含审核模型的自由文本原因，也不写入 Session。
- 插件不申请文件、进程、HTTP、secret 或 Agent Runtime 能力；模型代审仅申请受控 `model_completion`，命令面板只获准 `set_input`，不能清空会话或退出应用。

构建与测试：

```bash
bun run test:plugin:permission
```

审批提示会替换主输入区，以带边框的纵向卡片直接展示脱敏操作对象、scope 对应的原因、作用范围和风险。使用上下方向键选择后按 `Enter` 确认，也可以直接按 `A` 允许本次；`S` 只对同目录只读调用建立本次 Agent 运行规则，写入、Shell、网络和 Unknown 必须逐次批准；规则不会跨 Agent 运行复用，并在运行结束时清除。按 `D` 或 `Esc` 拒绝。拒绝只阻止当前调用并让 Agent 调整方案继续；全部放行只通过 `/permission` 模式切换，且不会绕过可信读写 scope 的敏感路径和工作区边界。Shell 仍是完整宿主进程能力，审批不等价于操作系统级隔离。
