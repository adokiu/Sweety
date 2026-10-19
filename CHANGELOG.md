# Changelog

本文件记录每个预发布/正式版本的**面向用户的变更**。格式基于 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)。

发布流程：更新本文件对应版本节 → 修改 `Cargo.toml` 的 `version` → 推送 `main` → 打 tag（`pre-x.y.z` 或 `v x.y.z`）触发 [Release 工作流](.github/workflows/release.yml)，Release 说明会自动嵌入本节内容。

## [0.2.8] - 2026-10-19

### Added

- **共享缓存 RFC 9110 / 9111 / 5861 / 9211**：`proxy_cache` 与 `fastcgi_cache` 共用逻辑；`origin_revalidate`、条件回源 304、Age、Warning 110、only-if-cached→504、`Cache-Status`（RFC 9211）。
- **Nginx 对齐运维项**：`cache_lock`、`min_uses`、`status_ttl`、`use_stale[]`、`cache_key_headers`、`cache_require_buffering`、`background_update`（HTTP/1 上游 SWR 后台条件 GET）。
- **HTTP 语义**：HEAD 缓存 HIT 正确 `Content-Length`；405 响应带 `Allow`；错误响应注入 `Date`。
- **文档**：VitePress 缓存/RFC 专页、Roadmap/README 同步；[RFC 合规差距说明](docs/rfc-compliance-gap-analysis.md)。

### Changed

- 反向代理 / WebSocket / H1·H2·H3 栈多项 RFC 与稳定性修复（Via、H3 GOAWAY/0-RTT、Accept-Encoding 透传等）。
- 静态 `open_file_cache` 可配置、`min_uses` 防污染；macOS sendfile 快路径。

### Known limits

- H2 上游尚未接入 `background_update` 后台刷新。
- `static_multipart_byteranges` 仅为配置占位。

### CI / Release

- Release 矩阵扩展：macOS（arm64/amd64）、Windows arm64、Linux armv7/i686 musl、FreeBSD arm64 等；`ci.yml` 对 main/PR 冒烟构建。
- 产物命名与发版流程见 `.github/RELEASE.md`。

## [0.2.7] - 2026-05-28

历史预发布（`pre-0.2.7` 及更早）的 Release 说明仅含 GitHub 自动生成的 compare 链接，自 **0.2.8 / pre-0.2.8** 起在 Release 正文中附带本 Changelog 对应章节。

详见 [Releases](https://github.com/adokiu/Sweety/releases)。
