---
layout: home

hero:
  name: Sweety
  text: Powered by Rust & Tokio
  tagline: ✨ High performance, simple configuration, fully customizable
  actions:
    - theme: brand
      text: Get Started
      link: /en-us/getting-started/introduction
    - theme: alt
      text: Configuration
      link: /en-us/configuration/overview
    - theme: alt
      text: GitHub
      link: https://github.com/ChuYao233/Sweety

features:
  - title: High Performance
    details: Small-file RPS 100%+ higher than Nginx, P99 tail latency 94% lower, zero errors. SO_REUSEPORT multi-core scaling, H2 per-connection writer eliminates head-of-line blocking.

  - title: Automatic HTTPS
    details: Built-in ACME — one line to enable automatic certificate issuance and renewal. Supports HTTP-01 and DNS-01 wildcard certificates.

  - title: HTTP/1.1 · H2 · H3
    details: Native HTTP/3 / QUIC support, shares port 443 with HTTP/2, no recompilation needed.

  - title: Out of the Box
    details: Built-in WordPress / Laravel / static site presets, php_fastcgi and acme_email sugar syntax — 8 lines to run a site.

  - title: Reverse Proxy & gRPC
    details: Load balancing, connection pooling, circuit breaker, RFC 9111 proxy_cache (cache_lock, use_stale, Cache-Status), transparent gRPC and WebSocket.

  - title: Zero-Downtime Ops
    details: Hot reload without dropping connections, Admin REST API, Prometheus /metrics, sweety validate config check.
---

> **Note**: Sweety is under active development and has not yet been validated in production. Feedback from testing/staging environments is welcome — [open an issue](https://github.com/ChuYao233/Sweety/issues).
