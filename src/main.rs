use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Component, Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use clap::Parser;
use rmcp::{
    ErrorData as McpError, ServiceExt,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock},
    schemars, tool, tool_router,
    transport::stdio,
};
use serde::{Deserialize, Serialize};
use tokio::{io::AsyncWriteExt, process::Command, time::timeout};

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const DEFAULT_MAX_TIMEOUT_MS: u64 = 300_000;
const DEFAULT_MAX_OUTPUT_BYTES: usize = 2 * 1024 * 1024;
const MAX_ARGS: usize = 1024;
const MAX_ARG_BYTES: usize = 64 * 1024;
const MAX_STDIN_BYTES: usize = 1024 * 1024;
const MAX_DIAGNOSTIC_HIGHLIGHTS: usize = 80;

const BASE_ENV_ALLOWLIST: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "TMPDIR",
    "TEMP",
    "TMP",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TERM",
    "COLORTERM",
    "CI",
    "CARGO_HOME",
    "RUSTUP_HOME",
    "RUSTC_WRAPPER",
    "RUSTFLAGS",
    "RUSTDOCFLAGS",
    "SCCACHE_DIR",
    "GOPATH",
    "GOROOT",
    "GOMODCACHE",
    "GOCACHE",
    "GOENV",
    "GOFLAGS",
    "PNPM_HOME",
    "NPM_CONFIG_USERCONFIG",
    "NVM_DIR",
    "VOLTA_HOME",
    "COREPACK_HOME",
    "BUN_INSTALL",
    "CC",
    "CXX",
    "AR",
    "CFLAGS",
    "CXXFLAGS",
    "LDFLAGS",
    "PKG_CONFIG_PATH",
    "CPATH",
    "LIBRARY_PATH",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "no_proxy",
    "SSH_AUTH_SOCK",
];

const BLOCKED_DIRECT_PROGRAMS: &[&str] = &[
    "sh",
    "bash",
    "zsh",
    "fish",
    "dash",
    "ksh",
    "csh",
    "tcsh",
    "powershell",
    "pwsh",
    "cmd",
    "sudo",
    "su",
    "doas",
    "env",
];

#[derive(Debug, Parser)]
#[command(
    version,
    about = "Workspace-scoped execution MCP server for development diagnostics and quality checks"
)]
struct Cli {
    #[arg(long, env = "MCP_EXEC_ROOT")]
    root: PathBuf,

    #[arg(
        long,
        env = "MCP_EXEC_DEFAULT_TIMEOUT_MS",
        default_value_t = DEFAULT_TIMEOUT_MS
    )]
    default_timeout_ms: u64,

    #[arg(
        long,
        env = "MCP_EXEC_MAX_TIMEOUT_MS",
        default_value_t = DEFAULT_MAX_TIMEOUT_MS
    )]
    max_timeout_ms: u64,

    #[arg(
        long,
        env = "MCP_EXEC_MAX_OUTPUT_BYTES",
        default_value_t = DEFAULT_MAX_OUTPUT_BYTES
    )]
    max_output_bytes: usize,
}

