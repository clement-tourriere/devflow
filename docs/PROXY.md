# Reverse Proxy

The devflow reverse proxy auto-discovers running Docker containers and serves them over HTTPS on `*.localhost` domains. It handles TLS termination with auto-generated certificates, HTTP-to-HTTPS redirection, and container-to-container DNS resolution via a shared Docker network.

## Quick Start

```sh
# Start the proxy (needs root for ports 80/443, or use custom ports)
devflow proxy start --daemon

# Install the CA certificate so browsers trust *.localhost
devflow proxy trust install

# Start any container — it's automatically proxied
docker run -d --name myapp nginx

# Access it over HTTPS
curl https://myapp.localhost
```

## Domain Resolution

Domains are resolved in priority order. The first match wins.

| Priority | Source | Format | Example |
|----------|--------|--------|---------|
| 1 | `devproxy.domains` label | Comma-separated, fully qualified | `app.localhost, api.localhost` |
| 2 | `devproxy.domain` label | Comma-separated, fully qualified | `myapp.test` |
| 3 | `VIRTUAL_HOST` env var | Comma-separated (nginx-proxy compatible) | `myapp.localhost` |
| 4 | devflow labels | `{service}.{workspace}.{project}.{suffix}` | `postgres.feat-1.myapp.localhost` |
| 5 | Compose labels | `{service}.{project}.{suffix}` | `web.myapp.localhost` |
| 6 | Container name | `{name}.{suffix}` | `myapp.localhost` |

Levels 4-6 are auto-generated from container metadata. Level 4 uses `devflow.service`, `devflow.workspace`, and `devflow.project` labels. Level 5 uses `com.docker.compose.service` and `com.docker.compose.project` labels. Level 6 is the fallback for standalone containers.

All domains are lowercased.

### Examples

**Custom domain via label:**

```yaml
# docker-compose.yml
services:
  web:
    image: nginx
    labels:
      devproxy.domains: "app.localhost, api.localhost"
```

**nginx-proxy compatible:**

```yaml
services:
  web:
    image: nginx
    environment:
      VIRTUAL_HOST: myapp.localhost
```

**Automatic Compose domain (no config needed):**

```sh
# docker-compose.yml with project "myapp" and service "web"
# → automatically available at https://web.myapp.localhost
docker compose -p myapp up -d
```

## Port Detection

The upstream port is resolved in priority order:

| Priority | Source | Example |
|----------|--------|---------|
| 1 | `devproxy.port` label | `devproxy.port=8080` |
| 2 | `DEVPROXY_PORT` env var | `DEVPROXY_PORT=3000` |
| 3 | `VIRTUAL_PORT` env var | `VIRTUAL_PORT=8080` |
| 4 | Container's exposed ports | `EXPOSE 3000` in Dockerfile |
| 5 | Fallback | `80` |

### Example

```yaml
services:
  api:
    build: .
    labels:
      devproxy.port: "3000"
```

## Container Filtering

By default, all running containers are proxied. You can exclude specific containers:

- **Explicit opt-out:** Set the `devproxy.enabled=false` label to exclude a container.
- **Auto-skipped:** Containers named `devproxy*` or `devflow-proxy*` are excluded automatically.

Containers with explicit domain labels (`devproxy.domains`, `devproxy.domain`) or the `VIRTUAL_HOST` env var are always included, even if they match an auto-skip pattern.

Non-running containers are always skipped.

## Container-to-Container DNS

When `auto_network` is enabled (the default), the proxy creates a `devflow` bridge network and connects every discovered container to it with DNS aliases matching their domain names.

**How it works:**

- **Host to container:** `https://web.myapp.localhost` routes through the proxy with TLS termination.
- **Container to container:** `http://web.myapp` resolves via Docker's embedded DNS directly, bypassing the proxy.

Each container gets two aliases: the full domain (e.g. `web.myapp.localhost`) and a suffix-stripped form (e.g. `web.myapp`). The short form exists because glibc resolves `.localhost` to `127.0.0.1` per RFC 6761 before consulting Docker DNS.

**Testing container-to-container resolution:**

```sh
# Start two containers
docker run -d --name web1 nginx
docker run -d --name web2 nginx

# From web2, reach web1 by name
docker exec web2 curl -s http://web1.localhost

# Verify DNS resolution
docker exec web2 nslookup web1.localhost 127.0.0.11

# With Compose services
docker compose -p myapp up -d
docker exec myapp-api-1 curl -s http://web.myapp
```

