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

/// Something Claude did while reviewing, as it streams in.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum StreamItem {
    /// Text Claude wrote.
    Text(String),
    /// A tool Claude called.
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    /// What that tool returned.
    ToolResult {
        id: String,
        content: String,
        is_error: bool,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ClaudeAnswer {
    /// The final text answer.
    pub text: String,
    /// The validated structured output, when the CLI provides one.
    pub structured: Option<serde_json::Value>,
    pub session_id: Option<String>,
    pub cost_usd: Option<f64>,
    /// As reported by the CLI.
    pub duration_ms: Option<i64>,
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
            None => crate::tools::find("claude").ok_or(ClaudeError::NotFound),
        }
    }

    /// Sends `prompt` (on stdin) and returns the final answer. `on_item` receives Claude's text,
    /// tool calls and tool results as they stream. `resume` continues an earlier session (used
    /// for repair requests).
    pub(crate) async fn run(
        &self,
        cwd: &Path,
        extra_dir: &Path,
        prompt: &str,
        resume: Option<&str>,
        options: &RunOptions,
        mut on_item: impl FnMut(StreamItem),
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
        // Only what the CLI needs: nothing it doesn't need can leak (cloud keys, tokens, …).
        cmd.env_clear()
            .envs(claude_environment(std::env::vars()))
            .envs(self.settings.env.iter().map(|(k, v)| (k, v)))
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
                if event.get("type").and_then(|v| v.as_str()) == Some("result") {
                    answer = Some(event);
                } else {
                    stream_items(&event).into_iter().for_each(&mut on_item);
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
            duration_ms: result.get("duration_ms").and_then(|v| v.as_i64()),
        })
    }
}

/// The environment a Claude run gets: the basics a program needs (home, path, locale, temp,
/// proxy and certificate settings), Claude Code's own settings and credentials (`CLAUDE_*`,
/// `ANTHROPIC_*`), and the cloud credentials of the Bedrock or Vertex setup it's configured
/// for. Everything else — other tokens, keys and secrets in the daemon's environment — is left
/// out.
fn claude_environment(vars: impl Iterator<Item = (String, String)>) -> Vec<(String, String)> {
    const EXACT: &[&str] = &[
        "HOME",
        "USER",
        "LOGNAME",
        "PATH",
        "SHELL",
        "LANG",
        "TERM",
        "TZ",
        "TMPDIR",
        "TMP",
        "TEMP",
        "SYSTEMROOT",
        "WINDIR",
        "COMSPEC",
        "PATHEXT",
        "APPDATA",
        "LOCALAPPDATA",
        "USERPROFILE",
        "HOMEDRIVE",
        "HOMEPATH",
        "PROGRAMDATA",
        "PROGRAMFILES",
        "SYSTEMDRIVE",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "NO_PROXY",
        "ALL_PROXY",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
        "NODE_EXTRA_CA_CERTS",
    ];
    let vars: Vec<(String, String)> = vars.collect();
    let set = |name: &str| {
        vars.iter()
            .any(|(k, v)| k == name && !v.is_empty() && v != "0")
    };
    let mut prefixes = vec!["LC_", "XDG_", "CLAUDE_", "ANTHROPIC_"];
    if set("CLAUDE_CODE_USE_BEDROCK") {
        prefixes.push("AWS_");
    }
    if set("CLAUDE_CODE_USE_VERTEX") {
        prefixes.extend(["GOOGLE_", "CLOUD_ML_", "GCLOUD_"]);
    }
    vars.into_iter()
        .filter(|(name, _)| {
            let upper = name.to_ascii_uppercase();
            EXACT.contains(&upper.as_str()) || prefixes.iter().any(|p| upper.starts_with(p))
        })
        .collect()
}

/// Text and tool calls from an `assistant` event, tool results from a `user` event.
fn stream_items(event: &serde_json::Value) -> Vec<StreamItem> {
    let str_of = |v: &serde_json::Value, key: &str| {
        v.get(key)
            .and_then(|t| t.as_str())
            .unwrap_or_default()
            .to_owned()
    };
    let role = event.get("type").and_then(|t| t.as_str());
    event
        .pointer("/message/content")
        .and_then(|c| c.as_array())
        .into_iter()
        .flatten()
        .filter_map(
            |block| match (role, block.get("type").and_then(|t| t.as_str())) {
                (Some("assistant"), Some("text")) => Some(StreamItem::Text(str_of(block, "text"))),
                (Some("assistant"), Some("tool_use")) => Some(StreamItem::ToolUse {
                    id: str_of(block, "id"),
                    name: str_of(block, "name"),
                    input: block.get("input").cloned().unwrap_or_default(),
                }),
                (Some("user"), Some("tool_result")) => Some(StreamItem::ToolResult {
                    id: str_of(block, "tool_use_id"),
                    content: tool_result_text(block.get("content")),
                    is_error: block.get("is_error").and_then(|e| e.as_bool()) == Some(true),
                }),
                _ => None,
            },
        )
        .collect()
}

/// A tool result's content: plain text, or the text parts of a list of blocks.
fn tool_result_text(content: Option<&serde_json::Value>) -> String {
    match content {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn names(vars: &[(&str, &str)]) -> Vec<String> {
        let mut kept: Vec<String> =
            claude_environment(vars.iter().map(|(k, v)| (k.to_string(), v.to_string())))
                .into_iter()
                .map(|(k, _)| k)
                .collect();
        kept.sort();
        kept
    }

    #[test]
    fn claude_gets_only_what_it_needs() {
        let kept = names(&[
            ("HOME", "/Users/a"),
            ("PATH", "/usr/bin"),
            ("LC_ALL", "en_US.UTF-8"),
            ("ANTHROPIC_API_KEY", "sk-ant-x"),
            ("CLAUDE_CONFIG_DIR", "/x"),
            ("https_proxy", "http://proxy"),
            ("GITHUB_TOKEN", "ghp_secret"),
            ("AWS_SECRET_ACCESS_KEY", "aws-secret"),
            ("OPENAI_API_KEY", "sk-x"),
            ("NPM_TOKEN", "npm"),
        ]);
        assert_eq!(
            kept,
            [
                "ANTHROPIC_API_KEY",
                "CLAUDE_CONFIG_DIR",
                "HOME",
                "LC_ALL",
                "PATH",
                "https_proxy"
            ]
        );
    }

    #[test]
    fn cloud_credentials_pass_only_for_the_cloud_claude_uses() {
        let kept = names(&[
            ("CLAUDE_CODE_USE_BEDROCK", "1"),
            ("AWS_REGION", "us-east-1"),
            ("AWS_ACCESS_KEY_ID", "AKIA"),
            ("GOOGLE_APPLICATION_CREDENTIALS", "/k.json"),
        ]);
        assert_eq!(
            kept,
            ["AWS_ACCESS_KEY_ID", "AWS_REGION", "CLAUDE_CODE_USE_BEDROCK"]
        );
    }
}