#[derive(Debug, Clone)]
struct ExecServer {
    workspace_root: PathBuf,
    default_timeout_ms: u64,
    max_timeout_ms: u64,
    max_output_bytes: usize,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ExecuteArgs {
    /// Executable name resolved through PATH, or a workspace-relative executable path. Direct shells and privilege escalation programs are rejected.
    program: String,
    /// Argument vector passed directly to the executable. No MCP-level shell parsing or expansion is performed.
    #[serde(default)]
    args: Vec<String>,
    /// Working directory relative to the configured workspace root. Defaults to the workspace root.
    cwd: Option<String>,
    /// Timeout for this process. Values above the server maximum are rejected.
    timeout_ms: Option<u64>,
    /// Optional UTF-8 stdin delivered to the child process.
    stdin: Option<String>,
    /// Additional environment variables. The inherited environment is otherwise restricted to a toolchain allowlist.
    #[serde(default)]
    env: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ProjectArgs {
    /// Project directory relative to the configured workspace root.
    cwd: String,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, schemars::JsonSchema, Serialize)]
#[serde(rename_all = "snake_case")]
enum QualityProfile {
    /// Prefer issue-finding checks and omit production builds.
    #[default]
    Fast,
    /// Run inferred lint/type/test/check commands plus conventional production builds.
    Full,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct QualityArgs {
    /// Project directory relative to the configured workspace root.
    cwd: String,
    /// Fast omits production builds; full includes them when a conventional command can be inferred.
    #[serde(default)]
    profile: QualityProfile,
    /// Stop after the first failed check instead of collecting additional diagnostics.
    #[serde(default)]
    fail_fast: bool,
    /// Optional timeout applied independently to each inferred command.
    timeout_ms: Option<u64>,
}

#[derive(Debug, Serialize)]
struct ExecutionOutput {
    program: String,
    args: Vec<String>,
    cwd: String,
    exit_code: Option<i32>,
    success: bool,
    timed_out: bool,
    duration_ms: u128,
    stdout: String,
    stderr: String,
    stdout_truncated: bool,
    stderr_truncated: bool,
    diagnostics: DiagnosticSummary,
}

#[derive(Debug, Default, Serialize)]
struct DiagnosticSummary {
    error_lines: usize,
    warning_lines: usize,
    failure_lines: usize,
    highlights: Vec<String>,
}

#[derive(Debug, Serialize)]
struct ProjectDetection {
    cwd: String,
    kinds: Vec<String>,
    package_manager: Option<String>,
    node_scripts: Vec<String>,
    make_targets: Vec<String>,
    markers: Vec<String>,
    recommended_checks: Vec<CommandSpec>,
}

#[derive(Debug, Clone, Serialize)]
struct CommandSpec {
    label: String,
    program: String,
    args: Vec<String>,
}

#[derive(Debug, Serialize)]
struct QualityOutput {
    cwd: String,
    profile: QualityProfile,
    detected_kinds: Vec<String>,
    passed: bool,
    checks_run: usize,
    checks_failed: usize,
    checks: Vec<QualityCheckResult>,
    diagnostics: DiagnosticSummary,
}

#[derive(Debug, Serialize)]
struct QualityCheckResult {
    label: String,
    command: String,
    result: ExecutionOutput,
}

#[derive(Debug)]
struct NodeManifestInfo {
    scripts: Vec<String>,
    has_next: bool,
    has_vue: bool,
}

impl ExecServer {
    fn new(cli: &Cli) -> Result<Self> {
        let workspace_root = std::fs::canonicalize(&cli.root)
            .with_context(|| format!("cannot resolve workspace root: {}", cli.root.display()))?;
        anyhow::ensure!(workspace_root.is_dir(), "workspace root is not a directory");
        anyhow::ensure!(
            cli.max_timeout_ms > 0,
            "max timeout must be greater than zero"
        );
        anyhow::ensure!(
            cli.default_timeout_ms > 0 && cli.default_timeout_ms <= cli.max_timeout_ms,
            "default timeout must be within 1..=max timeout"
        );

        Ok(Self {
            workspace_root,
            default_timeout_ms: cli.default_timeout_ms,
            max_timeout_ms: cli.max_timeout_ms,
            max_output_bytes: cli.max_output_bytes.max(1),
        })
    }

    fn success_json<T: Serialize>(&self, value: &T) -> CallToolResult {
        let text = serde_json::to_string_pretty(value).unwrap_or_else(|_| "{}".to_owned());
        CallToolResult::success(vec![ContentBlock::text(text)])
    }

    fn failure(message: impl Into<String>) -> CallToolResult {
        CallToolResult::error(vec![ContentBlock::text(message.into())])
    }

    fn resolve_cwd(&self, cwd: Option<&str>) -> std::result::Result<PathBuf, String> {
        let raw = cwd.unwrap_or(".");
        let relative = clean_relative_dir(raw)?;
        let requested = self.workspace_root.join(relative);
        let resolved = std::fs::canonicalize(&requested)
            .map_err(|error| format!("cannot resolve working directory `{raw}`: {error}"))?;

        if !resolved.is_dir() {
            return Err(format!("working directory is not a directory: `{raw}`"));
        }
        if !resolved.starts_with(&self.workspace_root) {
            return Err(format!(
                "working directory resolves outside the configured workspace: `{raw}`"
            ));
        }
        Ok(resolved)
    }

