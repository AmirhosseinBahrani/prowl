//! Runs the local `claude` CLI non-interactively with read-only tools, streaming its output.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

/// Tools Claude may use while reviewing: reading and searching the checkout, nothing else.
/// No shell: even "read-only" git commands can write files (`--output`) or read outside the
/// checkout (`--no-index`), and the PR content steering Claude is untrusted.
const TOOLS: &str = "Read,Grep,Glob";
const ALLOWED_TOOLS: &[&str] = &["Read", "Grep", "Glob"];
const DISALLOWED_TOOLS: &[&str] = &[
    "Bash",
    "PowerShell",
    "Write",
    "Edit",
    "MultiEdit",
    "NotebookEdit",
    "WebFetch",
    "WebSearch",
    "Task",
];

#[derive(Debug, Clone)]
pub(crate) struct ClaudeSettings {
    /// Explicit binary; otherwise `claude` is looked up on PATH.
    pub bin: Option<PathBuf>,
    pub env: Vec<(String, String)>,
}

/// Per-run choices, taken from the current settings.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RunOptions {
    pub model: Option<String>,
    pub timeout: Duration,
    /// Passed as `--max-budget-usd`.
    pub max_budget_usd: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ClaudeAnswer {
    /// The final text answer.
    pub text: String,
    /// The validated structured output, when the CLI provides one.
    pub structured: Option<serde_json::Value>,
    pub session_id: Option<String>,
    pub cost_usd: Option<f64>,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ClaudeError {
    #[error("the claude CLI was not found; install Claude Code and make sure `claude` is on PATH")]
    NotFound,
    #[error("claude did not finish within {0:?} and was stopped")]
    Timeout(Duration),
    #[error("claude exited with {code:?}: {stderr}")]
    Failed { code: Option<i32>, stderr: String },
    #[error("claude reported an error: {0}")]
    Reported(String),
    #[error("claude produced no result")]
    NoResult,
    #[error("could not run claude: {0}")]
    Io(#[from] std::io::Error),
}

pub(crate) struct Claude {
    settings: ClaudeSettings,
}

impl Claude {
    pub(crate) fn new(settings: ClaudeSettings) -> Self {
        Self { settings }
    }

    pub(crate) fn locate(&self) -> Result<PathBuf, ClaudeError> {
        match &self.settings.bin {
            Some(bin) if bin.exists() => Ok(bin.clone()),
            Some(_) => Err(ClaudeError::NotFound),
            // `which` resolves `claude.exe` / `claude.cmd` on Windows.
            None => which::which("claude").map_err(|_| ClaudeError::NotFound),
        }
    }

    /// Sends `prompt` (on stdin) and returns the final answer. `on_text` receives assistant
    /// text as it streams. `resume` continues an earlier session (used for repair requests).
    pub(crate) async fn run(
        &self,
        cwd: &Path,
        extra_dir: &Path,
        prompt: &str,
        resume: Option<&str>,
        options: &RunOptions,
        mut on_text: impl FnMut(&str),
    ) -> Result<ClaudeAnswer, ClaudeError> {
        let bin = self.locate()?;
        let mut cmd = Command::new(&bin);
        cmd.current_dir(cwd)
            .args(["-p", "--output-format", "stream-json", "--verbose"])
            // Restricted mode ignores every settings file (so a PR's .claude/ hooks never load),
            // drops code-running tools except those named in --tools, and confines file tools
            // to the checkout. No MCP servers either.
            .args(["--restricted", "--strict-mcp-config"])
            .args(["--permission-mode", "dontAsk"])
            .arg("--tools")
            .arg(TOOLS)
            .arg("--allowedTools")
            .args(ALLOWED_TOOLS)
            .arg("--disallowedTools")
            .args(DISALLOWED_TOOLS)
            // The only readable place outside the checkout: PRowl's prepared diff.
            .arg("--add-dir")
            .arg(extra_dir)
            .arg("--json-schema")
            .arg(crate::review::schema().to_string());
        if let Some(model) = &options.model {
            cmd.args(["--model", model]);
        }
        if let Some(budget) = options.max_budget_usd {
            cmd.args(["--max-budget-usd", &budget.to_string()]);
        }
        if let Some(session) = resume {
            cmd.args(["--resume", session]);
        }
        cmd.envs(self.settings.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        cmd.process_group(0);

        let mut child = cmd.spawn().map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => ClaudeError::NotFound,
            _ => ClaudeError::Io(e),
        })?;
        let pid = child.id();
        let mut stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let mut stderr = child.stderr.take().expect("piped stderr");
        let prompt = prompt.to_owned();

        let work = async {
            stdin.write_all(prompt.as_bytes()).await?;
            drop(stdin);
            let stderr_task = tokio::spawn(async move {
                let mut buf = String::new();
                let _ = tokio::io::AsyncReadExt::read_to_string(&mut stderr, &mut buf).await;
                buf
            });
            let mut lines = BufReader::new(stdout).lines();
            let mut answer = None;
            let mut session_id = None;
            while let Some(line) = lines.next_line().await? {
                let Ok(event) = serde_json::from_str::<serde_json::Value>(&line) else {
                    continue;
                };
                if let Some(id) = event.get("session_id").and_then(|v| v.as_str()) {
                    session_id = Some(id.to_owned());
                }
                match event.get("type").and_then(|v| v.as_str()) {
                    Some("assistant") => {
                        for text in assistant_texts(&event) {
                            on_text(text);
                        }
                    }
                    Some("result") => answer = Some(event),
                    _ => {}
                }
            }
            let status = child.wait().await?;
            let stderr = stderr_task.await.unwrap_or_default();
            Ok::<_, ClaudeError>((status, answer, session_id, stderr))
        };

        let (status, answer, session_id, stderr) =
            match tokio::time::timeout(options.timeout, work).await {
                Ok(result) => result?,
                Err(_) => {
                    kill_tree(pid);
                    return Err(ClaudeError::Timeout(options.timeout));
                }
            };

        let Some(result) = answer else {
            if !status.success() {
                return Err(ClaudeError::Failed {
                    code: status.code(),
                    stderr: stderr.trim().chars().take(2000).collect(),
                });
            }
            return Err(ClaudeError::NoResult);
        };
        if result.get("is_error").and_then(|v| v.as_bool()) == Some(true) {
            let reason = result
                .get("result")
                .and_then(|v| v.as_str())
                .or_else(|| result.get("subtype").and_then(|v| v.as_str()))
                .unwrap_or("unknown error");
            return Err(ClaudeError::Reported(reason.to_owned()));
        }
        Ok(ClaudeAnswer {
            text: result
                .get("result")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_owned(),
            structured: result
                .get("structured_output")
                .filter(|v| v.is_object())
                .cloned(),
            session_id,
            cost_usd: result.get("total_cost_usd").and_then(|v| v.as_f64()),
        })
    }
}

fn assistant_texts(event: &serde_json::Value) -> impl Iterator<Item = &str> {
    event
        .pointer("/message/content")
        .and_then(|c| c.as_array())
        .into_iter()
        .flatten()
        .filter(|block| block.get("type").and_then(|t| t.as_str()) == Some("text"))
        .filter_map(|block| block.get("text").and_then(|t| t.as_str()))
}

/// Kills the CLI and everything it started.
fn kill_tree(pid: Option<u32>) {
    let Some(pid) = pid else { return };
    #[cfg(unix)]
    // SAFETY: signalling a process group we created; a stale id at worst fails with ESRCH.
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .output();
    }
}
