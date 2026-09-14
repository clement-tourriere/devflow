# Pitchfork migration

The embedded Pitchfork runtime and its GUI/web configuration bridge are retired. Use `processes.provider: native` and explicit `processes.daemons` definitions. Old Pitchfork configuration is accepted as migration input, produces a warning, and is omitted from normalized output.

Existing process records retain their identities. The legacy stop adapter calls an installed `pitchfork` CLI for the exact recorded daemon and then cleans up the recorded native PID. It does not enumerate or stop unrelated daemons.

See [the architecture and compatibility guide](architecture.md) and [project processes](../docs-site/src/content/docs/guides/processes.md).
