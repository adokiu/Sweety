---
layout: home

hero:
  name: Sweety
  text: 由 Rust 和 Tokio 驱动
  tagline: ✨兼顾高性能、配置简单、自定义
  actions:
    - theme: brand
      text: 快速开始
      link: /getting-started/introduction
    - theme: alt
      text: 配置参考
      link: /configuration/overview
    - theme: alt
      text: GitHub
      link: https://github.com/ChuYao233/Sweety

features:
  - title: 高性能
    details: 小文件 RPS 比 Nginx 高 +100%，P99 尾延迟低 94%，零错误。SO_REUSEPORT 多核扩展，H2 per-connection writer 消除队头阻塞。

  - title: 自动 HTTPS
    details: 内置 ACME，一行配置开启证书自动申请与续期。支持 HTTP-01 与 DNS-01 通配符证书。

  - title: HTTP/1.1 · H2 · H3
    details: 原生支持 HTTP/3 / QUIC，与 HTTP/2 共享 443 端口，无需重编译。

  - title: 开箱即用
    details: 内置 WordPress / Laravel / 静态站预设，php_fastcgi 与 acme_email 语法糖，8 行跑起一个站点。

  - title: 反向代理 & gRPC
    details: 负载均衡、连接池、断路器、RFC 9111 proxy_cache（cache_lock、use_stale、Cache-Status），gRPC 与 WebSocket 全透传。

  - title: 零停机运维
    details: 热重载不断开连接，Admin REST API，Prometheus /metrics，sweety validate 配置校验。
---

> **注意**：Sweety 目前仍处于积极开发阶段，尚未经过生产环境验证。欢迎在测试/开发环境试用并[反馈问题](https://github.com/ChuYao233/Sweety/issues)。
