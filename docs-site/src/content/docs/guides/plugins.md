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