    fn relative_display(&self, path: &Path) -> String {
        path.strip_prefix(&self.workspace_root)
            .ok()
            .filter(|relative| !relative.as_os_str().is_empty())
            .map_or_else(
                || ".".to_owned(),
                |relative| relative.to_string_lossy().into_owned(),
            )
    }

    fn validated_timeout(&self, requested: Option<u64>) -> std::result::Result<u64, String> {
        let value = requested.unwrap_or(self.default_timeout_ms);
        if value == 0 || value > self.max_timeout_ms {
            return Err(format!(
                "timeout_ms must be within 1..={} (requested {value})",
                self.max_timeout_ms
            ));
        }
        Ok(value)
    }

    fn validate_program(&self, program: &str, cwd: &Path) -> std::result::Result<String, String> {
        validate_nonempty_text("program", program, 4096)?;
        if program.starts_with('-') {
            return Err("program must not start with '-'".to_owned());
        }

        let leaf = Path::new(program)
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or_else(|| "program must be valid UTF-8".to_owned())?;
        if BLOCKED_DIRECT_PROGRAMS.contains(&leaf) {
            return Err(format!(
                "direct shell/privilege program `{leaf}` is blocked; invoke a concrete development tool with an argument vector instead"
            ));
        }

        let path = Path::new(program);
        if path.is_absolute() {
            return Err("absolute executable paths are not allowed".to_owned());
        }
        if program.contains('/') || program.contains('\\') {
            let relative = clean_relative_path(program)?;
            let requested = cwd.join(relative);
            let resolved = std::fs::canonicalize(&requested)
                .map_err(|error| format!("cannot resolve executable `{program}`: {error}"))?;
            if !resolved.starts_with(&self.workspace_root) {
                return Err(format!(
                    "executable resolves outside the configured workspace: `{program}`"
                ));
            }
            if !resolved.is_file() {
                return Err(format!("executable is not a file: `{program}`"));
            }
            return Ok(resolved.to_string_lossy().into_owned());
        }

        Ok(program.to_owned())
    }

    fn detect_package_manager(&self, cwd: &Path) -> String {
        let mut current = Some(cwd);
        while let Some(directory) = current {
            if directory.join("pnpm-lock.yaml").is_file() {
                return "pnpm".to_owned();
            }
            if directory.join("yarn.lock").is_file() {
                return "yarn".to_owned();
            }
            if directory.join("bun.lockb").is_file() || directory.join("bun.lock").is_file() {
                return "bun".to_owned();
            }
            if directory.join("package-lock.json").is_file() {
                return "npm".to_owned();
            }
            if directory == self.workspace_root {
                break;
            }
            current = directory
                .parent()
                .filter(|parent| parent.starts_with(&self.workspace_root));
        }
        "npm".to_owned()
    }

    async fn execute(&self, args: &ExecuteArgs) -> std::result::Result<ExecutionOutput, String> {
        if args.args.len() > MAX_ARGS {
            return Err(format!("too many arguments; maximum is {MAX_ARGS}"));
        }
        for value in &args.args {
            validate_argument(value)?;
        }
        if let Some(stdin) = args.stdin.as_deref()
            && (stdin.len() > MAX_STDIN_BYTES || stdin.as_bytes().contains(&0))
        {
            return Err(format!(
                "stdin must be at most {MAX_STDIN_BYTES} bytes and must not contain NUL"
            ));
        }
        validate_env(&args.env)?;

        let cwd = self.resolve_cwd(args.cwd.as_deref())?;
        let program = self.validate_program(&args.program, &cwd)?;
        let timeout_ms = self.validated_timeout(args.timeout_ms)?;
        self.run_process(
            &program,
            &args.args,
            &cwd,
            timeout_ms,
            args.stdin.as_deref(),
            &args.env,
        )
        .await
    }

