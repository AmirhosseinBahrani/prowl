//! The test seam: boot the real daemon against a fake GitHub server and a fake `claude` CLI,
//! then assert only on what reaches GitHub and what the Local API reports.

mod fake_claude;
mod fake_github;

pub use fake_claude::{ClaudeInvocation, ClaudeResponse, FakeClaude};
pub use fake_github::{FakeGitHub, RecordedRequest};

use prowld::{ApiClient, Config, CredentialBackend, RunningDaemon};
use tempfile::TempDir;

pub const TEST_CLIENT_ID: &str = "Iv1.testclient";

pub struct Harness {
    pub github: FakeGitHub,
    pub claude: FakeClaude,
    daemon: Option<RunningDaemon>,
    home: TempDir,
}

impl Harness {
    pub async fn start() -> anyhow::Result<Self> {
        let home = tempfile::Builder::new().prefix("prowl-").tempdir()?;
        let github = FakeGitHub::start().await?;
        let claude = FakeClaude::new(home.path().join("fake-claude"))?;
        let daemon = prowld::start(Self::config(home.path(), &github, &claude)).await?;
        Ok(Self {
            github,
            claude,
            daemon: Some(daemon),
            home,
        })
    }

    fn config(home: &std::path::Path, github: &FakeGitHub, claude: &FakeClaude) -> Config {
        let mut config = Config::new(home.join("home"));
        config.github_api_url = github.url();
        config.github_web_url = github.url();
        config.github_client_id = Some(TEST_CLIENT_ID.into());
        // Never touch the developer's real keychain from tests.
        config.credentials = CredentialBackend::File;
        config.policy_sync_interval = std::time::Duration::from_millis(50);
        config.claude_bin = Some(claude.bin());
        config.claude_env = claude.env();
        config
    }

    pub fn client(&self) -> ApiClient {
        self.daemon().client()
    }

    pub fn daemon(&self) -> &RunningDaemon {
        self.daemon.as_ref().expect("daemon is running")
    }

    /// Stops the daemon and starts a fresh one on the same home, as after a crash or reboot.
    pub async fn restart(&mut self) -> anyhow::Result<()> {
        if let Some(daemon) = self.daemon.take() {
            daemon.shutdown().await?;
        }
        let config = Self::config(self.home.path(), &self.github, &self.claude);
        self.daemon = Some(prowld::start(config).await?);
        Ok(())
    }

    /// Starts a daemon with a custom config on this harness's home (the fakes are kept).
    pub async fn restart_with(&mut self, edit: impl FnOnce(&mut Config)) -> anyhow::Result<()> {
        if let Some(daemon) = self.daemon.take() {
            daemon.shutdown().await?;
        }
        let mut config = Self::config(self.home.path(), &self.github, &self.claude);
        edit(&mut config);
        self.daemon = Some(prowld::start(config).await?);
        Ok(())
    }

    /// Stops the daemon but keeps the home and fakes, for simulating changes between runs.
    pub async fn shutdown_daemon_only(&mut self) -> anyhow::Result<()> {
        if let Some(daemon) = self.daemon.take() {
            daemon.shutdown().await?;
        }
        Ok(())
    }

    pub async fn shutdown(mut self) -> anyhow::Result<()> {
        if let Some(daemon) = self.daemon.take() {
            daemon.shutdown().await?;
        }
        Ok(())
    }
}
