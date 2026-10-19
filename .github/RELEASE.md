# Release / CI 说明

## 工作流

| 工作流 | 触发 | 作用 |
|--------|------|------|
| [ci.yml](workflows/ci.yml) | `main` push / PR | 冒烟：`linux-amd64-musl`、`macos-aarch64`、`windows-amd64` |
| [release.yml](workflows/release.yml) | tag `pre-*` / `v*` | 全平台构建 + GitHub Release + `SHA256SUMS.txt` |

## 发版步骤

1. 更新根目录 [CHANGELOG.md](../CHANGELOG.md) 对应 `## [x.y.z]` 节。
2. 修改 [Cargo.toml](../Cargo.toml) 的 `version`。
3. 合并到 `main`。
4. 打 tag 并推送：
   ```bash
   git tag pre-0.2.9   # 或 v0.2.9
   git push origin pre-0.2.9
   ```
5. Actions 完成后在 [Releases](https://github.com/adokiu/Sweety/releases) 检查 Assets 与更新日志。

## 产物命名

```
sweety-{os}-{arch}[-{libc}][.exe]
```

| 前缀 | 含义 |
|------|------|
| `sweety-linux-*-musl` | Linux 静态链接（推荐） |
| `sweety-linux-*-gnu` | Linux glibc |
| `sweety-macos-aarch64` / `sweety-macos-amd64` | macOS |
| `sweety-freebsd-*` | FreeBSD |
| `sweety-netbsd-amd64` | NetBSD（secondary，可能缺失） |
| `sweety-windows-*.exe` | Windows MSVC |

`arch`：`amd64`、`aarch64`、`armv7`、`i686`、`riscv64`、`ppc64le`、`s390x`、`loong64`。

## 构建矩阵

- **build-primary**：发版必需；全部成功才会创建 Release。
- **build-secondary**：`ppc64le`、`s390x`、`netbsd`；`continue-on-error`，失败不阻断发版。

未纳入 CI 的平台（需自备工具链或 cross 镜像）：`loongarch64`、`freebsd-aarch64` 等。

本地与 CI 共用 [.github/actions/build-release](actions/build-release/action.yml)。

## 新增平台

在 `release.yml` 的 `build-primary` 或 `build-secondary` 中增加 `include` 行，并更新本文档与 `docs/getting-started/installation.md` 下载表。