    async fn run_process(
        &self,
        program: &str,
        args: &[String],
        cwd: &Path,
        timeout_ms: u64,
        stdin: Option<&str>,
        env: &BTreeMap<String, String>,
    ) -> std::result::Result<ExecutionOutput, String> {
        let mut command = Command::new(program);
        command
            .args(args)
            .current_dir(cwd)
            .kill_on_drop(true)
            .env_clear()
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        for name in BASE_ENV_ALLOWLIST {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        for (name, value) in std::env::vars_os() {
            if name
                .to_str()
                .is_some_and(|name_text| name_text.starts_with("LC_"))
            {
                command.env(name, value);
            }
        }
        command.envs(env);
        command.env("PAGER", "cat");
        command.env("GIT_PAGER", "cat");
        command.env("NO_COLOR", "1");

        if stdin.is_some() {
            command.stdin(std::process::Stdio::piped());
        }

        let started = Instant::now();
        let mut child = command
            .spawn()
            .map_err(|error| format!("failed to start `{program}`: {error}"))?;

        if let Some(input) = stdin
            && let Some(mut child_stdin) = child.stdin.take()
        {
            child_stdin
                .write_all(input.as_bytes())
                .await
                .map_err(|error| format!("failed to write child stdin: {error}"))?;
        }

        let waited = timeout(Duration::from_millis(timeout_ms), child.wait_with_output()).await;
        let duration_ms = started.elapsed().as_millis();

        match waited {
            Ok(result) => {
                let output = result
                    .map_err(|error| format!("failed while waiting for `{program}`: {error}"))?;
                let (stdout, stdout_truncated) = bounded_utf8(output.stdout, self.max_output_bytes);
                let (stderr, stderr_truncated) = bounded_utf8(output.stderr, self.max_output_bytes);
                let diagnostics = summarize_diagnostics(&stdout, &stderr);

                Ok(ExecutionOutput {
                    program: program.to_owned(),
                    args: args.to_vec(),
                    cwd: self.relative_display(cwd),
                    exit_code: output.status.code(),
                    success: output.status.success(),
                    timed_out: false,
                    duration_ms,
                    stdout,
                    stderr,
                    stdout_truncated,
                    stderr_truncated,
                    diagnostics,
                })
            }
            Err(_) => {
                let message = format!(
                    "process exceeded timeout of {timeout_ms} ms; the direct child was terminated"
                );
                Ok(ExecutionOutput {
                    program: program.to_owned(),
                    args: args.to_vec(),
                    cwd: self.relative_display(cwd),
                    exit_code: None,
                    success: false,
                    timed_out: true,
                    duration_ms,
                    stdout: String::new(),
                    stderr: message.clone(),
                    stdout_truncated: false,
                    stderr_truncated: false,
                    diagnostics: DiagnosticSummary {
                        error_lines: 1,
                        warning_lines: 0,
                        failure_lines: 1,
                        highlights: vec![message],
                    },
                })
            }
        }
    }

    fn detect_project(&self, cwd_raw: &str) -> std::result::Result<ProjectDetection, String> {
        let cwd = self.resolve_cwd(Some(cwd_raw))?;
        let mut kinds = BTreeSet::new();
        let mut markers = Vec::new();

        if cwd.join("Cargo.toml").is_file() {
            kinds.insert("rust".to_owned());
            markers.push("Cargo.toml".to_owned());
        }
        if cwd.join("go.mod").is_file() {
            kinds.insert("go".to_owned());
            markers.push("go.mod".to_owned());
        }

        let makefile = ["Makefile", "makefile", "GNUmakefile"]
            .iter()
            .map(|name| cwd.join(name))
            .find(|path| path.is_file());
        if let Some(path) = &makefile {
            kinds.insert("make".to_owned());
            markers.push(
                path.file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("Makefile")
                    .to_owned(),
            );
        }

        let package_json = cwd.join("package.json");
        let (package_manager, node_scripts) = if package_json.is_file() {
            kinds.insert("node".to_owned());
            markers.push("package.json".to_owned());
            let node = read_node_manifest(&package_json)?;
            if node.has_next {
                kinds.insert("nextjs".to_owned());
            }
            if node.has_vue {
                kinds.insert("vue".to_owned());
            }
            (Some(self.detect_package_manager(&cwd)), node.scripts)
        } else {
            (None, Vec::new())
        };

        let make_targets = makefile
            .as_deref()
            .map(read_make_targets)
            .transpose()?
            .unwrap_or_default();

        let mut detection = ProjectDetection {
            cwd: self.relative_display(&cwd),
            kinds: kinds.into_iter().collect(),
            package_manager,
            node_scripts,
            make_targets,
            markers,
            recommended_checks: Vec::new(),
        };
        detection.recommended_checks = build_quality_plan(&detection, QualityProfile::Fast);
        Ok(detection)
    }

    async fn quality_check(
        &self,
        args: &QualityArgs,
    ) -> std::result::Result<QualityOutput, String> {
        let detection = self.detect_project(&args.cwd)?;
        if detection.kinds.is_empty() {
            return Err(format!(
                "no supported project markers found in `{}`; use workspace_execute for an explicit command",
                detection.cwd
            ));
        }

        let plan = build_quality_plan(&detection, args.profile);
        if plan.is_empty() {
            return Err(format!(
                "project was detected ({}) but no conventional quality checks could be inferred; use workspace_execute for explicit commands",
                detection.kinds.join(", ")
            ));
        }

        let cwd = self.resolve_cwd(Some(&args.cwd))?;
        let timeout_ms = self.validated_timeout(args.timeout_ms)?;
        let mut checks = Vec::new();
        let mut checks_failed = 0usize;
        let mut aggregate = DiagnosticSummary::default();

        for spec in plan {
            let program = self.validate_program(&spec.program, &cwd)?;
            let result = self
                .run_process(
                    &program,
                    &spec.args,
                    &cwd,
                    timeout_ms,
                    None,
                    &BTreeMap::new(),
                )
                .await?;
            merge_diagnostics(&mut aggregate, &result.diagnostics);
            let failed = !result.success;
            if failed {
                checks_failed += 1;
            }
            checks.push(QualityCheckResult {
                command: render_command(&spec.program, &spec.args),
                label: spec.label,
                result,
            });
            if failed && args.fail_fast {
                break;
            }
        }

        Ok(QualityOutput {
            cwd: detection.cwd,
            profile: args.profile,
            detected_kinds: detection.kinds,
            passed: checks_failed == 0,
            checks_run: checks.len(),
            checks_failed,
            checks,
            diagnostics: aggregate,
        })
    }
}

#[tool_router(server_handler)]
impl ExecServer {
    #[tool(
        description = "Execute one concrete development command inside the configured workspace. Uses program + argv directly (no MCP-level shell parsing), enforces workspace-relative cwd, bounded output, restricted inherited environment, and a timeout. Intended for Next.js/Vue, Rust, Go, Make, tests, linters, compilers, and diagnostic Linux CLI commands."
    )]
    async fn workspace_execute(
        &self,
        Parameters(args): Parameters<ExecuteArgs>,
    ) -> std::result::Result<CallToolResult, McpError> {
        Ok(match self.execute(&args).await {
            Ok(output) => self.success_json(&output),
            Err(error) => Self::failure(error),
        })
    }

