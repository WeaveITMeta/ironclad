//! Shell execution tool for running commands in a sandboxed environment.
//!
//! Provides controlled command execution with:
//! - Docker sandbox isolation (when enabled)
//! - Working directory isolation
//! - Timeout enforcement
//! - Output capture and truncation
//! - Blocked command patterns for safety
//!
//! # Execution Modes
//!
//! When sandbox is available and enabled:
//! - Commands run inside ephemeral Docker containers
//! - Network traffic goes through a validating proxy
//! - Credentials are injected by the proxy, never exposed to commands
//!
//! When sandbox is unavailable:
//! - Commands run directly on host with basic protections
//! - Blocked command patterns are still enforced

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::context::JobContext;
use crate::sandbox::{SandboxManager, SandboxPolicy};
use crate::tools::tool::{Tool, ToolError, ToolOutput};

/// Maximum output size before truncation (64KB).
const MAX_OUTPUT_SIZE: usize = 64 * 1024;

/// Default command timeout.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

/// Commands that are always blocked for safety.
static BLOCKED_COMMANDS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    HashSet::from([
        "rm -rf /",
        "rm -rf /*",
        ":(){ :|:& };:", // Fork bomb
        "dd if=/dev/zero",
        "mkfs",
        "chmod -R 777 /",
        "> /dev/sda",
        "curl | sh",
        "wget | sh",
        "curl | bash",
        "wget | bash",
    ])
});

/// Patterns that indicate potentially dangerous commands.
static DANGEROUS_PATTERNS: LazyLock<Vec<&'static str>> = LazyLock::new(|| {
    vec![
        "sudo ",
        "doas ",
        " | sh",
        " | bash",
        " | zsh",
        "eval ",
        "$(curl",
        "$(wget",
        "/etc/passwd",
        "/etc/shadow",
        "~/.ssh",
        ".bash_history",
        "id_rsa",
    ]
});

/// Substring patterns whose presence in a shell command forces an
/// explicit approval banner EVERY time, even if the session has
/// "Always approved" the shell tool. These are write-path /
/// privilege-escalation / network-egress operations where consent
/// must be renewed per-invocation. Match is substring + case
/// insensitive on the normalized command, matching the existing
/// BLOCKED/DANGEROUS pattern style.
///
/// The principle: one "Always approve" click on a `cargo build`
/// banner should NOT silently grant `rm -rf` later in the same
/// session. Pattern-matching the actual command intent enforces
/// least-privilege per call.
static ALWAYS_REQUIRE_APPROVAL_PATTERNS: LazyLock<Vec<&'static str>> = LazyLock::new(|| {
    vec![
        // Destructive filesystem
        "rm ",
        "rmdir",
        "del ",
        "erase ",
        "chmod -r",
        "chown -r",
        "chmod 777",
        "mv ",
        // Privilege escalation
        "sudo ",
        "doas ",
        "runas",
        // Database write/drop
        "drop table",
        "drop database",
        "truncate table",
        "delete from",
        // Network egress + pipe-to-shell (already in DANGEROUS_PATTERNS,
        // restated here so the approval gate still fires when
        // `allow_dangerous` is on)
        "curl ",
        "wget ",
        "scp ",
        "rsync ",
        "ssh ",
        "nc ",
        " | sh",
        " | bash",
        " | zsh",
        "iex ",
        "invoke-expression",
        // Git rewrites / force-pushes
        "git push --force",
        "git push -f",
        "git push --mirror",
        "git reset --hard",
        "git clean -f",
        "git rebase -i",
        "git filter-branch",
        // Package publishing (irreversible)
        "npm publish",
        "cargo publish",
        "pip upload",
        "twine upload",
        // Process / system control
        "kill -9",
        "killall",
        "taskkill /f",
        "shutdown",
        "reboot",
        "halt",
        // Disk write
        "dd if=",
        "mkfs",
        "fdisk",
        "format c:",
    ]
});

