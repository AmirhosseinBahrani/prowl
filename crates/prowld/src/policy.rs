//! The Review Policy: Markdown the user writes describing which changes need a human.
//!
//! Policies live as files under `<home>/policies/` — `global.md`, plus optional per-repository
//! overrides at `repos/<owner>/<repo>.md` — so they can be edited in the app or any editor.
//! They are never read from the repository under review, so a PR cannot rewrite its own rules.
//! Every change, from the API or from disk, is recorded as a version.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use tokio::sync::Mutex;

use crate::events::{self, EventBus};

pub const DEFAULT_POLICY: &str = include_str!("default_policy.md");
const MAX_POLICY_BYTES: usize = 64 * 1024;

pub mod kind {
    pub const CHANGED: &str = "policy.changed";
}

/// Which policy: the global one, or an override for a single repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    Global,
    Repo { owner: String, repo: String },
}

impl Scope {
    /// Validates owner/repo so they are safe to use as path components.
    pub fn repo(owner: &str, repo: &str) -> Result<Self, PolicyError> {
        for part in [owner, repo] {
            let valid = !part.is_empty()
                && part.len() <= 100
                && !part.starts_with('.')
                && part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
            if !valid {
                return Err(PolicyError::InvalidRepo(format!("{owner}/{repo}")));
            }
        }
        Ok(Self::Repo {
            owner: owner.to_owned(),
            repo: repo.to_owned(),
        })
    }

    /// Parses `owner/repo`.
    pub fn parse_repo(full_name: &str) -> Result<Self, PolicyError> {
        let (owner, repo) = full_name
            .split_once('/')
            .ok_or_else(|| PolicyError::InvalidRepo(full_name.to_owned()))?;
        Self::repo(owner, repo)
    }

    /// The key stored with each version: `global` or `owner/repo`.
    pub fn key(&self) -> String {
        match self {
            Self::Global => "global".into(),
            Self::Repo { owner, repo } => format!("{owner}/{repo}"),
        }
    }

    fn path(&self, dir: &Path) -> PathBuf {
        match self {
            Self::Global => dir.join("global.md"),
            Self::Repo { owner, repo } => dir.join("repos").join(owner).join(format!("{repo}.md")),
        }
    }
}

/// A rule under "ALWAYS require human review", e.g. `- [AUTH] Authentication changes`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyRule {
    pub id: String,
    pub text: String,
}

/// The policy that applies to a review, with the version it came from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EffectivePolicy {
    /// `global` or `owner/repo`.
    pub scope: String,
    pub version_id: i64,
    pub content: String,
    pub rules: Vec<PolicyRule>,
}

/// One policy in effect, for listings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicySummary {
    /// `global` or `owner/repo`.
    pub scope: String,
    pub version_id: i64,
    pub rules: Vec<String>,
    /// The file to edit; PRowl picks up changes to it.
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PolicyVersion {
    pub id: i64,
    pub scope: String,
    pub source: String,
    pub created_at_ms: i64,
    pub content: String,
}

#[derive(Debug, thiserror::Error)]
pub enum PolicyError {
    #[error("invalid repository name {0:?}; expected owner/repo")]
    InvalidRepo(String),
    #[error("the policy is empty")]
    Empty,
    #[error("the policy is larger than {MAX_POLICY_BYTES} bytes")]
    TooLarge,
    #[error("no policy override exists for {0}")]
    NotFound(String),
    #[error("version {0} does not belong to this policy")]
    UnknownVersion(i64),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl From<sqlx::Error> for PolicyError {
    fn from(e: sqlx::Error) -> Self {
        Self::Internal(e.into())
    }
}

#[derive(Clone)]
pub(crate) struct PolicyStore {
    dir: PathBuf,
    db: SqlitePool,
    events: EventBus,
    /// Serialises file writes with disk syncs, so a sync never sees a file whose version
    /// has not been recorded yet.
    lock: Arc<Mutex<()>>,
}

impl PolicyStore {
    /// Opens the store, writing the default global policy on first run and recording any
    /// edits made to the files while the daemon was not running.
    pub(crate) async fn open(
        home: &Path,
        db: SqlitePool,
        events: EventBus,
    ) -> anyhow::Result<Self> {
        let store = Self {
            dir: home.join("policies"),
            db,
            events,
            lock: Arc::new(Mutex::new(())),
        };
        std::fs::create_dir_all(&store.dir)
            .with_context(|| format!("creating {}", store.dir.display()))?;
        let global = Scope::Global.path(&store.dir);
        if !global.exists() && store.latest(&Scope::Global).await?.is_none() {
            write_atomically(&global, DEFAULT_POLICY)?;
            store
                .record(&Scope::Global, DEFAULT_POLICY, "default")
                .await?;
        }
        store.sync_from_disk().await?;
        Ok(store)
    }