    #[tool(
        description = "Detect supported project types and conventional quality commands from package.json, Cargo.toml, go.mod, and Makefile. Recognizes Next.js and Vue and searches ancestor lockfiles up to the workspace root so nested monorepo apps use the correct package manager. Does not execute commands."
    )]
    async fn workspace_detect_project(
        &self,
        Parameters(args): Parameters<ProjectArgs>,
    ) -> std::result::Result<CallToolResult, McpError> {
        Ok(match self.detect_project(&args.cwd) {
            Ok(output) => self.success_json(&output),
            Err(error) => Self::failure(error),
        })
    }

    #[tool(
        description = "Run inferred development quality checks and aggregate issue/error diagnostics. Supports Node/Next.js/Vue package scripts, Rust cargo checks, Go test/vet, and conventional Make targets. Fast mode focuses on lint/type/test/check; full mode additionally includes conventional production builds."
    )]
    async fn workspace_quality_check(
        &self,
        Parameters(args): Parameters<QualityArgs>,
    ) -> std::result::Result<CallToolResult, McpError> {
        Ok(match self.quality_check(&args).await {
            Ok(output) => self.success_json(&output),
            Err(error) => Self::failure(error),
        })
    }
}

fn clean_relative_dir(raw: &str) -> std::result::Result<PathBuf, String> {
    if raw == "." {
        return Ok(PathBuf::from("."));
    }
    clean_relative_path(raw)
}

