# Activation state

The gateway definition is intentionally installed with `enabled: false` until the release binary exists.

Once the host has built:

```text
/Users/xiivth/workspaces/signs/mcp-server/exec/target/release/rust-mcp-exec
```

set `enabled: true` in `mcp-server/gateway/servers.d/exec.yaml` and reload the gateway. Then verify the child reports `running: true` and `tool_count: 3`.
