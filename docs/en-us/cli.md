# CLI Reference

## Syntax

```
sweety [OPTIONS] [COMMAND]
```

## Global Options

| Option | Short | Default | Description |
|--------|-------|---------|-------------|
| `--config <PATH>` | `-c` | `config/sweety.toml` | Config file path |
| `--pid-file <PATH>` | — | `/var/run/sweety.pid` | PID file path (daemon mode) |
| `--version` | `-v` | — | Show version |

Environment variable `SWEETY_CONFIG` can replace `-c`:

```bash
SWEETY_CONFIG=/etc/sweety/sweety.toml sweety run
```

---

## Subcommands

### `run` — Foreground

Run in foreground, recommended under systemd / supervisord:

```bash
sweety run
sweety run -c /etc/sweety/sweety.toml
```

- Logs output to stdout/stderr
- Graceful shutdown on `SIGTERM` / `SIGINT` (waits for existing connections)
- Default subcommand when omitted

---

### `start` — Background (Daemon)

Start in background, writes PID file:

```bash
sweety start
sweety start -c /etc/sweety/sweety.toml --pid-file /var/run/sweety.pid
```

---

### `stop` — Stop Background Process

Reads PID file, sends `SIGTERM`:

```bash
sweety stop
sweety stop --pid-file /var/run/sweety.pid
```

---

### `restart` — Restart

Equivalent to `stop` + `start`:

```bash
sweety restart
sweety restart -c /etc/sweety/sweety.toml
```

---

### `reload` — Hot Reload Config

Reload config without dropping existing connections:

```bash
sweety reload
sweety reload -c /etc/sweety/sweety.toml
```

**Prerequisite**: `global.admin_listen` must be configured — reload sends a signal via the Admin API.

```toml
[global]
admin_listen = "127.0.0.1:9099"
```

Hot reload scope:
- ✅ Site config (server_name, root, locations, etc.)
- ✅ Rate limiting, cache config
- ✅ Upstream node list
- ⚠️ Listen port changes require restart

---

### `validate` — Validate Config

Equivalent to `nginx -t`, checks config syntax and TLS certificates:

```bash
sweety validate
sweety validate -c /etc/sweety/sweety.toml
```

Checks:
- TOML/JSON/YAML syntax validity
- Required fields (`name`, `server_name`)
- TLS certificate and key file paths are readable
- Upstream node format correctness
- Port conflict detection

Success output:

```
Configuration check passed ✓
  Sites: 3
  Listen ports: 80, 443
  TLS sites: 2 (ACME: 1, Manual: 1)
```

---

### `version` — Show Version

```bash
sweety version
# or
sweety -v
```

---

### `api-doc` — Output Admin API Docs

Output Admin REST API documentation (OpenAPI format):

```bash
sweety api-doc
sweety api-doc > api-doc.json
```

---

## Admin API

After configuring `admin_listen`, Sweety provides a REST API:

```toml
[global]
admin_listen = "127.0.0.1:9099"
admin_token  = "your-secret-token"
```

### Authentication

Endpoints requiring auth must include a Bearer Token:

```bash
curl -H "Authorization: Bearer your-secret-token" http://127.0.0.1:9099/api/v1/stats
```

### Endpoints

| Method | Path | Auth | Status | Description |
|--------|------|------|--------|-------------|
| `GET` | `/api/v1/health` | No | ✅ | Health check |
| `GET` | `/api/v1/version` | No | ✅ | Version info |
| `GET` | `/api/v1/stats` | Yes | ✅ | Global request statistics snapshot |
| `GET` | `/api/v1/plugins` | Yes | ✅ | Registered plugin list |
| `GET` | `/api/v1/doc` | No | ✅ | API documentation (JSON) |
| `GET` | `/api/v1/sites` | Yes | ⚠️ Stub | Site list (full in v0.5) |
| `POST` | `/api/v1/reload` | Yes | 🚧 Planned | Hot reload (v0.5) |
| `GET` | `/api/v1/upstreams` | Yes | 🚧 Planned | Upstream nodes & circuit breaker status (v0.5) |
| `POST` | `/api/v1/upstreams/:name/nodes/:addr/enable` | Yes | 🚧 Planned | Enable node (v0.5) |
| `POST` | `/api/v1/upstreams/:name/nodes/:addr/disable` | Yes | 🚧 Planned | Disable node (v0.5) |
| — | WebSocket `/api/v1/stats/stream` | — | 🚧 Planned | Real-time stats push (v0.5) |
| `GET` | `/metrics` | No | ✅ | Prometheus metrics (text/plain) |

> Full Admin API (config tree, `/config/reload`, upstreams/certs/cache, etc.) is documented in [Admin API](/en-us/configuration/admin-api). CLI `sweety reload` via signals still works alongside Admin endpoints.

---

## Common Operations

```bash
# Install as systemd service (foreground mode, recommended)
sudo systemctl start sweety

# Reload config (no service interruption)
sweety reload -c /etc/sweety/sweety.toml

# Test config then restart
sweety validate -c /etc/sweety/sweety.toml && sweety restart -c /etc/sweety/sweety.toml
```
