# Lucia 官方 Skill 插件

该插件负责扫描、解析和加载 Skill。Plugin Host 只提供受控目录读取、developer prompt 与动态工具注册 API，不包含 Skill 文件格式或选择规则。

Evidence Genome 的 `skills` 非空时，TUI 不扫描目录，也不信任 manifest metadata。它会从真实 Artifact CAS 逐项读取 `SkillArtifactV1`，复核规范 JSON、SHA-256 摘要、强类型 `SkillId` 和完整状态链。Serve 平面只允许终态为 `Active` 的制品；Evaluation 通过运行绑定器把有效策略单调收紧，可装载 `Quarantined`、`Evaluated` 或 `Active` 候选，但不会改写 Candidate Genome 的 Serve execution、Revision ID 或摘要；`Deprecated`、`Deleted` 及 Mutation 平面都拒绝装配。随后，TUI 仅向已绑定且真实提供 `agent.skills` 的唯一插件注入版本化 `skill_set_json`；缺少 provider 或存在多个 provider 都会失败关闭。

Guest 会再次计算每份原始 `artifact_json` 的摘要并核对 Skill ID。Genome 模式下，`skill_read` 直接返回 CAS 固定的 `instructions`，不会读取 `skills/` 目录；成功加载会发出 `skill.loaded.v1` 结构化事件，其中固定包含 `skill_id`、`artifact_digest`、原 Candidate 的 `genome_revision_id`、`genome_digest` 和本次工具调用的 `call_id`，但不会自报任务成功。未注入 `skill_set_json` 时继续使用下述目录扫描行为，以兼容普通安装模式。

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

安装插件版 TUI 后，由用户显式安装 Skill 插件：

```bash
bun run install:tui
lucia plugin install skill
lucia
```

普通安装模式可以通过 manifest 的 `metadata.skills_dir` 修改扫描目录，但该目录必须同时包含在 `capabilities.fs_read` 中；Evidence Genome 模式忽略该值。
