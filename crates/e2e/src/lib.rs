//! The test seam: boot the real daemon against a fake GitHub server and a fake `claude` CLI,
//! then assert only on what reaches GitHub and what the Local API reports.

mod fake_claude;
mod fake_github;
mod fake_power;
mod git_remote;

pub use fake_claude::{ClaudeInvocation, ClaudeResponse, FakeClaude};
pub use fake_github::{FakeGitHub, RecordedRequest};
pub use fake_power::FakePower;
pub use git_remote::{GitRemote, PrCommits};

use prowld::{ApiClient, Config, CredentialBackend, RunningDaemon};
use tempfile::TempDir;

pub const TEST_CLIENT_ID: &str = "Iv1.testclient";

pub struct Harness {
    pub github: FakeGitHub,
    pub claude: FakeClaude,
    pub git: GitRemote,
    pub power: std::sync::Arc<FakePower>,
    daemon: Option<RunningDaemon>,
    home: TempDir,
}

impl Harness {
    pub async fn start() -> anyhow::Result<Self> {
        let home = tempfile::Builder::new().prefix("prowl-").tempdir()?;
        let github = FakeGitHub::start().await?;
        let claude = FakeClaude::new(home.path().join("fake-claude"))?;
        let git = GitRemote::new(home.path().join("remotes"))?;
        let power = std::sync::Arc::new(FakePower::laptop());
        let daemon = prowld::start(Self::config(home.path(), &github, &claude, &power)).await?;
        Ok(Self {
            github,
            claude,
            git,
            power,
            daemon: Some(daemon),
            home,
        })
    }

    fn config(
        home: &std::path::Path,
        github: &FakeGitHub,
        claude: &FakeClaude,
        power: &std::sync::Arc<FakePower>,
    ) -> Config {
        let mut config = Config::new(home.join("home"));
        config.github_api_url = github.url();
        config.github_web_url = github.url();
        config.github_client_id = Some(TEST_CLIENT_ID.into());
        // Never touch the developer's real keychain from tests.
        config.credentials = CredentialBackend::File;
        config.policy_sync_interval = std::time::Duration::from_millis(50);
        // Tests poll explicitly with `poll_now`; the scheduler stays out of the way.
        config.poll_interval = std::time::Duration::from_secs(3600);
        config.retry_base = std::time::Duration::from_millis(20);
        // Never take real sleep assertions on the machine running the tests.
        config.power = Some(prowld::PowerHook(power.clone()));
        config.power_check_interval = std::time::Duration::from_millis(50);
        config.wake_gap = std::time::Duration::from_secs(1);
        config.claude_bin = Some(claude.bin());
        config.claude_env = claude.env();
        config.git_base_url = home
            .join("remotes")
            .display()
            .to_string()
            .replace('\\', "/");
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
        let config = Self::config(self.home.path(), &self.github, &self.claude, &self.power);
        self.daemon = Some(prowld::start(config).await?);
        Ok(())
    }

    /// Starts a daemon with a custom config on this harness's home (the fakes are kept).
    pub async fn restart_with(&mut self, edit: impl FnOnce(&mut Config)) -> anyhow::Result<()> {
        if let Some(daemon) = self.daemon.take() {
            daemon.shutdown().await?;
        }
        let mut config = Self::config(self.home.path(), &self.github, &self.claude, &self.power);
        edit(&mut config);
        self.daemon = Some(prowld::start(config).await?);
        Ok(())
    }

    /// Stops the daemon but keeps the home and fakes, for simulating changes between runs.
    /// Scripts a successful device flow for `login` and waits until the daemon is connected.
    pub async fn connect_github(&self, login: &str) -> anyhow::Result<()> {
        use axum::http::{Method, StatusCode};
        use serde_json::json;
        self.github.route(
            Method::POST,
            "/login/device/code",
            StatusCode::OK,
            json!({
                "device_code": "dev-123",
                "user_code": "ABCD-1234",
                "verification_uri": "https://github.com/login/device",
                "expires_in": 900,
                "interval": 0
            }),
        );
        self.github.route(
            Method::POST,
            "/login/oauth/access_token",
            StatusCode::OK,
            json!({ "access_token": "gho_test", "token_type": "bearer" }),
        );
        self.github.route(
            Method::GET,
            "/user",
            StatusCode::OK,
            json!({ "login": login, "name": null }),
        );
        let client = self.client();
        client.start_login().await?;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let prowld::AuthState::Connected { .. } = client.auth_state().await? {
                return Ok(());
            }
            anyhow::ensure!(
                tokio::time::Instant::now() < deadline,
                "GitHub login did not complete"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

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
