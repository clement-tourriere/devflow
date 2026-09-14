---
title: Project processes
description: Run workspace application servers with readiness checks, logs, dependency ordering, and explicit desired state.
sidebar:
  order: 6
---

Services manage data. Processes run project commands in a materialized workspace. The native runtime handles startup, readiness, logs, stopping, port allocation, and dependencies.

```yaml
processes:
  provider: native
  auto_start: true
  auto_stop: true
  start_policy: config
  daemons:
    web:
      run: "npm run dev -- --host 127.0.0.1 --port $PORT"
      port: 3000
      ready_http: "http://127.0.0.1:$PORT"
      ready_timeout: 60
      required: true
    worker:
      run: "npm run worker"
      depends: [web]
```

`start_policy: config` starts configured processes after services and required hooks are ready. `inherit_parent` inherits the parent's selected process set. Service connection variables are available to processes through the workspace environment.

```sh
devflow process start
devflow process status
devflow process logs web
devflow process stop
```

Automatic startup uses the hook approval store. Pre-approve a command with `devflow hook approvals add 'npm run worker'`, or use `DEVFLOW_APPROVE_HOOKS=1` in trusted automation. An unapproved required process leaves setup incomplete. Optional process failures remain visible without blocking readiness.

Ordinary workspace commands work without a permanently running controller. `devflow daemon start` enables background reconciliation of desired process state, `watch` restart-on-change, and configured retries. CLI and desktop inspect the same native runtime records.

The proxy is a separate optional companion. It can expose running processes with ports through stable workspace URLs; it does not supervise their lifecycle.

## Migration from embedded Pitchfork

The embedded Pitchfork dependency and its GUI/configuration editor are retired. Loading `processes.provider: pitchfork` warns and selects `native`; obsolete `processes.pitchfork` settings are omitted when saving normalized configuration. Move any external-only daemon definitions into `processes.daemons` explicitly.

Existing runtime records retain their original identity. Stopping a legacy record uses the external `pitchfork` executable, if installed, to stop that exact recorded daemon before native PID cleanup. Devflow does not discover or stop unrelated external daemons. Restart workspace processes with the native runtime after migration.
