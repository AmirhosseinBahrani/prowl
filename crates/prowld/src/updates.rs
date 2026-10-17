//! Checks GitHub for a newer PRowl release, so the TUI can suggest updating.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::auth::Auth;
use crate::events::EventBus;
use crate::github::GitHub;

pub mod kind {
    /// A newer release exists: `current`, `latest`, `url`.
    pub const AVAILABLE: &str = "update.available";
}

/// How PRowl's version compares with the latest release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateInfo {
    pub current: String,
    /// The latest release's tag, e.g. `v0.1.3`.
    pub latest: String,
    /// Its release page.
    pub url: String,
    /// `latest` is newer than `current`.
    pub available: bool,
}

/// Where and how often to check.
pub(crate) struct CheckSettings {
    /// `owner/repo` whose releases are PRowl's.
    pub repo: String,
    pub interval: Duration,
    /// Used instead of `interval` when a check couldn't run.
    pub retry: Duration,
}

#[derive(Clone, Default)]
pub(crate) struct Updates {
    latest: Arc<Mutex<Option<UpdateInfo>>>,
}

impl Updates {
    pub(crate) fn get(&self) -> Option<UpdateInfo> {
        self.latest.lock().unwrap().clone()
    }

    /// Checks `repo`'s latest release every `interval` (retrying sooner while there is no
    /// connected account or GitHub can't be reached), and announces each newer version once.
    pub(crate) fn spawn(
        &self,
        github: GitHub,
        auth: Auth,
        events: EventBus,
        check: CheckSettings,
        shutdown: CancellationToken,
    ) {
        let this = self.clone();
        tokio::spawn(async move {
            let mut announced: Option<String> = None;
            loop {
                let wait = match this.check(&github, &auth, &check.repo).await {
                    Some(info) => {
                        if info.available && announced.as_ref() != Some(&info.latest) {
                            announced = Some(info.latest.clone());
                            let _ = events
                                .emit(
                                    kind::AVAILABLE,
                                    serde_json::to_value(&info).unwrap_or_default(),
                                )
                                .await;
                        }
                        check.interval
                    }
                    None => check.retry,
                };
                tokio::select! {
                    () = tokio::time::sleep(wait) => {}
                    () = shutdown.cancelled() => return,
                }
            }
        });
    }

    async fn check(&self, github: &GitHub, auth: &Auth, repo: &str) -> Option<UpdateInfo> {
        let token = auth.token().await?;
        let release = match github.latest_release(&token, repo).await {
            Ok(release) => release?,
            Err(e) => {
                tracing::debug!(error = %e, "could not check for a newer PRowl release");
                return None;
            }
        };
        let current = env!("CARGO_PKG_VERSION").to_owned();
        let info = UpdateInfo {
            available: is_newer(&release.tag_name, &current),
            current,
            latest: release.tag_name,
            url: release.html_url,
        };
        *self.latest.lock().unwrap() = Some(info.clone());
        Some(info)
    }
}

/// Whether version `latest` (e.g. `v0.2.0`) is newer than `current` (e.g. `0.1.9`). Pre-release
/// suffixes are ignored; anything unparsable is never newer.
pub(crate) fn is_newer(latest: &str, current: &str) -> bool {
    fn parts(v: &str) -> Option<Vec<u64>> {
        let core = v.trim().trim_start_matches('v').split(['-', '+']).next()?;
        core.split('.').map(|p| p.parse().ok()).collect()
    }
    match (parts(latest), parts(current)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_numerically() {
        assert!(is_newer("v0.1.10", "0.1.9"));
        assert!(is_newer("v0.2.0", "0.1.1"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(!is_newer("v0.1.1", "0.1.1"));
        assert!(!is_newer("v0.1.0", "0.1.1"));
        assert!(!is_newer("v0.2.0-rc.1", "0.2.0"));
        assert!(!is_newer("nightly", "0.1.1"));
    }
}
