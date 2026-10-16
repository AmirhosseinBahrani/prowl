use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow};

pub const TOKEN_FILE: &str = "api-token";
pub const DB_FILE: &str = "prowl.db";

/// Everything the daemon needs to start. All on-disk state lives under `home`.
#[derive(Debug, Clone)]
pub struct Config {
    pub home: PathBuf,
    pub github_api_url: String,
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

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    use std::io::Write;
    options
        .open(path)
        .and_then(|mut f| f.write_all(token.as_bytes()))
        .with_context(|| format!("writing API token to {}", path.display()))?;
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
