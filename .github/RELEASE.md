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
| `sweety-windows-*.exe` | Windows MSVC |

`arch`：`amd64`、`aarch64`、`armv7`、`i686`、`riscv64`、`ppc64le`、`s390x`、`loong64`。

## 构建矩阵

- **build-primary**：发版必需；全部成功才会创建 Release。
- **build-secondary**：`ppc64le`、`s390x`；`continue-on-error`，失败不阻断发版。

- **32 位 musl**（`i686` / `armv7`）：`--no-default-features`（jemalloc 在 32 位 musl 上链接会缺 `__ffsdi2`）。

未纳入 CI：`loongarch64`、`freebsd-aarch64`、`netbsd` 等（cross / 链接库不完整）。

CI 上 `x86_64-unknown-linux-musl` 使用 `musl-gcc`（见 `.cargo/config.toml`）；本机 Zig 静态编译仍用 `scripts/build-linux-amd64-musl.sh`。

本地与 CI 共用 [.github/actions/build-release](actions/build-release/action.yml)。

## 新增平台

在 `release.yml` 的 `build-primary` 或 `build-secondary` 中增加 `include` 行，并更新本文档与 `docs/getting-started/installation.md` 下载表。
