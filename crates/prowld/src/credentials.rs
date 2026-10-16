//! Storage for the GitHub token: OS credential store (Keychain, Secret Service, Credential
//! Manager), or a file readable only by the current user when no credential store is available.

use std::path::PathBuf;

use anyhow::Context;

use crate::config::CredentialBackend;

const SERVICE: &str = "dev.prowl.prowl";
const ACCOUNT: &str = "github-token";
const FILE_NAME: &str = "github-token";

#[derive(Debug, Clone)]
pub(crate) enum CredentialStore {
    Keyring,
    File(PathBuf),
}

impl CredentialStore {
    pub(crate) async fn open(backend: CredentialBackend, home: PathBuf) -> Self {
        let file = Self::File(home.join(FILE_NAME));
        if backend == CredentialBackend::File {
            return file;
        }
        let usable = tokio::task::spawn_blocking(|| match entry().and_then(|e| e.get_password()) {
            Ok(_) | Err(keyring::Error::NoEntry) => true,
            Err(e) => {
                tracing::warn!(error = %e, "OS credential store unavailable; using a private file");
                false
            }
        })
        .await
        .unwrap_or(false);
        if usable { Self::Keyring } else { file }
    }

    pub(crate) fn describe(&self) -> &'static str {
        match self {
            Self::Keyring => "os-credential-store",
            Self::File(_) => "private-file",
        }
    }

    pub(crate) async fn get(&self) -> anyhow::Result<Option<String>> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || match this {
            Self::Keyring => match entry()?.get_password() {
                Ok(token) => Ok(Some(token)),
                Err(keyring::Error::NoEntry) => Ok(None),
                Err(e) => Err(e).context("reading GitHub token from the credential store"),
            },
            Self::File(path) => match std::fs::read_to_string(&path) {
                Ok(token) => Ok(Some(token.trim().to_owned()).filter(|t| !t.is_empty())),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
            },
        })
        .await?
    }

    pub(crate) async fn set(&self, token: String) -> anyhow::Result<()> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || match this {
            Self::Keyring => entry()?
                .set_password(&token)
                .context("saving GitHub token to the credential store"),
            Self::File(path) => write_private(&path, &token),
        })
        .await?
    }

    pub(crate) async fn delete(&self) -> anyhow::Result<()> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || match this {
            Self::Keyring => match entry()?.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                Err(e) => Err(e).context("removing GitHub token from the credential store"),
            },
            Self::File(path) => match std::fs::remove_file(&path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                    Err(e).with_context(|| format!("removing {}", path.display()))
                }
                _ => Ok(()),
            },
        })
        .await?
    }
}

fn entry() -> keyring::Result<keyring::Entry> {
    keyring::Entry::new(SERVICE, ACCOUNT)
}

pub(crate) fn write_private(path: &std::path::Path, contents: &str) -> anyhow::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .and_then(|mut f| f.write_all(contents.as_bytes()))
        .with_context(|| format!("writing {}", path.display()))
}
