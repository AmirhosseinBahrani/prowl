//! Checks the tools reviews depend on, from the daemon's point of view: the background
//! service may see a different PATH and environment than the user's shell.

use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::process::Command;

use crate::claude::Claude;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCheck {
    pub found: bool,
    pub path: Option<String>,
    pub version: Option<String>,
    /// What's wrong and how to fix it, if anything.
    pub problem: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaudeCheck {
    #[serde(flatten)]
    pub tool: ToolCheck,
    /// From `claude auth status`; `None` if it couldn't be determined.
    pub logged_in: Option<bool>,
    pub auth_method: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Doctor {
    pub claude: ClaudeCheck,
    pub git: ToolCheck,
}

impl Doctor {
    /// Whether reviews can run.
    pub fn ok(&self) -> bool {
        self.claude.tool.problem.is_none() && self.git.problem.is_none()
    }
}

const TIMEOUT: Duration = Duration::from_secs(20);

pub(crate) async fn check(claude: &Claude, env: &[(String, String)]) -> Doctor {
    Doctor {
        claude: check_claude(claude, env).await,
        git: check_git().await,
    }
}

async fn check_claude(claude: &Claude, env: &[(String, String)]) -> ClaudeCheck {
    let Ok(bin) = claude.locate() else {
        return ClaudeCheck {
            tool: ToolCheck {
                problem: Some(
                    "The claude CLI was not found. Install Claude Code (https://claude.com/claude-code) \
                     and make sure `claude` is on the PATH the PRowl service uses."
                        .into(),
                ),
                ..Default::default()
            },
            ..Default::default()
        };
    };
    let mut check = ClaudeCheck {
        tool: ToolCheck {
            found: true,
            path: Some(bin.display().to_string()),
            version: output(&bin, &["--version"], env)
                .await
                .map(|v| v.trim().to_owned()),
            problem: None,
        },
        ..Default::default()
    };
    // Only the login state and method are kept; the account's email and org are not.
    let status = output(&bin, &["auth", "status", "--json"], env)
        .await
        .and_then(|out| serde_json::from_str::<serde_json::Value>(&out).ok());
    check.logged_in = status.as_ref().and_then(|s| s.get("loggedIn")?.as_bool());
    check.auth_method = status
        .as_ref()
        .and_then(|s| s.get("authMethod")?.as_str().map(str::to_owned));
    if check.logged_in == Some(false) {
        check.tool.problem =
            Some("The claude CLI is not logged in. Run `claude auth login` once.".into());
    }
    check
}

async fn check_git() -> ToolCheck {
    let Ok(bin) = which::which("git") else {
        let hint = if cfg!(windows) {
            "Git was not found. Install Git for Windows (https://git-scm.com/download/win)."
        } else {
            "Git was not found. Install git and make sure it is on PATH."
        };
        return ToolCheck {
            problem: Some(hint.into()),
            ..Default::default()
        };
    };
    ToolCheck {
        found: true,
        path: Some(bin.display().to_string()),
        version: output(&bin, &["--version"], &[])
            .await
            .map(|v| v.trim().to_owned()),
        problem: None,
    }
}

async fn output(bin: &Path, args: &[&str], env: &[(String, String)]) -> Option<String> {
    let mut cmd = Command::new(bin);
    cmd.args(args)
        .envs(env.iter().map(|(k, v)| (k, v)))
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let out = tokio::time::timeout(TIMEOUT, cmd.output())
        .await
        .ok()?
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}
