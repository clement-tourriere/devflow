---
title: Reverse proxy
description: Trusted HTTPS URLs for Docker containers and devflow-managed host processes — automatic discovery, mDNS names that work everywhere, and a local CA.
sidebar:
  order: 6
---

Stop hand-editing `/etc/hosts`, juggling ports, and clicking through certificate warnings. The built-in proxy watches Docker and devflow process state, giving every container and port-backed host process a stable `https://name.local` URL the moment it starts — with a locally-trusted certificate. Webhooks, OAuth callbacks, and cross-service calls just work, per branch.

## Quick start

```bash
devflow proxy start --daemon      # needs root for ports 80/443, or use custom ports
devflow proxy trust install      # trust the local CA once, system-wide

docker run -d --name myapp nginx
curl https://myapp.local          # works — from the host AND from other containers
```

## One suffix everywhere: `.local`

The same `name.local` resolves from the **host** (devflow advertises it over mDNS — Bonjour on macOS, Avahi on Linux; no `/etc/hosts` edits) and from **inside containers** (Docker DNS aliases on a shared `devflow` network).

`.localhost` can't do that: many runtimes (musl, Node, browsers — per RFC 6761) hard-resolve `*.localhost` to loopback without consulting DNS, so inside a container the name points at the container itself. Prefer loopback-only names anyway? `devflow proxy start --domain-suffix localhost`.

On Linux, mDNS publishing needs `avahi-daemon` + `avahi-utils` (preinstalled on most desktops).

## How routing works

The proxy monitors Docker events in real time and polls devflow process records. When a target starts it derives a domain, then:

- **HTTP services** — TLS is terminated with a per-domain certificate signed by the local CA; requests forward to the container. The name resolves to the proxy on `127.0.0.1`. Plain-HTTP requests get a 301 to HTTPS.
- **Host processes, including Pitchfork-managed processes** — devflow-managed native or Pitchfork processes with resolved ports are exposed as `https://<process>.<workspace>.<project>.<suffix>` (for example `.local`) and forward to `127.0.0.1:<port>`.
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
| 4 | devflow process state, native or Pitchfork: `{process}.{workspace}.{project}.{suffix}` | `api.feat-1.myapp.local` |
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

For Pitchfork-backed project processes, keep the proxy config in devflow and set the process runtime in `.devflow.yml`:

```yaml
processes:
  provider: pitchfork
  daemons:
    api:
      run: npm run dev
      port: { expect: [3000], bump: 50 }
```

When `api` starts in workspace `feat-1` for project `myapp`, the devflow proxy publishes the same unified URL shape as native processes:

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

Disable with `--no-auto-network` or `auto_network: false` in global config.

## HTTPS & certificates

First start generates a local CA (`~/.devflow/proxy/ca.crt`, key mode 0600). Per-domain certs (1-year) are minted on demand via SNI and cached in memory.

```bash
devflow proxy trust install    # add CA to the system trust store
devflow proxy trust verify
devflow proxy trust remove
devflow proxy trust info       # manual instructions per platform
```

macOS uses the login keychain; Debian/Ubuntu/Alpine use `/usr/local/share/ca-certificates` + `update-ca-certificates`; Fedora/RHEL use `update-ca-trust`. On Linux, `sudo` (TTY) or `pkexec` is used; otherwise manual instructions are printed.

## Configuration & API

```bash
devflow proxy start [--daemon] [--https-port 443] [--http-port 80] [--api-port 2019]
                    [--domain-suffix local] [--no-mdns] [--no-auto-network]
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