**Verify network membership:**

```sh
docker network inspect devflow --format '{{range .Containers}}{{.Name}} {{end}}'
```

The `devflow` network persists across proxy restarts. Remove it manually with `docker network rm devflow` if needed.

Disable auto-networking with `--no-auto-network` or set `auto_network: false` in global config.

## HTTPS & Certificates

On first start, the proxy generates a local Certificate Authority:

- **CA certificate:** `~/.devflow/proxy/ca.crt`
- **CA private key:** `~/.devflow/proxy/ca.key` (mode `0600`)

The CA signs short-lived (1 year) per-domain certificates on demand via SNI. Certificates are cached in memory for the lifetime of the proxy process.

HTTP requests to known domains are redirected to HTTPS with a `301 Moved Permanently`.

### Trust Management

```sh
devflow proxy trust install   # Install CA to system trust store
devflow proxy trust verify    # Check if CA is trusted
devflow proxy trust remove    # Remove CA from system trust store
devflow proxy trust info      # Show manual installation instructions
```

**Platform details:**

| Platform | Trust store |
|----------|-------------|
| macOS | Login keychain (`~/Library/Keychains/login.keychain-db`) |
| Debian/Ubuntu | `/usr/local/share/ca-certificates/devflow.crt` + `update-ca-certificates` |
| Fedora/RHEL | `/etc/pki/ca-trust/source/anchors/devflow.crt` + `update-ca-trust` |
| Alpine Linux | `/usr/local/share/ca-certificates/devflow.crt` + `update-ca-certificates` |

On Linux, `sudo` is used when a TTY is available; `pkexec` is tried otherwise. If neither works, manual instructions are printed.

## Configuration

### CLI Flags

```
devflow proxy start [OPTIONS]

Options:
  --daemon              Run as a background daemon
  --https-port <PORT>   HTTPS listen port [default: 443]
  --http-port <PORT>    HTTP listen port [default: 80]
  --api-port <PORT>     API listen port [default: 2019]
  --domain-suffix <S>   Domain suffix for auto-discovered containers [default: localhost]
  --no-auto-network     Disable auto-connecting containers to shared devflow network
```

### Global Config

Stored at `~/.config/devflow/config.yml`:

```yaml
proxy:
  domain_suffix: localhost
  https_port: 443
  http_port: 80
  api_port: 2019
```

### Precedence

CLI flags override global config, which overrides built-in defaults.

### Other Commands

```sh
devflow proxy stop     # Stop the daemon (sends SIGTERM)
devflow proxy status   # Show running state, target count, CA status
devflow proxy list     # List all proxied containers with domains and upstreams
```

All commands support `--json` for machine-readable output.

## API Endpoints

The API server listens on `127.0.0.1:2019` (localhost only).

### `GET /api/status`

Returns proxy running state and summary.

```json
{
  "running": true,
  "targets": 3,
  "https_port": 443,
  "http_port": 80,
  "ca_installed": true
}
```

### `GET /api/targets`

Returns all currently proxied targets.

```json
[
  {
    "domain": "web.myapp.localhost",
    "container_ip": "172.18.0.2",
    "port": 80,
    "container_id": "abc123...",
    "container_name": "myapp-web-1",
    "project": "myapp",
    "service": "web",
    "workspace": null
  }
]
```

### `GET /api/ca`

Returns CA certificate path and trust status.

```json
{
  "cert_path": "/home/user/.devflow/proxy/ca.crt",
  "installed": true,
  "info": "CA certificate: /home/user/.devflow/proxy/ca.crt\n..."
}
```

## Label & Environment Variable Reference

| Name | Type | Purpose |
|------|------|---------|
| `devproxy.domains` | Label | Custom domain(s), comma-separated. Highest priority. |
| `devproxy.domain` | Label | Custom domain(s), comma-separated. Alias for `devproxy.domains`. |
| `devproxy.port` | Label | Override upstream port. |
| `devproxy.enabled` | Label | Set to `false` to exclude a container. |
| `devflow.project` | Label | Project name for auto-generated domains (level 4). |
| `devflow.workspace` | Label | Workspace name for auto-generated domains (level 4). |
| `devflow.service` | Label | Service name for auto-generated domains (level 4). |
| `VIRTUAL_HOST` | Env var | Custom domain(s), comma-separated. nginx-proxy compatible. |
| `VIRTUAL_PORT` | Env var | Override upstream port. nginx-proxy compatible. |
| `DEVPROXY_PORT` | Env var | Override upstream port. |
