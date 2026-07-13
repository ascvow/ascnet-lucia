# 全局诊断

`lucia doctor` 对整个 Lucia 程序执行无侵入诊断：

```bash
lucia doctor
lucia doctor --json
```

默认检查覆盖 Lucia Home、Core/TUI 配置、模型凭据来源、项目 Session 记录、事件日志路径、
受管理插件完整性，以及配置插件、受管理插件和官方插件组成的实际依赖与能力关系。

诊断严格只读：不创建目录或配置，不打开会创建锁和摘要索引的 Session Store，不更新插件
锁文件，不加载或执行 WASM，不调用模型，也不写事件日志。配置缺失、尚无会话等首次运行
状态以警告或通过展示；损坏的记录、锁文件、manifest、依赖和能力冲突返回非零退出码。

默认诊断不联网。需要验证 GitHub API 连通性时显式运行：

```bash
lucia doctor --network
```

`--network` 只发送 GitHub API 读取请求，不下载或安装插件。设置 `GITHUB_TOKEN` 时会用于授权，
报告不会包含 token 或模型密钥内容。