/// Substring patterns whose presence in a shell command SKIPS the
/// approval banner entirely. These are read-only / dev-loop /
/// idempotent operations safe to fire without consent.
///
/// IMPORTANT: a command matches "safe" ONLY when it doesn't ALSO
/// match `ALWAYS_REQUIRE_APPROVAL_PATTERNS`. The dangerous-pattern
/// check runs first so e.g. `cargo publish` is treated as
/// always-require even though `cargo ` looks safe. See
/// `classify_command` for the dispatch order.
static SAFE_COMMAND_PREFIXES: LazyLock<Vec<&'static str>> = LazyLock::new(|| {
    vec![
        // Build / test loops (the most-used non-destructive dev cycle)
        "cargo build",
        "cargo test",
        "cargo check",
        "cargo clippy",
        "cargo fmt",
        "cargo doc",
        "cargo tree",
        "cargo run",
        "cargo bench",
        // Read-only git
        "git status",
        "git log",
        "git diff",
        "git show",
        "git branch",
        "git remote",
        "git ls-files",
        "git fetch",
        "git config --get",
        "git rev-parse",
        // Read-only filesystem inspection
        "ls",
        "pwd",
        "cd ",
        "echo ",
        "which ",
        "where ",
        "whoami",
        "uname",
        // Version probes (common pre-build sanity checks)
        "rustc --version",
        "node --version",
        "python --version",
        "python3 --version",
        "npm --version",
        "pnpm --version",
        "git --version",
    ]
});

/// Classification of a shell command for approval gating.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellApproval {
    /// Skip the approval banner entirely.
    AlwaysAllow,
    /// Always prompt, even when "Always" was previously clicked.
    AlwaysAsk,
    /// Use the existing per-tool default — banner with "Always"
    /// honored across the session.
    Default,
}

/// Decide how a shell command should be gated.
///
/// Order matters: dangerous wins over safe so `cargo publish`,
/// `git push --force`, etc. don't slip through on prefix match.
pub fn classify_command(cmd: &str) -> ShellApproval {
    let normalized = cmd.to_lowercase();
    let trimmed = normalized.trim();
    for pat in ALWAYS_REQUIRE_APPROVAL_PATTERNS.iter() {
        if trimmed.contains(pat) {
            return ShellApproval::AlwaysAsk;
        }
    }
    for prefix in SAFE_COMMAND_PREFIXES.iter() {
        if trimmed == *prefix
            || trimmed.starts_with(&format!("{prefix} "))
            || trimmed.starts_with(&format!("{prefix}\t"))
            || (prefix.ends_with(' ') && trimmed.starts_with(prefix.trim_end()))
        {
            return ShellApproval::AlwaysAllow;
        }
    }
    ShellApproval::Default
}

/// Shell command execution tool.
pub struct ShellTool {
    /// Working directory for commands (if None, uses job's working dir or cwd).
    working_dir: Option<PathBuf>,
    /// Command timeout.
    timeout: Duration,
    /// Whether to allow potentially dangerous commands (requires explicit approval).
    allow_dangerous: bool,
    /// Optional sandbox manager for Docker execution.
    sandbox: Option<Arc<SandboxManager>>,
    /// Sandbox policy to use when sandbox is available.
    sandbox_policy: SandboxPolicy,
}

impl std::fmt::Debug for ShellTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShellTool")
            .field("working_dir", &self.working_dir)
            .field("timeout", &self.timeout)
            .field("allow_dangerous", &self.allow_dangerous)
            .field("sandbox", &self.sandbox.is_some())
            .field("sandbox_policy", &self.sandbox_policy)
            .finish()
    }
}

impl ShellTool {
    /// Create a new shell tool with default settings.
    pub fn new() -> Self {
        Self {
            working_dir: None,
            timeout: DEFAULT_TIMEOUT,
            allow_dangerous: false,
            sandbox: None,
            sandbox_policy: SandboxPolicy::ReadOnly,
        }
    }

    /// Set the working directory.
    pub fn with_working_dir(mut self, dir: PathBuf) -> Self {
        self.working_dir = Some(dir);
        self
    }

    /// Set the command timeout.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Enable sandbox execution with the given manager.
    pub fn with_sandbox(mut self, sandbox: Arc<SandboxManager>) -> Self {
        self.sandbox = Some(sandbox);
        self
    }

    /// Set the sandbox policy.
    pub fn with_sandbox_policy(mut self, policy: SandboxPolicy) -> Self {
        self.sandbox_policy = policy;
        self
    }

    /// Check if a command is blocked.
    fn is_blocked(&self, cmd: &str) -> Option<&'static str> {
        let normalized = cmd.to_lowercase();

        for blocked in BLOCKED_COMMANDS.iter() {
            if normalized.contains(blocked) {
                return Some("Command contains blocked pattern");
            }
        }

        if !self.allow_dangerous {
            for pattern in DANGEROUS_PATTERNS.iter() {
                if normalized.contains(pattern) {
                    return Some("Command contains potentially dangerous pattern");
                }
            }
        }

