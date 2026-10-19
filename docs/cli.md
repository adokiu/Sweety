# 命令行参考

## 语法

```
sweety [OPTIONS] [COMMAND]
```

## 全局选项

| 选项 | 短名 | 默认值 | 说明 |
|------|------|--------|------|
| `--config <PATH>` | `-c` | `config/sweety.toml` | 配置文件路径 |
| `--pid-file <PATH>` | — | `/var/run/sweety.pid` | PID 文件路径（daemon 模式用） |
| `--version` | `-v` | — | 显示版本号 |

环境变量 `SWEETY_CONFIG` 可替代 `-c`：

```bash
SWEETY_CONFIG=/etc/sweety/sweety.toml sweety run
```

---

## 子命令

### `run` — 前台运行

前台运行，推荐在 systemd / supervisord 管理下使用：

```bash
sweety run
sweety run -c /etc/sweety/sweety.toml
```

- 日志直接输出到 stdout/stderr
- 收到 `SIGTERM` / `SIGINT` 后优雅退出（等待现有连接处理完成）
- 省略子命令时默认执行 `run`

---

### `start` — 后台启动（daemon）

后台启动，写入 PID 文件：

```bash
sweety start
sweety start -c /etc/sweety/sweety.toml --pid-file /var/run/sweety.pid
```

---

### `stop` — 停止后台进程

读取 PID 文件，向进程发送 `SIGTERM`：

```bash
sweety stop
sweety stop --pid-file /var/run/sweety.pid
```

---

### `restart` — 重启

等价 `stop` + `start`：

```bash
sweety restart
sweety restart -c /etc/sweety/sweety.toml
```

---

### `reload` — 热重载配置

不断开现有连接，重新加载配置文件：

```bash
sweety reload
sweety reload -c /etc/sweety/sweety.toml
```

**前提**：`global.admin_listen` 必须已配置，reload 命令通过 Admin API 发送重载信号。

```toml
[global]
admin_listen = "127.0.0.1:9099"
```

热重载范围：
- ✅ 站点配置（server_name、root、locations 等）
- ✅ 限流、缓存配置
- ✅ 上游节点列表
- ⚠️ 监听端口变更需要重启

---

### `validate` — 验证配置

等价 `nginx -t`，校验配置文件语法和 TLS 证书：

```bash
sweety validate
sweety validate -c /etc/sweety/sweety.toml
```

检查内容：
- TOML/JSON/YAML 语法合法性
- 必填字段（`name`、`server_name`）
- TLS 证书和私钥文件路径可读
- 上游节点格式正确
- 端口冲突检测

成功输出：

```
配置文件检查通过 ✓
  站点数: 3
  监听端口: 80, 443
  TLS 站点: 2 (ACME: 1, 手动: 1)
```

---

### `version` — 显示版本

```bash
sweety version
# 或
sweety -v
```

---

### `api-doc` — 输出 Admin API 文档

输出 Admin REST API 的 JSON 文档（OpenAPI 格式）：

```bash
sweety api-doc
sweety api-doc > api-doc.json
```

---

## Admin API

配置 `admin_listen` 后，Sweety 提供 REST API：

```toml
[global]
admin_listen = "127.0.0.1:9099"
admin_token  = "your-secret-token"
```

### 鉴权

需要鉴权的端点必须携带 Bearer Token：

```bash
curl -H "Authorization: Bearer your-secret-token" http://127.0.0.1:9099/api/v1/stats
```

### 端点列表

| 方法 | 路径 | 鉴权 | 状态 | 说明 |
|------|------|------|------|------|
| `GET` | `/api/v1/health` | 否 | ✅ | 健康检查 |
| `GET` | `/api/v1/version` | 否 | ✅ | 版本信息 |
| `GET` | `/api/v1/stats` | 是 | ✅ | 全局请求统计快照 |
| `GET` | `/api/v1/plugins` | 是 | ✅ | 已注册插件列表 |
| `GET` | `/api/v1/doc` | 否 | ✅ | API 文档（JSON） |
| `GET` | `/api/v1/sites` | 是 | ⚠️ 桩 | 站点列表（v0.5 完善） |
| `POST` | `/api/v1/reload` | 是 | 🚧 计划 | 热重载（v0.5） |
| `GET` | `/api/v1/upstreams` | 是 | 🚧 计划 | 上游节点及断路器状态（v0.5） |
| `POST` | `/api/v1/upstreams/:name/nodes/:addr/enable` | 是 | 🚧 计划 | 启用节点（v0.5） |
| `POST` | `/api/v1/upstreams/:name/nodes/:addr/disable` | 是 | 🚧 计划 | 禁用节点（v0.5） |
| — | WebSocket `/api/v1/stats/stream` | — | 🚧 计划 | 实时统计推送（v0.5） |
| `GET` | `/metrics` | 否 | ✅ | Prometheus 指标（text/plain） |

> 完整 Admin API（配置树、`/config/reload`、上游/证书/缓存等）见 [管理 API](/configuration/admin-api)。CLI `sweety reload` 仍可通过系统信号热重载，与 Admin 端点并存。

---

## 常用操作

```bash
# 安装为 systemd 服务（前台模式，推荐）
sudo systemctl start sweety

# 重载配置（不中断服务）
sweety reload -c /etc/sweety/sweety.toml

# 手动测试配置后重启
sweety validate -c /etc/sweety/sweety.toml && sweety restart -c /etc/sweety/sweety.toml
```
