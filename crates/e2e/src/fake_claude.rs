use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const SCRIPT_ENV: &str = "PROWL_FAKE_CLAUDE_DIR";

/// What the fake CLI does for one invocation.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClaudeResponse {
    #[serde(default)]
    pub stdout: String,
    #[serde(default)]
    pub stderr: String,
    #[serde(default)]
    pub exit_code: i32,
    /// Sleep this long before answering (simulates a slow or hung run).
    #[serde(default)]
    pub delay_ms: u64,
}

/// What the fake CLI was called with.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaudeInvocation {
    pub args: Vec<String>,
    pub stdin: String,
    pub cwd: PathBuf,
    /// Files at the top of `cwd` when the CLI started.
    #[serde(default)]
    pub files: Vec<String>,
    /// `git rev-parse HEAD` in `cwd`, if it is a git checkout.
    #[serde(default)]
    pub git_head: Option<String>,
    /// Contents of `pr.diff` and `commits.txt` in the `--add-dir` directory, if present.
    #[serde(default)]
    pub review_diff: Option<String>,
    #[serde(default)]
    pub review_commits: Option<String>,
}

impl ClaudeInvocation {
    /// The value following `flag` on the command line.
    pub fn arg_after(&self, flag: &str) -> Option<&str> {
        let i = self.args.iter().position(|a| a == flag)?;
        self.args.get(i + 1).map(String::as_str)
    }

    /// Every value following `flag` until the next `--flag`.
    pub fn args_after(&self, flag: &str) -> Vec<&str> {
        let Some(i) = self.args.iter().position(|a| a == flag) else {
            return Vec::new();
        };
        self.args[i + 1..]
            .iter()
            .take_while(|a| !a.starts_with("--"))
            .map(String::as_str)
            .collect()
    }
}

/// Handle on the `fake-claude` binary. Responses are consumed in order; the last one repeats.
pub struct FakeClaude {
    dir: PathBuf,
}

impl ClaudeResponse {
    /// A successful run in `stream-json` format whose final answer is `answer`.
    pub fn answer(answer: &str) -> Self {
        let lines = [
            serde_json::json!({ "type": "system", "subtype": "init", "session_id": "session-1" }),
            serde_json::json!({
                "type": "assistant", "session_id": "session-1",
                "message": { "content": [{ "type": "text", "text": "Reading the diff…" }] }
            }),
            serde_json::json!({
                "type": "result", "subtype": "success", "is_error": false, "result": answer,
                "session_id": "session-1", "total_cost_usd": 0.05
            }),
        ];
        Self {
            stdout: lines.iter().map(|l| format!("{l}\n")).collect(),
            ..Default::default()
        }
    }

    /// A successful run answering with this review JSON.
    pub fn review(review: &serde_json::Value) -> Self {
        Self::answer(&review.to_string())
    }
}

impl FakeClaude {
    pub fn new(dir: PathBuf) -> anyhow::Result<Self> {
        std::fs::create_dir_all(dir.join("invocations"))?;
        let fake = Self { dir };
        fake.respond_with(vec![ClaudeResponse::default()])?;
        Ok(fake)
    }

    pub fn respond_with(&self, responses: Vec<ClaudeResponse>) -> anyhow::Result<()> {
        std::fs::write(
            self.dir.join("responses.json"),
            serde_json::to_vec_pretty(&responses)?,
        )?;
        Ok(())
    }

    /// Makes `claude auth status` report logged out (or in again).
    pub fn set_logged_in(&self, logged_in: bool) -> anyhow::Result<()> {
        let marker = self.dir.join("logged-out");
        if logged_in {
            let _ = std::fs::remove_file(marker);
        } else {
            std::fs::write(marker, "")?;
        }
        Ok(())
    }

    pub fn invocations(&self) -> anyhow::Result<Vec<ClaudeInvocation>> {
        read_invocations(&self.dir)
    }

    /// Path of the compiled `fake-claude` binary, next to the test executable's directory.
    pub fn bin(&self) -> PathBuf {
        if let Ok(path) = std::env::var("PROWL_FAKE_CLAUDE_BIN") {
            return path.into();
        }
        let exe = std::env::current_exe().expect("current exe");
        let mut dir = exe.parent().expect("exe dir").to_path_buf();
        if dir.ends_with("deps") {
            dir.pop();
        }
        dir.join(format!("fake-claude{}", std::env::consts::EXE_SUFFIX))
    }

    pub fn env(&self) -> Vec<(String, String)> {
        vec![(SCRIPT_ENV.into(), self.dir.display().to_string())]
    }
}

pub fn read_invocations(dir: &Path) -> anyhow::Result<Vec<ClaudeInvocation>> {
    let mut files: Vec<_> = std::fs::read_dir(dir.join("invocations"))?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .collect();
    files.sort();
    files
        .iter()
        .map(|f| Ok(serde_json::from_slice(&std::fs::read(f)?)?))
        .collect()
}
