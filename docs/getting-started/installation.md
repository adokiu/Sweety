# 编译与安装

## 预编译二进制（推荐）

在 [GitHub Releases](https://github.com/adokiu/Sweety/releases) 下载对应平台的单文件，无需安装运行时。

| 场景 | 推荐文件 |
|------|----------|
| Linux 服务器（通用 x86_64） | `sweety-linux-amd64-musl` |
| Linux ARM64 / 树莓派 4+ | `sweety-linux-aarch64-musl` |
| Linux ARMv7 | `sweety-linux-armv7-musl` |
| macOS Apple Silicon | `sweety-macos-aarch64` |
| macOS Intel | `sweety-macos-amd64` |
| Windows x64 | `sweety-windows-amd64.exe` |
| FreeBSD x86_64 | `sweety-freebsd-amd64` |

完整矩阵与命名规则见仓库 [.github/RELEASE.md](https://github.com/adokiu/Sweety/blob/main/.github/RELEASE.md)。每个 Release 附带 `SHA256SUMS.txt` 校验。

```bash
chmod +x sweety-linux-amd64-musl
./sweety-linux-amd64-musl validate -c /etc/sweety/sweety.toml
```

## 系统要求

| 依赖 | 最低版本 | 说明 |
|------|----------|------|
| Rust | 1.75+ | 推荐使用 `rustup` 管理 |
| OpenSSL / aws-lc-rs | — | TLS 依赖，自动编译 |
| CMake | 3.12+ | aws-lc-rs 编译依赖（Linux/macOS） |

> Windows 需额外安装 [NASM](https://www.nasm.us/) 和 Visual C++ Build Tools。

## 从源码编译

```bash
git clone https://github.com/ChuYao233/Sweety.git
cd Sweety

# 生产构建（开启全部优化）
cargo build --release

# 二进制位于
./target/release/sweety
```

### 功能特性开关（Cargo features）

| Feature | 默认 | 说明 |
|---------|------|------|
| `jemalloc` | 关 | 使用 jemalloc 替换系统分配器（Linux/macOS），高并发场景可提升 ~10% 性能 |

```bash
# 启用 jemalloc
cargo build --release --features jemalloc
```

## 安装到系统

```bash
# 复制到 /usr/local/bin
sudo cp target/release/sweety /usr/local/bin/
sweety --version
```

## systemd 服务

将以下内容保存为 `/etc/systemd/system/sweety.service`：

```ini
[Unit]
Description=Sweety Web Server
After=network.target

[Service]
Type=simple
User=www-data
ExecStart=/usr/local/bin/sweety run -c /etc/sweety/sweety.toml
ExecReload=/usr/local/bin/sweety reload -c /etc/sweety/sweety.toml
Restart=on-failure
LimitNOFILE=65535

[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now sweety
```

## 目录布局建议

```
/etc/sweety/
├── sweety.toml          # 主配置
├── certs/               # 手动证书（或 ACME 自动管理）
│   ├── example.com.crt
│   └── example.com.key
└── acme/                # ACME 自动证书缓存目录
```