    /// The policy that applies to `repo` (its override if one exists, else the global one).
    pub(crate) async fn effective(
        &self,
        repo: Option<&Scope>,
    ) -> Result<EffectivePolicy, PolicyError> {
        if let Some(scope @ Scope::Repo { .. }) = repo
            && let Some(policy) = self.current(scope).await?
        {
            return Ok(policy);
        }
        self.current(&Scope::Global)
            .await?
            .ok_or_else(|| PolicyError::NotFound("global".into()))
    }

    /// The current policy for exactly this scope, picking up on-disk edits first.
    pub(crate) async fn current(
        &self,
        scope: &Scope,
    ) -> Result<Option<EffectivePolicy>, PolicyError> {
        let _guard = self.lock.lock().await;
        self.sync_scope(scope).await?;
        if !scope.path(&self.dir).exists() {
            return Ok(None);
        }
        Ok(self.latest(scope).await?.map(|v| EffectivePolicy {
            rules: parse_rules(&v.content),
            scope: v.scope,
            version_id: v.id,
            content: v.content,
        }))
    }

    pub(crate) async fn save(
        &self,
        scope: &Scope,
        content: &str,
    ) -> Result<EffectivePolicy, PolicyError> {
        let _guard = self.lock.lock().await;
        self.write(scope, content, "api").await
    }

    pub(crate) async fn restore(
        &self,
        scope: &Scope,
        version_id: i64,
    ) -> Result<EffectivePolicy, PolicyError> {
        let content: Option<String> = sqlx::query_scalar(
            "SELECT content FROM policy_versions WHERE id = ? AND scope = ? AND source != 'delete'",
        )
        .bind(version_id)
        .bind(scope.key())
        .fetch_optional(&self.db)
        .await?;
        let content = content.ok_or(PolicyError::UnknownVersion(version_id))?;
        let _guard = self.lock.lock().await;
        self.write(scope, &content, "restore").await
    }

    /// Removes a repository override; that repository falls back to the global policy.
    pub(crate) async fn delete(&self, scope: &Scope) -> Result<(), PolicyError> {
        let Scope::Repo { .. } = scope else {
            return Err(PolicyError::NotFound(
                "cannot delete the global policy".into(),
            ));
        };
        let _guard = self.lock.lock().await;
        let path = scope.path(&self.dir);
        if !path.exists() {
            // Deleting is idempotent: an override that is already gone is fine; one that
            // never existed is not.
            return match self.latest_any(scope).await? {
                Some(v) if v.source == "delete" => Ok(()),
                _ => Err(PolicyError::NotFound(scope.key())),
            };
        }
        std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
        self.record(scope, "", "delete").await?;
        Ok(())
    }

    pub(crate) async fn versions(&self, scope: &Scope) -> Result<Vec<PolicyVersion>, PolicyError> {
        let _guard = self.lock.lock().await;
        self.sync_scope(scope).await?;
        Ok(sqlx::query_as::<_, (i64, String, String, i64, String)>(
            "SELECT id, scope, source, created_at_ms, content FROM policy_versions
             WHERE scope = ? ORDER BY id DESC",
        )
        .bind(scope.key())
        .fetch_all(&self.db)
        .await?
        .into_iter()
        .map(
            |(id, scope, source, created_at_ms, content)| PolicyVersion {
                id,
                scope,
                source,
                created_at_ms,
                content,
            },
        )
        .collect())
    }