fn clean_relative_path(raw: &str) -> std::result::Result<PathBuf, String> {
    validate_nonempty_text("path", raw, 4096)?;
    let path = Path::new(raw);
    if path.is_absolute() {
        return Err("absolute paths are not allowed".to_owned());
    }

    let mut cleaned = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => cleaned.push(part),
            Component::CurDir => {}
            Component::ParentDir => return Err("parent traversal (`..`) is not allowed".to_owned()),
            Component::RootDir | Component::Prefix(_) => {
                return Err("absolute paths are not allowed".to_owned());
            }
        }
    }

    if cleaned.as_os_str().is_empty() {
        return Err("path must resolve at or below the configured workspace root".to_owned());
    }
    Ok(cleaned)
}

fn validate_nonempty_text(
    kind: &str,
    value: &str,
    max_bytes: usize,
) -> std::result::Result<(), String> {
    if value.is_empty()
        || value.len() > max_bytes
        || value.as_bytes().contains(&0)
        || value.contains('\r')
    {
        return Err(format!(
            "{kind} must be non-empty, at most {max_bytes} bytes, and contain neither NUL nor CR"
        ));
    }
    Ok(())
}

fn validate_argument(value: &str) -> std::result::Result<(), String> {
    if value.len() > MAX_ARG_BYTES || value.as_bytes().contains(&0) {
        return Err(format!(
            "each argument must be at most {MAX_ARG_BYTES} bytes and must not contain NUL"
        ));
    }
    Ok(())
}

fn validate_env(env: &BTreeMap<String, String>) -> std::result::Result<(), String> {
    if env.len() > 128 {
        return Err("at most 128 explicit environment variables are allowed".to_owned());
    }

    for (name, value) in env {
        let name_valid = !name.is_empty()
            && name.len() <= 256
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            && !name
                .bytes()
                .next()
                .is_some_and(|byte| byte.is_ascii_digit());
        if !name_valid {
            return Err(format!("invalid environment variable name `{name}`"));
        }
        if value.len() > MAX_ARG_BYTES || value.as_bytes().contains(&0) {
            return Err(format!("invalid value for environment variable `{name}`"));
        }
    }
    Ok(())
}

fn bounded_utf8(bytes: Vec<u8>, limit: usize) -> (String, bool) {
    if bytes.len() <= limit {
        return (String::from_utf8_lossy(&bytes).into_owned(), false);
    }

    let mut output = String::from_utf8_lossy(&bytes[..limit]).into_owned();
    output.push_str("\n[output truncated by MCP Exec]\n");
    (output, true)
}

fn summarize_diagnostics(stdout: &str, stderr: &str) -> DiagnosticSummary {
    let mut summary = DiagnosticSummary::default();
    let mut seen = BTreeSet::new();

    for line in stdout.lines().chain(stderr.lines()) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let lower = trimmed.to_ascii_lowercase();
        let is_error = lower.contains("error")
            || lower.contains("panic")
            || lower.contains("exception")
            || lower.contains("traceback")
            || lower.contains("ts2")
            || lower.contains("ts6");
        let is_warning = lower.contains("warning") || lower.contains("warn:");
        let is_failure = lower.contains("failed")
            || lower.contains("failure")
            || lower == "fail"
            || lower.starts_with("fail ")
            || lower.starts_with("fail\t")
            || lower.contains("fail:")
            || lower.contains("✖")
            || lower.contains("not ok");

        if is_error {
            summary.error_lines += 1;
        }
        if is_warning {
            summary.warning_lines += 1;
        }
        if is_failure {
            summary.failure_lines += 1;
        }

        if (is_error || is_warning || is_failure)
            && summary.highlights.len() < MAX_DIAGNOSTIC_HIGHLIGHTS
            && seen.insert(trimmed.to_owned())
        {
            summary.highlights.push(trimmed.to_owned());
        }
    }

    summary
}