        None
    }

    /// Execute a command through the sandbox.
    async fn execute_sandboxed(
        &self,
        sandbox: &SandboxManager,
        cmd: &str,
        workdir: &Path,
        timeout: Duration,
    ) -> Result<(String, i64), ToolError> {
        // Override sandbox config timeout if needed
        let result = tokio::time::timeout(timeout, async {
            sandbox
                .execute_with_policy(
                    cmd,
                    workdir,
                    self.sandbox_policy,
                    std::collections::HashMap::new(),
                )
                .await
        })
        .await;

        match result {
            Ok(Ok(output)) => {
                let combined = truncate_output(&output.output);
                Ok((combined, output.exit_code))
            }
            Ok(Err(e)) => Err(ToolError::ExecutionFailed(format!("Sandbox error: {}", e))),
            Err(_) => Err(ToolError::Timeout(timeout)),
        }
    }

    /// Execute a command directly (fallback when sandbox unavailable).
    async fn execute_direct(
        &self,
        cmd: &str,
        workdir: &PathBuf,
        timeout: Duration,
    ) -> Result<(String, i32), ToolError> {
        // Build command
        let mut command = if cfg!(target_os = "windows") {
            let mut c = Command::new("cmd");
            c.args(["/C", cmd]);
            c
        } else {
            let mut c = Command::new("sh");
            c.args(["-c", cmd]);
            c
        };

        command
            .current_dir(workdir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // Spawn process
        let mut child = command
            .spawn()
            .map_err(|e| ToolError::ExecutionFailed(format!("Failed to spawn command: {}", e)))?;

        // Wait with timeout
        let result = tokio::time::timeout(timeout, async {
            let status = child.wait().await?;

            // Read stdout
            let mut stdout = String::new();
            if let Some(mut out) = child.stdout.take() {
                let mut buf = vec![0u8; MAX_OUTPUT_SIZE];
                let n = out.read(&mut buf).await.unwrap_or(0);
                stdout = String::from_utf8_lossy(&buf[..n]).to_string();
            }

            // Read stderr
            let mut stderr = String::new();
            if let Some(mut err) = child.stderr.take() {
                let mut buf = vec![0u8; MAX_OUTPUT_SIZE];
                let n = err.read(&mut buf).await.unwrap_or(0);
                stderr = String::from_utf8_lossy(&buf[..n]).to_string();
            }

            // Combine output
            let output = if stderr.is_empty() {
                stdout
            } else if stdout.is_empty() {
                stderr
            } else {
                format!("{}\n\n--- stderr ---\n{}", stdout, stderr)
            };

            Ok::<_, std::io::Error>((output, status.code().unwrap_or(-1)))
        })
        .await;

        match result {
            Ok(Ok((output, code))) => Ok((truncate_output(&output), code)),
            Ok(Err(e)) => Err(ToolError::ExecutionFailed(format!(
                "Command execution failed: {}",
                e
            ))),
            Err(_) => {
                // Timeout - try to kill the process
                let _ = child.kill().await;
                Err(ToolError::Timeout(timeout))
            }
        }
    }

    /// Execute a command, using sandbox if available.
    async fn execute_command(
        &self,
        cmd: &str,
        workdir: Option<&str>,
        timeout: Option<u64>,
    ) -> Result<(String, i64), ToolError> {
        // Check for blocked commands
        if let Some(reason) = self.is_blocked(cmd) {
            return Err(ToolError::NotAuthorized(format!(
                "{}: {}",
                reason,
                truncate_for_error(cmd)
            )));
        }

        // Determine working directory
        let cwd = workdir
            .map(PathBuf::from)
            .or_else(|| self.working_dir.clone())
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

        // Determine timeout
        let timeout_duration = timeout.map(Duration::from_secs).unwrap_or(self.timeout);

        // Try sandbox execution if available
        if let Some(ref sandbox) = self.sandbox {
            if sandbox.is_initialized() || sandbox.config().enabled {
                match self
                    .execute_sandboxed(sandbox, cmd, &cwd, timeout_duration)
                    .await
                {
                    Ok((output, code)) => return Ok((output, code)),
                    Err(e) => {
                        // Log sandbox failure and fall through to direct execution
                        tracing::warn!("Sandbox execution failed, falling back to direct: {}", e);
                    }
                }
            }
        }

        // Fallback to direct execution
        let (output, code) = self.execute_direct(cmd, &cwd, timeout_duration).await?;
        Ok((output, code as i64))
    }
}

impl Default for ShellTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for ShellTool {
    fn name(&self) -> &str {
        "shell"
    }

