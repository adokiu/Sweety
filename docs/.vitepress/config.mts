import { defineConfig, type DefaultTheme } from 'vitepress'

// ─── 中文侧边栏 ─────────────────────────────────────────────────
function sidebarZh(): DefaultTheme.SidebarItem[] {
  return [
    {
      text: '入门',
      items: [
        { text: '简介与特性', link: '/getting-started/introduction' },
        { text: '编译与安装', link: '/getting-started/installation' },
        { text: '快速开始', link: '/getting-started/quickstart' },
        { text: '常见问题', link: '/getting-started/faq' },
      ],
    },
    {
      text: '配置',
      items: [
        { text: '概述', link: '/configuration/overview' },
        { text: '全局配置 [global]', link: '/configuration/global' },
        { text: '站点配置 [[sites]]', link: '/configuration/sites' },
        { text: 'TLS / ACME', link: '/configuration/tls' },
        { text: 'HTTP/3 调优', link: '/configuration/http3' },
        { text: '路由规则 locations', link: '/configuration/locations' },
        { text: 'FastCGI / PHP', link: '/configuration/fastcgi' },
        { text: '反向代理', link: '/configuration/reverse-proxy' },
        { text: 'gRPC 代理', link: '/configuration/grpc' },
        { text: 'WebSocket', link: '/configuration/websocket' },
        { text: '缓存', link: '/configuration/cache' },
        { text: '速率限制', link: '/configuration/rate-limit' },
        { text: '内置预设', link: '/configuration/presets' },
        { text: '管理 API', link: '/configuration/admin-api' },
      ],
    },
    {
      text: '示例',
      items: [
        { text: 'WordPress / Laravel', link: '/examples/wordpress' },
        { text: '静态站点 / SPA', link: '/examples/static-site' },
        { text: '反向代理 / gRPC', link: '/examples/reverse-proxy' },
      ],
    },
    {
      text: '更多',
      items: [
        { text: '命令行参考', link: '/cli' },
        { text: '性能测试', link: '/benchmark' },
        { text: 'Roadmap', link: '/roadmap' },
        { text: 'RFC / 缓存合规', link: '/rfc-compliance-gap-analysis' },
        { text: '热重载', link: '/advanced/hot-reload' },
      ],
    },
  ]
}

// ─── 英文侧边栏 ─────────────────────────────────────────────────
function sidebarEn(): DefaultTheme.SidebarItem[] {
  return [
    {
      text: 'Getting Started',
      items: [
        { text: 'Introduction', link: '/en-us/getting-started/introduction' },
        { text: 'Installation', link: '/en-us/getting-started/installation' },
        { text: 'Quick Start', link: '/en-us/getting-started/quickstart' },
        { text: 'FAQ', link: '/en-us/getting-started/faq' },
      ],
    },
    {
      text: 'Configuration',
      items: [
        { text: 'Overview', link: '/en-us/configuration/overview' },
        { text: 'Global [global]', link: '/en-us/configuration/global' },
        { text: 'Sites [[sites]]', link: '/en-us/configuration/sites' },
        { text: 'TLS / ACME', link: '/en-us/configuration/tls' },
        { text: 'HTTP/3 Tuning', link: '/en-us/configuration/http3' },
        { text: 'Locations', link: '/en-us/configuration/locations' },
        { text: 'FastCGI / PHP', link: '/en-us/configuration/fastcgi' },
        { text: 'Reverse Proxy', link: '/en-us/configuration/reverse-proxy' },
        { text: 'gRPC Proxy', link: '/en-us/configuration/grpc' },
        { text: 'WebSocket', link: '/en-us/configuration/websocket' },
        { text: 'Cache', link: '/en-us/configuration/cache' },
        { text: 'Rate Limiting', link: '/en-us/configuration/rate-limit' },
        { text: 'Presets', link: '/en-us/configuration/presets' },
        { text: 'Admin API', link: '/en-us/configuration/admin-api' },
      ],
    },
    {
      text: 'Examples',
      items: [
        { text: 'WordPress / Laravel', link: '/en-us/examples/wordpress' },
        { text: 'Static Site / SPA', link: '/en-us/examples/static-site' },
        { text: 'Reverse Proxy / gRPC', link: '/en-us/examples/reverse-proxy' },
      ],
    },
    {
      text: 'More',
      items: [
        { text: 'CLI Reference', link: '/en-us/cli' },
        { text: 'Benchmark', link: '/en-us/benchmark' },
        { text: 'Roadmap', link: '/en-us/roadmap' },
        { text: 'RFC / cache compliance', link: '/en-us/rfc-compliance-gap-analysis' },
        { text: 'Hot Reload', link: '/en-us/advanced/hot-reload' },
      ],
    },
  ]
}

export default defineConfig({
  title: 'Sweety',

  head: [
    ['link', { rel: 'icon', type: 'image/svg+xml', href: '/logo.svg' }],
  ],

  // ─── 多语言配置 ────────────────────────────────────────────────
  locales: {
    root: {
      label: '简体中文',
      lang: 'zh-CN',
      description: '高性能多站点 Web 服务器 —— 基于 Rust，兼顾 Nginx 深度配置与 Caddy 开箱即用体验',
      themeConfig: {
        sidebar: sidebarZh(),
        editLink: {
          pattern: 'https://github.com/ChuYao233/Sweety/edit/main/docs/:path',
          text: '在 GitHub 上编辑此页',
        },
        outline: { label: '本页目录', level: [2, 3] },
        docFooter: { prev: '上一篇', next: '下一篇' },
        lastUpdated: { text: '最后更新' },
        returnToTopLabel: '回到顶部',
        sidebarMenuLabel: '目录',
        darkModeSwitchLabel: '深色模式',
        footer: { message: '基于 Apache License 2.0 发布' },
      },
    },
    'en-us': {
      label: 'English',
      lang: 'en-US',
      link: '/en-us/',
      description: 'High-performance, single-binary, multi-site web server powered by Rust',
      themeConfig: {
        sidebar: sidebarEn(),
        editLink: {
          pattern: 'https://github.com/ChuYao233/Sweety/edit/main/docs/:path',
          text: 'Edit this page on GitHub',
        },
        outline: { label: 'On this page', level: [2, 3] },
        docFooter: { prev: 'Previous', next: 'Next' },
        lastUpdated: { text: 'Last updated' },
        returnToTopLabel: 'Back to top',
        sidebarMenuLabel: 'Menu',
        darkModeSwitchLabel: 'Dark mode',
        footer: { message: 'Released under the Apache License 2.0' },
      },
    },
    // 添加更多语言示例：
    // 'ja': { label: '日本語', lang: 'ja', link: '/ja/', ... },
  },

  themeConfig: {
    logo: '/logo.svg',
    siteTitle: 'Sweety',
    socialLinks: [
      { icon: 'github', link: 'https://github.com/ChuYao233/Sweety' },
    ],
    search: { provider: 'local' },
  },
})