fn merge_diagnostics(target: &mut DiagnosticSummary, source: &DiagnosticSummary) {
    target.error_lines += source.error_lines;
    target.warning_lines += source.warning_lines;
    target.failure_lines += source.failure_lines;
    let mut seen: BTreeSet<String> = target.highlights.iter().cloned().collect();

    for line in &source.highlights {
        if target.highlights.len() >= MAX_DIAGNOSTIC_HIGHLIGHTS {
            break;
        }
        if seen.insert(line.clone()) {
            target.highlights.push(line.clone());
        }
    }
}

fn read_node_manifest(path: &Path) -> std::result::Result<NodeManifestInfo, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|error| format!("invalid {}: {error}", path.display()))?;

    let mut scripts: Vec<String> = value
        .get("scripts")
        .and_then(serde_json::Value::as_object)
        .map(|scripts| scripts.keys().cloned().collect())
        .unwrap_or_default();
    scripts.sort();

    let has_dependency = |name: &str| {
        ["dependencies", "devDependencies", "peerDependencies"]
            .iter()
            .any(|key| {
                value
                    .get(*key)
                    .and_then(serde_json::Value::as_object)
                    .is_some_and(|dependencies| dependencies.contains_key(name))
            })
    };

    Ok(NodeManifestInfo {
        scripts,
        has_next: has_dependency("next"),
        has_vue: has_dependency("vue"),
    })
}

fn read_make_targets(path: &Path) -> std::result::Result<Vec<String>, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let mut targets = BTreeSet::new();

    for line in text.lines() {
        if line.starts_with(' ')
            || line.starts_with('\t')
            || line.starts_with('#')
            || line.starts_with('.')
            || line.contains("::=")
            || line.contains(" :=")
        {
            continue;
        }

        let Some((left, _)) = line.split_once(':') else {
            continue;
        };
        for target in left.split_whitespace() {
            let valid = !target.contains('%')
                && !target.contains('$')
                && !target.contains('=')
                && target.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b'/')
                });
            if valid {
                targets.insert(target.to_owned());
            }
        }
    }

    Ok(targets.into_iter().collect())
}

fn build_quality_plan(detection: &ProjectDetection, profile: QualityProfile) -> Vec<CommandSpec> {
    let mut plan = Vec::new();

    if detection.kinds.iter().any(|kind| kind == "node") {
        let manager = detection.package_manager.as_deref().unwrap_or("npm");
        for (label, candidates) in [
            ("Node lint", &["lint", "lint:check", "check:lint"][..]),
            (
                "Node type check",
                &["typecheck", "type-check", "check:types", "types"][..],
            ),
            ("Node tests", &["test", "test:unit", "test:ci"][..]),
        ] {
            if let Some(script) = first_script(&detection.node_scripts, candidates) {
                plan.push(package_script_spec(manager, label, script));
            }
        }

        if matches!(profile, QualityProfile::Full)
            && first_script(&detection.node_scripts, &["build"]).is_some()
        {
            plan.push(package_script_spec(
                manager,
                "Node production build",
                "build",
            ));
        }
    }

    if detection.kinds.iter().any(|kind| kind == "rust") {
        plan.extend([
            CommandSpec {
                label: "Rust fmt check".to_owned(),
                program: "cargo".to_owned(),
                args: vec!["fmt".into(), "--all".into(), "--".into(), "--check".into()],
            },
            CommandSpec {
                label: "Rust check".to_owned(),
                program: "cargo".to_owned(),
                args: vec![
                    "check".into(),
                    "--all-targets".into(),
                    "--all-features".into(),
                ],
            },
            CommandSpec {
                label: "Rust clippy".to_owned(),
                program: "cargo".to_owned(),
                args: vec![
                    "clippy".into(),
                    "--all-targets".into(),
                    "--all-features".into(),
                    "--".into(),
                    "-D".into(),
                    "warnings".into(),
                ],
            },
            CommandSpec {
                label: "Rust tests".to_owned(),
                program: "cargo".to_owned(),
                args: vec![
                    "test".into(),
                    "--all-targets".into(),
                    "--all-features".into(),
                ],
            },
        ]);
        if matches!(profile, QualityProfile::Full) {
            plan.push(CommandSpec {
                label: "Rust release build".to_owned(),
                program: "cargo".to_owned(),
                args: vec!["build".into(), "--release".into(), "--all-features".into()],
            });
        }
    }

    if detection.kinds.iter().any(|kind| kind == "go") {
        plan.extend([
            CommandSpec {
                label: "Go vet".to_owned(),
                program: "go".to_owned(),
                args: vec!["vet".into(), "./...".into()],
            },
            CommandSpec {
                label: "Go tests".to_owned(),
                program: "go".to_owned(),
                args: vec!["test".into(), "./...".into()],
            },
        ]);
        if matches!(profile, QualityProfile::Full) {
            plan.push(CommandSpec {
                label: "Go build".to_owned(),
                program: "go".to_owned(),
                args: vec!["build".into(), "./...".into()],
            });
        }
    }

    if detection.kinds.iter().any(|kind| kind == "make") {
        for (target, label) in [
            ("lint", "Make lint"),
            ("check", "Make check"),
            ("test", "Make test"),
        ] {
            if detection.make_targets.iter().any(|value| value == target) {
                plan.push(CommandSpec {
                    label: label.to_owned(),
                    program: "make".to_owned(),
                    args: vec![target.to_owned()],
                });
            }
        }
        if matches!(profile, QualityProfile::Full)
            && detection
                .make_targets
                .iter()
                .any(|target| target == "build")
        {
            plan.push(CommandSpec {
                label: "Make build".to_owned(),
                program: "make".to_owned(),
                args: vec!["build".to_owned()],
            });
        }
    }

    dedupe_plan(plan)
}