    /// Records a version for every policy file whose content differs from its latest version.
    /// Global plus every override that exists on disk or has a recorded history.
    async fn known_scopes(&self) -> Result<Vec<Scope>, PolicyError> {
        let mut scopes = vec![Scope::Global];
        let repos = self.dir.join("repos");
        for owner in read_dir_names(&repos) {
            for file in read_dir_names(&repos.join(&owner)) {
                if let Some(repo) = file.strip_suffix(".md")
                    && let Ok(scope) = Scope::repo(&owner, repo)
                {
                    scopes.push(scope);
                }
            }
        }
        // Overrides deleted on disk while we weren't looking.
        let known: Vec<String> = sqlx::query_scalar(
            "SELECT DISTINCT scope FROM policy_versions WHERE scope != 'global'",
        )
        .fetch_all(&self.db)
        .await?;
        for key in known {
            if let Ok(scope) = Scope::parse_repo(&key)
                && !scopes.contains(&scope)
            {
                scopes.push(scope);
            }
        }
        Ok(scopes)
    }

    /// Every policy currently in effect somewhere: the global one and each override.
    pub(crate) async fn list(&self) -> Result<Vec<PolicySummary>, PolicyError> {
        let _guard = self.lock.lock().await;
        let mut out = Vec::new();
        for scope in self.known_scopes().await? {
            self.sync_scope(&scope).await?;
            let path = scope.path(&self.dir);
            if !path.exists() {
                continue;
            }
            if let Some(latest) = self.latest(&scope).await? {
                out.push(PolicySummary {
                    scope: scope.key(),
                    version_id: latest.id,
                    rules: parse_rules(&latest.content)
                        .into_iter()
                        .map(|r| r.id)
                        .collect(),
                    path: path.display().to_string(),
                });
            }
        }
        Ok(out)
    }

    pub(crate) async fn sync_from_disk(&self) -> Result<(), PolicyError> {
        let _guard = self.lock.lock().await;
        let scopes = self.known_scopes().await?;
        for scope in &scopes {
            self.sync_scope(scope).await?;
        }
        Ok(())
    }

    /// Caller must hold `lock`.
    async fn sync_scope(&self, scope: &Scope) -> Result<(), PolicyError> {
        let path = scope.path(&self.dir);
        let on_disk = match std::fs::read_to_string(&path) {
            Ok(content) => Some(content),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                return Err(anyhow::Error::from(e)
                    .context(format!("reading {}", path.display()))
                    .into());
            }
        };
        let latest = self.latest_any(scope).await?;
        match (on_disk, latest) {
            (Some(content), Some(v)) if v.source != "delete" && v.content == content => {}
            (Some(content), _) => {
                self.record(scope, &content, "file").await?;
            }
            // The global policy must always exist: restore the file from its latest version.
            (None, latest) if *scope == Scope::Global => {
                let content = latest.map_or_else(|| DEFAULT_POLICY.to_owned(), |v| v.content);
                write_atomically(&path, &content)?;
                self.record(scope, &content, "restore").await?;
            }
            (None, Some(v)) if v.source != "delete" => {
                self.record(scope, "", "delete").await?;
            }
            (None, _) => {}
        }
        Ok(())
    }

    /// Caller must hold `lock`.
    async fn write(
        &self,
        scope: &Scope,
        content: &str,
        source: &str,
    ) -> Result<EffectivePolicy, PolicyError> {
        if content.trim().is_empty() {
            return Err(PolicyError::Empty);
        }
        if content.len() > MAX_POLICY_BYTES {
            return Err(PolicyError::TooLarge);
        }
        write_atomically(&scope.path(&self.dir), content)?;
        let id = self.record(scope, content, source).await?;
        Ok(EffectivePolicy {
            scope: scope.key(),
            version_id: id,
            content: content.to_owned(),
            rules: parse_rules(content),
        })
    }

