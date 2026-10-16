use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, anyhow};

pub const TOKEN_FILE: &str = "api-token";
pub const DB_FILE: &str = "prowl.db";

/// Where the GitHub token is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialBackend {
    /// The OS credential store, falling back to a private file when none is available.
    Auto,
    /// A file readable only by the current user, inside the PRowl home.
    File,
}

/// Everything the daemon needs to start. All on-disk state lives under `home`.
#[derive(Debug, Clone)]
pub struct Config {
    pub home: PathBuf,
    pub github_api_url: String,
    /// Base URL for OAuth endpoints (device flow lives on github.com, not the API host).
    pub github_web_url: String,
    /// Client ID of the PRowl GitHub OAuth App (device flow must be enabled on it).
    pub github_client_id: Option<String>,
    pub credentials: CredentialBackend,
    /// How often policy files are checked for edits made outside PRowl.
    pub policy_sync_interval: Duration,
    /// How often GitHub is checked for review requests.
    pub poll_interval: Duration,
    /// Count review requests made to one of the user's teams, not only direct ones.
    pub include_team_requests: bool,
    pub skip_drafts: bool,
    /// Path to the `claude` executable; `None` means look it up on PATH.
    pub claude_bin: Option<PathBuf>,
    /// Extra environment passed to every `claude` process.
    pub claude_env: Vec<(String, String)>,
}

impl Config {
    pub fn new(home: impl Into<PathBuf>) -> Self {
        Self {
            home: home.into(),
            github_api_url: "https://api.github.com".into(),
            github_web_url: "https://github.com".into(),
            github_client_id: std::env::var("PROWL_GITHUB_CLIENT_ID")
                .ok()
                .or_else(|| option_env!("PROWL_GITHUB_CLIENT_ID").map(str::to_owned))
                .filter(|id| !id.is_empty()),
            credentials: CredentialBackend::Auto,
            policy_sync_interval: Duration::from_secs(2),
            poll_interval: Duration::from_secs(60),
            include_team_requests: false,
            skip_drafts: true,
            claude_bin: None,
            claude_env: Vec::new(),
        }
    }

    pub fn db_path(&self) -> PathBuf {
        self.home.join(DB_FILE)
    }

    pub fn token_path(&self) -> PathBuf {
        self.home.join(TOKEN_FILE)
    }
}

/// OS-appropriate data directory, e.g. `~/Library/Application Support/dev.prowl.prowl`.
pub fn default_home() -> anyhow::Result<PathBuf> {
    directories::ProjectDirs::from("dev", "prowl", "prowl")
        .map(|d| d.data_local_dir().to_path_buf())
        .ok_or_else(|| anyhow!("could not determine a home directory for PRowl"))
}

pub fn read_token(home: &Path) -> anyhow::Result<String> {
    let path = home.join(TOKEN_FILE);
    let token = std::fs::read_to_string(&path)
        .with_context(|| format!("reading API token from {}", path.display()))?;
    Ok(token.trim().to_owned())
}

/// Returns the existing API token, creating one readable only by the current user if absent.
pub(crate) fn ensure_token(path: &Path) -> anyhow::Result<String> {
    if let Ok(existing) = std::fs::read_to_string(path) {
        let existing = existing.trim();
        if !existing.is_empty() {
            return Ok(existing.to_owned());
        }
    }
    let bytes: [u8; 32] = rand::random();
    let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();

    crate::credentials::write_private(path, &token)?;
    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_is_created_once_and_reused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(TOKEN_FILE);
        let first = ensure_token(&path).unwrap();
        let second = ensure_token(&path).unwrap();
        assert_eq!(first.len(), 64);
        assert_eq!(first, second);
        assert_eq!(read_token(dir.path()).unwrap(), first);
    }

    #[cfg(unix)]
    #[test]
    fn token_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(TOKEN_FILE);
        ensure_token(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
