# Lucia 官方 Skill 插件

该插件负责扫描、解析和加载 Skill。Plugin Host 只提供受控目录读取、developer prompt 与动态工具注册 API，不包含 Skill 文件格式或选择规则。

## 目录格式

每个 Skill 使用独立目录和 `SKILL.md`：

```text
skills/
  lucia-plugin-development/
    SKILL.md
```

`SKILL.md` 必须以 YAML frontmatter 开头，并包含 `name` 与 `description`。描述会在插件激活时注入 Agent；正文只在模型调用 `skill_read` 后加载，避免把所有完整指令提前放入上下文。

```markdown
---
name: lucia-plugin-development
description: 开发 Lucia WASM 插件时使用。
---

# Lucia 插件开发

按项目插件 ABI 和 manifest 规范完成实现。
```

## 构建与测试

```bash
bun run build:plugin:skill
bun run test:plugin:skill
```

在插件版 TUI 中加载：

```bash
bun run install:tui:plugins
lucia --plugin-manifest examples/plugins/skill-plugin/plugin.toml
```

可以通过 manifest 的 `metadata.skills_dir` 修改扫描目录，但该目录必须同时包含在 `capabilities.fs_read` 中。
