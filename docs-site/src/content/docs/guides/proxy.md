---
title: Reverse proxy
description: HTTPS URLs for Docker containers and devflow-managed host processes, with native discovery and explicit certificate setup.
sidebar:
  order: 7
---

The optional proxy discovers Docker containers and managed host processes, then routes HTTPS requests to them. The default `.localhost` suffix resolves to the host loopback address in browsers, without a DNS helper.

Install the companion by setting `DEVFLOW_INSTALL_PROXY=1` when running the installer. The standard `devflow proxy` command delegates to the installed companion; source builds can also embed proxy support with `--features proxy`.

## Quick start

```bash
devflow proxy start --daemon     # ports 80/443 require permission; custom ports are supported
devflow proxy trust info         # follow the platform's manual CA setup instructions
devflow proxy trust verify

docker run -d --name myapp nginx
curl --resolve myapp.localhost:443:127.0.0.1 https://myapp.localhost
```

## Domain names

Use `.localhost` for HTTP access from the host. It resolves to the calling machine's loopback address, so inside a container it refers to that container. Use Docker service names for container-to-container traffic.

Custom suffixes require DNS configuration. On macOS, `--domain-suffix local` enables native Bonjour registration. Automatic Linux Avahi helper processes have been removed; provision DNS externally when using custom names there. Direct database endpoints under `.localhost` are shown using their upstream IP, since a loopback name cannot identify a container's database. For portable host access, use `devflow service connection` and the published host port.

## How routing works

The proxy monitors Docker events in real time and polls devflow process records. When a target starts it derives a domain, then:

- **HTTP services** — TLS is terminated with a per-domain certificate signed by the local CA; requests forward to the container. The name resolves to the proxy on `127.0.0.1`. Plain-HTTP requests get a 301 to HTTPS.
- **Host processes, managed by devflow** — devflow-managed native processes with resolved ports are exposed as `https://<process>.<workspace>.<project>.<suffix>` (for example `.local`) and forward to `127.0.0.1:<port>`.
- **TCP services (databases)** — well-known ports (PostgreSQL 5432, MySQL 3306, Redis 6379, …) are exposed as **native direct endpoints** like `postgresql://postgres.myapp.local:5432`; the name resolves to the container's own IP.

:::caution
Direct database endpoints need the container IP to be **routable from the host**: native on Linux; on macOS via OrbStack or similar (Docker Desktop's container IPs aren't host-routable). HTTP services work everywhere regardless. If a name doesn't resolve, use the UPSTREAM IP from `devflow proxy list`.
:::

## Domain resolution (first match wins)

| Priority | Source | Example |
| --- | --- | --- |
| 1 | `devproxy.domains` label | `app.local, api.local` |
| 2 | `devproxy.domain` label | `myapp.test` |
| 3 | `VIRTUAL_HOST` env var (nginx-proxy compatible) | `myapp.local` |
| 4 | devflow process state, native: `{process}.{workspace}.{project}.{suffix}` | `api.feat-1.myapp.local` |
| 5 | devflow labels: `{service}.{workspace}.{project}.{suffix}` | `postgres.feat-1.myapp.local` |
| 6 | Compose labels: `{service}.{project}.{suffix}` | `web.myapp.local` |
| 7 | Container name: `{name}.{suffix}` | `myapp.local` |

```yaml
# docker-compose.yml — custom domains
services:
  web:
    image: nginx
    labels:
      devproxy.domains: "app.local, api.local"
```

```bash
# Compose projects need zero config:
docker compose -p myapp up -d     # → https://web.myapp.local
```

For project processes, keep the proxy config in devflow and set the process runtime in `.devflow.yml`:

```yaml
processes:
  provider: native
  daemons:
    api:
      run: npm run dev
      port: { expect: [3000], bump: 50 }
```

When `api` starts in workspace `feat-1` for project `myapp`, the devflow proxy publishes a workspace-specific URL:

```text
https://api.feat-1.myapp.local -> 127.0.0.1:<resolved-port>
```

## Port detection (first match wins)

`devproxy.port` label → `DEVPROXY_PORT` env → `VIRTUAL_PORT` env → exposed ports → 80. Multi-port containers pick deterministically (well-known HTTP ports first, then lowest) — set `devproxy.port` to be explicit.

## Filtering

All running containers are proxied by default. Opt out with the `devproxy.enabled=false` label; `devproxy*`/`devflow-proxy*` containers are skipped automatically. Containers with explicit domain labels are always included.

## Container-to-container DNS

With auto-networking (default), the proxy maintains a `devflow` bridge network and connects every discovered container with two DNS aliases: the full domain (`web.myapp.local`) and a suffix-stripped form (`web.myapp`). Container-to-container traffic resolves via Docker's embedded DNS directly, bypassing the proxy.

```bash
docker exec web2 curl -s http://web1.local                  # same name as the host uses
docker network inspect devflow --format '{{range .Containers}}{{.Name}} {{end}}'
```

For custom suffixes, disable with `--no-auto-network` or `auto_network: false` in global config.

## HTTPS & certificates

First start generates a local CA and stores its private key with mode 0600. Certificates are minted on demand via SNI and cached in memory.

```bash
devflow proxy trust info       # system certificate manager / distribution instructions
devflow proxy trust verify     # compare the actual CA with native trusted roots
```

Install or remove the CA through your operating system's certificate manager. devflow does not run `sudo`, `pkexec`, or `security`, and the desktop displays the same setup instructions. Linux certificate-bundle updates remain part of system setup. Firefox may require a manual CA import.

## Configuration & API

```bash
devflow proxy start [--daemon] [--https-port 443] [--http-port 80] [--api-port 2019]
                    [--domain-suffix localhost] [--no-mdns] [--no-auto-network]
devflow proxy status | list | stop        # all support --json
```

Global defaults live in `~/.config/devflow/config.yml` (`proxy.domain_suffix`, `proxy.https_port`, …); CLI flags override them.

A localhost-only JSON API serves dashboards: `GET /api/status`, `GET /api/targets` (all proxied targets with domain, upstream IP/port, project/service/workspace), `GET /api/ca`. Host process targets use upstream IP `127.0.0.1`.

### Label & env reference

| Name | Type | Purpose |
| --- | --- | --- |
| `devproxy.domains` / `devproxy.domain` | label | custom domain(s), comma-separated — highest priority |
| `devproxy.port` | label | override upstream port |
| `devproxy.enabled=false` | label | exclude container |
| `devflow.project` / `devflow.workspace` / `devflow.service` | label | components for auto-generated devflow domains |
| `VIRTUAL_HOST` / `VIRTUAL_PORT` | env | nginx-proxy-compatible domain/port |
| `DEVPROXY_PORT` | env | override upstream port |

## Optional desktop companion

The desktop connects to one loopback proxy endpoint and invokes `devflow-proxy` for explicit lifecycle commands. Install the companion on `PATH`:

```sh
cargo install --path . --no-default-features --features proxy --bin devflow-proxy
devflow-proxy --json proxy status
```

CLI and desktop use the same global proxy configuration and runtime owner lease. A second proxy process cannot acquire ownership while the first holds it. Existing desktop-only proxy settings are retired; copy customized ports/suffixes into the global devflow proxy configuration. Workspace and database operations work without the companion.
