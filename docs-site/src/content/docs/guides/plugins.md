---
title: Plugin providers
description: Extend devflow with custom service providers via a JSON-over-stdio protocol — any executable, any language.
sidebar:
  order: 11
---

Any executable that speaks a simple JSON-over-stdio protocol can be a devflow service provider — provision DNS records, spin up VMs, call internal platform APIs, whatever your workspaces need.

## Configure

```yaml
services:
  - name: custom-service
    type: local
    service_type: plugin
    auto_workspace: true
    plugin:
      name: my-plugin           # resolved as devflow-plugin-my-plugin on PATH
      # path: /usr/local/bin/my-plugin   # or an explicit path
      timeout: 30               # seconds per invocation
      config:                   # opaque JSON forwarded to every call
        region: us-east-1
        tier: development
```

## Protocol

devflow invokes the executable per operation, writing one JSON request to stdin and reading one JSON response from stdout. Operations mirror the provider trait: create/delete/switch a workspace, fetch connection info, status, start/stop. The `config` block is passed through verbatim so plugins can carry their own settings.

Scaffold a working skeleton instead of memorizing the schema:

```bash
devflow plugin init my-plugin --lang bash      # or --lang python
```

The generated script handles request parsing, dispatch, and response shape — fill in the operation bodies.

## Manage

```bash
devflow plugin list             # configured plugin services + status
devflow plugin check my-plugin  # verify the executable responds correctly
```

`devflow doctor` includes plugin reachability, and plugin services participate in `switch`/`remove`/`connection` like any other provider — including hook template variables (`{{ service['custom-service'].url }}`).

## Protocol version 1

The first request is `negotiate` with `params.supported_versions: [1]`. Return:

```json
{"ok":true,"result":{"protocol_version":1,"operations":{"start":true,"stop":true,"reset":false,"seed":false,"logs":true},"destroy":false}}
```

Only advertise implemented operations. Unspecified capabilities are false; unsupported lifecycle calls return a typed error. Baseline sealing currently belongs to local PostgreSQL, so plugin snapshot claims are not exposed.

Every request carries `protocol_version`, `project_id`, `resource_id`, `operation_id`, `idempotency_key`, and an absolute UTC `deadline`, alongside `method`, `params`, `service_name`, and `config`. Repeated calls within the same recoverable operation carry the same idempotency key for that method and resource. Persist external operation IDs in your backend when a request may complete after a timeout; inspect its state before retrying. Devflow terminates the local plugin process on timeout but cannot cancel an already-submitted remote action.

Structured errors use `{"ok":false,"error":{"code":"busy","message":"resource is leased","retryable":true}}`. Errors retain the devflow operation ID. `logs` returns a string and receives `workspace_name` and optional `tail`; `seed_from_source` receives `workspace_name` and `source`.

Relative executable paths and the process working directory resolve from the project root. Keep stdout for the single JSON response and send logs to stderr. Retention decisions, workspace pins, dependency checks and project leases remain in core; plugins only delete explicitly requested resources.

Existing plugins without negotiation must be updated. Regenerate an adapter with `devflow plugin init` and preserve your operation implementations.