    fn description(&self) -> &str {
        "Execute shell commands. Use for running builds, tests, git operations, and other CLI tasks. \
         Commands run in a subprocess with captured output. Long-running commands have a timeout. \
         When Docker sandbox is enabled, commands run in isolated containers for security."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The shell command to execute"
                },
                "workdir": {
                    "type": "string",
                    "description": "Working directory for the command (optional)"
                },
                "timeout": {
                    "type": "integer",
                    "description": "Timeout in seconds (optional, default 120)"
                }
            },
            "required": ["command"]
        })
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: &JobContext,
    ) -> Result<ToolOutput, ToolError> {
        let command = params
            .get("command")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidParameters("missing 'command' parameter".into()))?;

        let workdir = params.get("workdir").and_then(|v| v.as_str());
        let timeout = params.get("timeout").and_then(|v| v.as_u64());

        let start = std::time::Instant::now();
        let (output, exit_code) = self.execute_command(command, workdir, timeout).await?;
        let duration = start.elapsed();

        let sandboxed = self.sandbox.is_some();

        let result = serde_json::json!({
            "output": output,
            "exit_code": exit_code,
            "success": exit_code == 0,
            "sandboxed": sandboxed
        });

        Ok(ToolOutput::success(result, duration))
    }

    fn requires_approval(&self) -> bool {
        true // Shell commands should require approval
    }

    fn requires_sanitization(&self) -> bool {
        true // Shell output could contain anything
    }

    /// Per-command approval gating. Skips the banner for read-only
    /// dev-loop patterns (cargo build, git status, ls, etc.); forces
    /// the banner for destructive/escalation/egress patterns (rm,
    /// sudo, curl, git push --force, etc.) even when the session has
    /// "Always approved" shell as a whole. See `classify_command` for
    /// the full pattern lists and dispatch order.
    ///
    /// This is the gateway-level enforcement of the wishlist's
    /// "per-pattern allowlist" item — a single Always-click no longer
    /// unlocks unlimited write access through subsequent shell
    /// invocations.
    fn approval_override(&self, params: &serde_json::Value) -> Option<bool> {
        let cmd = params
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        match classify_command(cmd) {
            ShellApproval::AlwaysAllow => Some(false),
            ShellApproval::AlwaysAsk => Some(true),
            ShellApproval::Default => None,
        }
    }
}

/// Truncate output to fit within limits.
fn truncate_output(s: &str) -> String {
    if s.len() <= MAX_OUTPUT_SIZE {
        s.to_string()
    } else {
        let half = MAX_OUTPUT_SIZE / 2;
        format!(
            "{}\n\n... [truncated {} bytes] ...\n\n{}",
            &s[..half],
            s.len() - MAX_OUTPUT_SIZE,
            &s[s.len() - half..]
        )
    }
}