fn first_script<'a>(scripts: &'a [String], candidates: &[&str]) -> Option<&'a str> {
    candidates.iter().find_map(|candidate| {
        scripts
            .iter()
            .find(|script| script.as_str() == *candidate)
            .map(String::as_str)
    })
}

fn package_script_spec(manager: &str, label: &str, script: &str) -> CommandSpec {
    CommandSpec {
        label: label.to_owned(),
        program: manager.to_owned(),
        args: vec!["run".to_owned(), script.to_owned()],
    }
}

fn dedupe_plan(plan: Vec<CommandSpec>) -> Vec<CommandSpec> {
    let mut seen = BTreeSet::new();
    plan.into_iter()
        .filter(|spec| seen.insert(render_command(&spec.program, &spec.args)))
        .collect()
}

fn render_command(program: &str, args: &[String]) -> String {
    std::iter::once(program)
        .chain(args.iter().map(String::as_str))
        .map(shell_display_token)
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_display_token(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(&byte))
    {
        return value.to_owned();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "rust_mcp_exec=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    let server = ExecServer::new(&cli)?;
    tracing::info!(
        workspace_root = %server.workspace_root.display(),
        default_timeout_ms = server.default_timeout_ms,
        max_timeout_ms = server.max_timeout_ms,
        max_output_bytes = server.max_output_bytes,
        "starting workspace Exec MCP server"
    );
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_validation_blocks_escape() {
        assert_eq!(clean_relative_dir(".").unwrap(), PathBuf::from("."));
        assert_eq!(
            clean_relative_path("project/src").unwrap(),
            PathBuf::from("project/src")
        );
        assert!(clean_relative_path("../secret").is_err());
        assert!(clean_relative_path("/etc").is_err());
    }

    #[test]
    fn diagnostic_summary_extracts_common_failures() {
        let result = summarize_diagnostics(
            "warning: unused value\nerror[E0308]: mismatch\n",
            "FAIL test_x\n",
        );
        assert_eq!(result.warning_lines, 1);
        assert_eq!(result.error_lines, 1);
        assert_eq!(result.failure_lines, 1);
        assert_eq!(result.highlights.len(), 3);
    }

    #[test]
    fn package_script_uses_explicit_run() {
        assert_eq!(
            package_script_spec("pnpm", "lint", "lint").args,
            vec!["run".to_owned(), "lint".to_owned()]
        );
        assert_eq!(
            package_script_spec("npm", "test", "test").args,
            vec!["run".to_owned(), "test".to_owned()]
        );
    }

    #[test]
    fn render_command_is_unambiguous() {
        assert_eq!(
            render_command("cargo", &["test".into(), "--all-features".into()]),
            "cargo test --all-features"
        );
        assert_eq!(
            render_command("tool", &["two words".into()]),
            "tool 'two words'"
        );
    }
}
