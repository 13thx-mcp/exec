# Workspace Exec MCP

`rust-mcp-exec` is a workspace-scoped MCP server for running development commands, collecting build/test/lint diagnostics, and inferring conventional quality checks.

It complements the existing filesystem and Git MCP servers:

```text
ChatGPT
  |
  v
Workspace gateway
  |-- filesystem MCP   read/write project files
  |-- git MCP          repository operations
  `-- exec MCP         build/test/lint/diagnostics
```

## Tools

### `workspace_execute`

Runs one concrete executable with an explicit argument vector.

```json
{
  "program": "pnpm",
  "args": ["run", "test"],
  "cwd": "sagittarius/apps/web",
  "timeout_ms": 120000
}
```

There is no MCP-level shell-string parsing. Direct `sh`, `bash`, `zsh`, other shell interpreters, and privilege escalation commands are rejected as the direct program.

Typical workflows include:

- Next.js / Vue: `pnpm`, `npm`, `yarn`, `bun`, `node`, framework scripts
- Rust: `cargo`, `rustc`, `rustfmt`
- Go: `go`
- Make/build systems: `make`, `cmake`, `ninja`
- Linux/dev diagnostics: concrete CLI tools invoked directly with argv
- hardware/toolchain commands available in host `PATH`, including compilers and simulators

The result includes exit code, stdout/stderr, timeout/truncation state, duration, and a lightweight diagnostic summary.

### `workspace_detect_project`

Read-only project detection based on conventional markers:

- `package.json` plus ancestor lockfiles up to the configured workspace root
- Next.js dependency
- Vue dependency
- `Cargo.toml`
- `go.mod`
- `Makefile`, `makefile`, or `GNUmakefile`

Searching ancestor lockfiles is important for monorepos: for example, an app under `sagittarius/apps/web` can still resolve the repository-level `pnpm-lock.yaml` and use `pnpm`.

For Node projects the tool lists declared package scripts. For Make it discovers simple concrete targets.

### `workspace_quality_check`

Runs inferred quality checks and aggregates issue/error highlights.

`fast` profile:

- Node/Next/Vue: declared lint, typecheck, and test scripts when present
- Rust: `cargo fmt --all -- --check`, `cargo check`, `cargo clippy`, `cargo test`
- Go: `go vet ./...`, `go test ./...`
- Make: declared `lint`, `check`, and `test` targets

`full` additionally runs conventional build commands:

- declared Node `build` script
- Rust release build
- Go build
- Make `build` target

Use `fail_fast=false` for issue discovery when you want independent quality gates to keep running and return multiple failures in one call.

## Workspace and process safety

The server enforces:

- working directories must resolve inside the configured workspace root
- absolute executable paths and `..` traversal are rejected
- explicit program + argv; no MCP-level shell command string
- direct shell/privilege programs are blocked
- bounded arguments, stdin, stdout, and stderr
- configurable process timeout
- restricted inherited environment, retaining common compiler/package-manager/toolchain variables
- `PAGER=cat`, `GIT_PAGER=cat`, and `NO_COLOR=1` for machine-oriented output

### Important trust boundary

This server is **not an OS sandbox**. Development tools can execute project-controlled code. Cargo build scripts, npm/pnpm lifecycle scripts, Make recipes, tests, compiler plugins, generators, Python/Node programs, and similar tools can access resources available to the local user account.

Treat Exec MCP as trusted local development execution. For hostile or untrusted repositories, run the MCP process inside a separate OS/container sandbox.

## Build and activate

See [`BUILD.md`](BUILD.md). The gateway definition is prepared at:

```text
mcp-server/gateway/servers.d/exec.yaml
```

It starts with `enabled: false` because the execution MCP cannot compile itself before an execution capability exists. After the first host-side release build, enable it and reload the gateway.

Expected gateway tool surface:

```text
workspace_execute
workspace_detect_project
workspace_quality_check
```