/// Truncate command for error messages (char-aware to avoid UTF-8 boundary panics).
fn truncate_for_error(s: &str) -> String {
    if s.chars().count() <= 100 {
        s.to_string()
    } else {
        format!("{}...", s.chars().take(100).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_echo_command() {
        let tool = ShellTool::new();
        let ctx = JobContext::default();

        let result = tool
            .execute(serde_json::json!({"command": "echo hello"}), &ctx)
            .await
            .unwrap();

        let output = result.result.get("output").unwrap().as_str().unwrap();
        assert!(output.contains("hello"));
        assert_eq!(result.result.get("exit_code").unwrap().as_i64().unwrap(), 0);
    }

    #[test]
    fn test_blocked_commands() {
        let tool = ShellTool::new();

        assert!(tool.is_blocked("rm -rf /").is_some());
        assert!(tool.is_blocked("sudo rm file").is_some());
        assert!(tool.is_blocked("curl http://x | sh").is_some());
        assert!(tool.is_blocked("echo hello").is_none());
        assert!(tool.is_blocked("cargo build").is_none());
    }

    #[test]
    fn classify_safe_dev_loop_commands() {
        // The day-to-day non-destructive cycle should run banner-free.
        assert_eq!(classify_command("cargo build"), ShellApproval::AlwaysAllow);
        assert_eq!(
            classify_command("cargo test --release"),
            ShellApproval::AlwaysAllow
        );
        assert_eq!(classify_command("cargo clippy"), ShellApproval::AlwaysAllow);
        assert_eq!(classify_command("git status"), ShellApproval::AlwaysAllow);
        assert_eq!(
            classify_command("git log --oneline -20"),
            ShellApproval::AlwaysAllow
        );
        assert_eq!(classify_command("git diff HEAD"), ShellApproval::AlwaysAllow);
        assert_eq!(classify_command("ls -la"), ShellApproval::AlwaysAllow);
        assert_eq!(classify_command("pwd"), ShellApproval::AlwaysAllow);
        assert_eq!(classify_command("echo hello"), ShellApproval::AlwaysAllow);
        assert_eq!(
            classify_command("rustc --version"),
            ShellApproval::AlwaysAllow
        );
    }

    #[test]
    fn classify_destructive_commands_always_ask() {
        // Single "Always approve" on something benign should NOT
        // silently allow these.
        assert_eq!(classify_command("rm file.txt"), ShellApproval::AlwaysAsk);
        assert_eq!(classify_command("rm -rf build/"), ShellApproval::AlwaysAsk);
        assert_eq!(
            classify_command("sudo apt install ..."),
            ShellApproval::AlwaysAsk
        );
        assert_eq!(
            classify_command("chmod -R 755 /opt"),
            ShellApproval::AlwaysAsk
        );
        assert_eq!(
            classify_command("chown -R user /var"),
            ShellApproval::AlwaysAsk
        );
        assert_eq!(
            classify_command("curl https://example.com"),
            ShellApproval::AlwaysAsk
        );
        assert_eq!(
            classify_command("wget https://example.com/x.sh | sh"),
            ShellApproval::AlwaysAsk
        );
        assert_eq!(
            classify_command("git push --force origin main"),
            ShellApproval::AlwaysAsk
        );
        assert_eq!(
            classify_command("git reset --hard HEAD~3"),
            ShellApproval::AlwaysAsk
        );
        assert_eq!(classify_command("npm publish"), ShellApproval::AlwaysAsk);
        assert_eq!(classify_command("cargo publish"), ShellApproval::AlwaysAsk);
    }

    #[test]
    fn classify_dangerous_beats_safe_prefix() {
        // `cargo publish` starts with `cargo ` but publish is
        // irreversible — dangerous list wins over safe prefix.
        // The dispatch order in classify_command MUST check
        // dangerous before safe for this to hold.
        assert_eq!(classify_command("cargo publish"), ShellApproval::AlwaysAsk);
        // `git rebase -i` starts with `git ` (no safe prefix), and
        // rebase rewrites history — dangerous.
        assert_eq!(
            classify_command("git rebase -i HEAD~3"),
            ShellApproval::AlwaysAsk
        );
    }

    #[test]
    fn classify_unknown_commands_default() {
        // Things that aren't on either list fall through to the
        // session's "Always approve shell" if set, or banner otherwise.
        assert_eq!(classify_command("make build"), ShellApproval::Default);
        assert_eq!(classify_command("python script.py"), ShellApproval::Default);
        assert_eq!(classify_command("docker ps"), ShellApproval::Default);
    }

    #[test]
    fn classify_case_insensitive() {
        // The classifier normalizes case before matching so a model
        // emitting "Rm -Rf" or "SUDO" doesn't slip through.
        assert_eq!(classify_command("RM -rf /"), ShellApproval::AlwaysAsk);
        assert_eq!(classify_command("SUDO rm"), ShellApproval::AlwaysAsk);
        assert_eq!(classify_command("Cargo Build"), ShellApproval::AlwaysAllow);
    }

    #[test]
    fn approval_override_forces_or_skips() {
        // Wire-up check: the trait override returns the right
        // Some(bool) based on the classifier.
        let tool = ShellTool::new();
        let safe = serde_json::json!({"command": "cargo build"});
        let danger = serde_json::json!({"command": "rm -rf node_modules"});
        let neutral = serde_json::json!({"command": "make all"});
        assert_eq!(tool.approval_override(&safe), Some(false));
        assert_eq!(tool.approval_override(&danger), Some(true));
        assert_eq!(tool.approval_override(&neutral), None);
        // Missing `command` param falls through to default (no override).
        let no_cmd = serde_json::json!({});
        assert_eq!(tool.approval_override(&no_cmd), None);
    }

    #[tokio::test]
    async fn test_command_timeout() {
        let tool = ShellTool::new().with_timeout(Duration::from_millis(100));
        let ctx = JobContext::default();

        let result = tool
            .execute(serde_json::json!({"command": "sleep 10"}), &ctx)
            .await;

        assert!(matches!(result, Err(ToolError::Timeout(_))));
    }

    #[test]
    fn test_sandbox_policy_builder() {
        let tool = ShellTool::new()
            .with_sandbox_policy(SandboxPolicy::WorkspaceWrite)
            .with_timeout(Duration::from_secs(60));

        assert_eq!(tool.sandbox_policy, SandboxPolicy::WorkspaceWrite);
        assert_eq!(tool.timeout, Duration::from_secs(60));
    }
}