    async fn record(&self, scope: &Scope, content: &str, source: &str) -> anyhow::Result<i64> {
        let id = sqlx::query(
            "INSERT INTO policy_versions (scope, content, source, created_at_ms) VALUES (?, ?, ?, ?)",
        )
        .bind(scope.key())
        .bind(content)
        .bind(source)
        .bind(events::now_ms())
        .execute(&self.db)
        .await?
        .last_insert_rowid();
        self.events
            .emit(
                kind::CHANGED,
                serde_json::json!({ "scope": scope.key(), "version_id": id, "source": source }),
            )
            .await?;
        Ok(id)
    }

    /// Latest non-deleted version.
    async fn latest(&self, scope: &Scope) -> anyhow::Result<Option<PolicyVersion>> {
        Ok(self
            .latest_any(scope)
            .await?
            .filter(|v| v.source != "delete"))
    }

    async fn latest_any(&self, scope: &Scope) -> anyhow::Result<Option<PolicyVersion>> {
        Ok(sqlx::query_as::<_, (i64, String, String, i64, String)>(
            "SELECT id, scope, source, created_at_ms, content FROM policy_versions
             WHERE scope = ? ORDER BY id DESC LIMIT 1",
        )
        .bind(scope.key())
        .fetch_optional(&self.db)
        .await?
        .map(
            |(id, scope, source, created_at_ms, content)| PolicyVersion {
                id,
                scope,
                source,
                created_at_ms,
                content,
            },
        ))
    }
}

/// Extracts `[ID] text` list items under the "ALWAYS require human review" heading.
pub fn parse_rules(content: &str) -> Vec<PolicyRule> {
    let mut rules = Vec::new();
    let mut in_always = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(heading) = trimmed.strip_prefix('#') {
            in_always = heading
                .trim_start_matches('#')
                .trim()
                .to_ascii_uppercase()
                .starts_with("ALWAYS");
            continue;
        }
        if !in_always {
            continue;
        }
        let Some(item) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
        else {
            continue;
        };
        let Some(rest) = item.trim_start().strip_prefix('[') else {
            continue;
        };
        let Some((id, text)) = rest.split_once(']') else {
            continue;
        };
        let id = id.trim();
        if !id.is_empty()
            && id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            && !rules.iter().any(|r: &PolicyRule| r.id == id)
        {
            rules.push(PolicyRule {
                id: id.to_owned(),
                text: text.trim().to_owned(),
            });
        }
    }
    rules
}

fn read_dir_names(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter_map(|e| e.file_name().into_string().ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Writes via a temporary file and rename, so readers never see a half-written policy.
fn write_atomically(path: &Path, content: &str) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("md.tmp");
    std::fs::write(&tmp, content).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_has_the_six_starter_rules() {
        let ids: Vec<_> = parse_rules(DEFAULT_POLICY)
            .into_iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(ids, ["AUTH", "MONEY", "DATA", "INFRA", "SECRETS", "DEPS"]);
    }

    #[test]
    fn only_rules_under_the_always_heading_count() {
        let policy = "\
## ALWAYS require human review
- [AUTH] auth changes
* [PAY_2] payments
- no id here
- [AUTH] duplicate ignored
## Agent may handle alone
- [DOCS] docs
### always (lowercase heading still counts)
- [LOW-1] lower
";
        assert_eq!(
            parse_rules(policy),
            [
                PolicyRule {
                    id: "AUTH".into(),
                    text: "auth changes".into()
                },
                PolicyRule {
                    id: "PAY_2".into(),
                    text: "payments".into()
                },
                PolicyRule {
                    id: "LOW-1".into(),
                    text: "lower".into()
                },
            ]
        );
    }

    #[test]
    fn repo_scopes_reject_path_tricks() {
        for bad in ["../x", "a/..", "a/b/c", "a/", "/b", "a/.hidden", "a b/c"] {
            assert!(Scope::parse_repo(bad).is_err(), "{bad} should be rejected");
        }
        assert_eq!(
            Scope::parse_repo("octo-org/my_repo.rs").unwrap().key(),
            "octo-org/my_repo.rs"
        );
    }
}
