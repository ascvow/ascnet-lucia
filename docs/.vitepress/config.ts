import { defineConfig } from "vitepress";

/** Lucia 中文文档站配置，按使用与开发板块组织导航并发布到 GitHub Pages 子路径。 */
export default defineConfig({
  base: "/ascnet-lucia/",
  lang: "zh-CN",
  title: "Lucia",
  description: "Lucia TUI、CLI、插件管理与 Rust 开发文档",
  cleanUrls: true,
  lastUpdated: true,
  head: [
    ["meta", { name: "theme-color", content: "#0b7a75" }],
    ["meta", { name: "color-scheme", content: "light dark" }],
  ],
  themeConfig: {
    nav: [
      {
        text: "用户指南",
        items: [
          { text: "TUI 使用", link: "/usage/tui" },
          { text: "CLI 使用", link: "/usage/cli" },
          { text: "插件管理", link: "/usage/plugin-management" },
          { text: "其他", link: "/usage/other" },
        ],
      },
      {
        text: "开发者指南",
        items: [
          { text: "插件开发", link: "/development/plugin" },
          { text: "二次开发", link: "/development/custom" },
          { text: "离线检查", link: "/development/checks" },
        ],
      },
      {
        text: "API 参考",
        items: [
          { text: "Rust API 总览", link: "/reference/rust-api" },
          { text: "Core 与 Runtime API", link: "/reference/rust-core" },
          { text: "插件 API", link: "/reference/rust-plugin" },
          { text: "WIT API 0.6", link: "/reference/wit" },
        ],
      },
    ],
    sidebar: {
      "/usage/": [
        {
          text: "用户指南",
          items: [
            { text: "TUI 使用", link: "/usage/tui" },
            { text: "CLI 使用", link: "/usage/cli" },
            { text: "插件管理", link: "/usage/plugin-management" },
            { text: "其他", link: "/usage/other" },
          ],
        },
      ],
      "/development/": [
        {
          text: "开发者指南",
          items: [
            { text: "插件开发", link: "/development/plugin" },
            { text: "二次开发", link: "/development/custom" },
            { text: "离线检查", link: "/development/checks" },
          ],
        },
      ],
      "/guide/": [
        {
          text: "指南与架构",
          items: [
            { text: "快速开始", link: "/guide/quick-start" },
            { text: "架构边界", link: "/guide/architecture" },
            { text: "构建与打包", link: "/guide/distribution" },
            { text: "插件性能", link: "/guide/performance" },
            { text: "真实模型测试", link: "/guide/live-testing" },
          ],
        },
      ],
      "/agent/": [
        {
          text: "Agent Core",
          items: [
            { text: "Agent API", link: "/agent/api" },
            { text: "Agent Runtime", link: "/agent/agent-runtime" },
            { text: "会话持久化", link: "/agent/session-persistence" },
            { text: "上下文加载", link: "/agent/context-loader" },
            { text: "工具与事件", link: "/agent/tools-events" },
          ],
        },
      ],
      "/host/": [
        {
          text: "Plugin Host",
          items: [
            { text: "宿主 API", link: "/host/overview" },
            { text: "Manifest 与权限", link: "/host/manifest-capabilities" },
            { text: "工具 owner 路由", link: "/host/tool-routing" },
          ],
        },
      ],
      "/plugin/": [
        {
          text: "WASM 插件参考",
          items: [
            { text: "官方插件", link: "/plugin/official" },
            { text: "生命周期", link: "/plugin/lifecycle" },
            { text: "Host API", link: "/plugin/host-api" },
            { text: "API 能力地图", link: "/plugin/api-capability-map" },
            { text: "依赖与服务", link: "/plugin/dependencies-services" },
            { text: "TUI 与事件展示", link: "/plugin/tui" },
            { text: "测试与调试", link: "/plugin/testing" },
          ],
        },
      ],
      "/examples/": [
        {
          text: "示例",
          items: [{ text: "官方 MCP 插件", link: "/examples/mcp" }],
        },
      ],
      "/reference/": [
        {
          text: "API 参考",
          items: [
            { text: "Rust API 总览", link: "/reference/rust-api" },
            { text: "Core 与 Runtime API", link: "/reference/rust-core" },
            { text: "插件 API", link: "/reference/rust-plugin" },
            { text: "WIT API 0.6", link: "/reference/wit" },
          ],
        },
      ],
    },
    search: {
      provider: "local",
      options: {
        translations: {
          button: { buttonText: "搜索文档", buttonAriaLabel: "搜索文档" },
          modal: {
            noResultsText: "没有找到相关内容",
            resetButtonTitle: "清除查询",
            footer: { selectText: "选择", navigateText: "切换", closeText: "关闭" },
          },
        },
      },
    },
    outline: { level: [2, 3], label: "本页" },
    docFooter: { prev: "上一页", next: "下一页" },
    lastUpdated: { text: "最后更新" },
    darkModeSwitchLabel: "外观",
    sidebarMenuLabel: "目录",
    returnToTopLabel: "返回顶部",
    langMenuLabel: "语言",
    footer: {
      message: "Lucia 文档与实现保持同一仓库版本。",
      copyright: "Released under the MIT License.",
    },
  },
});
