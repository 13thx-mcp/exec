# Bootstrap and activation

Exec MCP needs one host-side bootstrap because there is no execution tool available to build the first execution tool.

Run once on the workspace host:

```bash
cd /Users/xiivth/workspaces/signs/mcp-server/exec
cargo fmt --all
cargo fmt --all -- --check
cargo check --all-targets --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
cargo build --release --locked
```

`cargo check` creates `Cargo.lock` on the first run, so the final release build can already use `--locked`.

Do **not** edit the gateway config manually after the build if ChatGPT still has the gateway administration actions available. The checked-in/prepared config is already present as:

```text
mcp-server/gateway/servers.d/exec.yaml
```

It is intentionally `enabled: false`. After the release binary exists, enable the `exec` child and reload the gateway.

Expected healthy state:

```text
name: exec
running: true
tool_count: 3
```

Expected tools:

```text
workspace_execute
workspace_detect_project
workspace_quality_check
```

Gateway tool-call timeout is 600000 ms (the gateway maximum). Exec's direct per-process default is 120000 ms and maximum is 300000 ms. A pathological aggregate quality run that exceeds the gateway's 10-minute ceiling will be stopped by the gateway.
