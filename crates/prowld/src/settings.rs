//! Settings the user can change while the daemon runs. Stored as overrides over the defaults
//! from [`Config`], and published to every component through a watch channel so changes take
//! effect without a restart.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use tokio::sync::{Mutex, watch};

use crate::config::Config;
use crate::events::{self, EventBus};
use crate::power::SleepMode;

/// Where the daily budget's day starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetDay {
    Utc,
    /// Midnight in the machine's time zone.
    Local,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub poll_interval_secs: u64,
    pub review_concurrency: usize,
    /// `None`: the claude CLI's default model.
    pub claude_model: Option<String>,
    pub review_timeout_secs: u64,
    pub review_budget_usd: Option<f64>,
    pub daily_budget_usd: Option<f64>,
    pub budget_day: BudgetDay,
    pub risk_threshold: u8,
    pub max_changed_lines: u64,
    pub max_changed_files: u64,
    /// `None`: `ac_only` with a battery, `always` without.
    pub sleep_mode: Option<SleepMode>,
    pub skip_drafts: bool,
    pub include_team_requests: bool,
    pub notifications: bool,
    /// Per repository (`owner/repo`), glob patterns of files that always need a human.
    pub file_rules: BTreeMap<String, Vec<String>>,
}

impl Settings {
    pub fn from_config(config: &Config) -> Self {
        Self {
            poll_interval_secs: config.poll_interval.as_secs().max(1),
            review_concurrency: config.review_concurrency,
            claude_model: config.claude_model.clone(),
            review_timeout_secs: config.review_timeout.as_secs().max(1),
            review_budget_usd: config.review_budget_usd,
            daily_budget_usd: config.daily_budget_usd,
            budget_day: BudgetDay::Utc,
            risk_threshold: config.risk_threshold,
            max_changed_lines: config.max_changed_lines,
            max_changed_files: config.max_changed_files,
            sleep_mode: config.sleep_mode,
            skip_drafts: config.skip_drafts,
            include_team_requests: config.include_team_requests,
            notifications: config.notifications,
            file_rules: BTreeMap::new(),
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        let check = |ok: bool, message: &str| if ok { Ok(()) } else { Err(message.to_owned()) };
        check(
            (10..=86_400).contains(&self.poll_interval_secs),
            "poll_interval_secs must be 10–86400",
        )?;
        check(
            (1..=16).contains(&self.review_concurrency),
            "review_concurrency must be 1–16",
        )?;
        check(
            (30..=7_200).contains(&self.review_timeout_secs),
            "review_timeout_secs must be 30–7200",
        )?;
        check(
            (1..=100).contains(&self.risk_threshold),
            "risk_threshold must be 1–100",
        )?;
        check(
            self.max_changed_lines >= 1,
            "max_changed_lines must be at least 1",
        )?;
        check(
            self.max_changed_files >= 1,
            "max_changed_files must be at least 1",
        )?;
        for (name, value) in [
            ("review_budget_usd", self.review_budget_usd),
            ("daily_budget_usd", self.daily_budget_usd),
        ] {
            if let Some(v) = value {
                check(
                    v.is_finite() && v > 0.0,
                    &format!("{name} must be more than 0"),
                )?;
            }
        }
        if let Some(model) = &self.claude_model {
            check(
                !model.trim().is_empty()
                    && !model.starts_with('-')
                    && !model.contains(char::is_whitespace),
                "claude_model must be a model name",
            )?;
        }
        for (repo, patterns) in &self.file_rules {
            crate::policy::Scope::parse_repo(repo).map_err(|e| format!("file_rules: {e}"))?;
            for pattern in patterns {
                globset::Glob::new(pattern).map_err(|e| format!("file_rules for {repo}: {e}"))?;
            }
        }
        Ok(())
    }

    pub(crate) fn poll_interval(&self) -> Duration {
        Duration::from_secs(self.poll_interval_secs)
    }

    pub(crate) fn review_timeout(&self) -> Duration {
        Duration::from_secs(self.review_timeout_secs)
    }

    /// Start (Unix ms) of the budget day containing `now_ms`.
    pub(crate) fn day_start_ms(&self, now_ms: i64) -> i64 {
        const DAY_MS: i64 = 86_400_000;
        match self.budget_day {
            BudgetDay::Utc => now_ms - now_ms.rem_euclid(DAY_MS),
            BudgetDay::Local => {
                use chrono::{Local, TimeZone};
                let Some(now) = Local.timestamp_millis_opt(now_ms).single() else {
                    return now_ms - now_ms.rem_euclid(DAY_MS);
                };
                now.date_naive()
                    .and_hms_opt(0, 0, 0)
                    .and_then(|midnight| Local.from_local_datetime(&midnight).earliest())
                    .map_or(now_ms - now_ms.rem_euclid(DAY_MS), |m| m.timestamp_millis())
            }
        }
    }

    /// File patterns (as written) for `repo` that match `file`.
    pub(crate) fn matching_rule(&self, repo: &str, files: &[String]) -> Option<(String, String)> {
        let patterns = self
            .file_rules
            .iter()
            .find(|(r, _)| r.eq_ignore_ascii_case(repo))
            .map(|(_, p)| p)?;
        for pattern in patterns {
            let Ok(glob) = globset::Glob::new(pattern) else {
                continue;
            };
            let matcher = glob.compile_matcher();
            if let Some(file) = files.iter().find(|f| matcher.is_match(f)) {
                return Some((pattern.clone(), file.clone()));
            }
        }
        None
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

#[derive(Clone)]
pub(crate) struct SettingsStore {
    db: SqlitePool,
    events: EventBus,
    defaults: Arc<Settings>,
    tx: Arc<watch::Sender<Settings>>,
    write: Arc<Mutex<()>>,
}

pub mod kind {
    pub const CHANGED: &str = "settings.changed";
}

impl SettingsStore {
    pub(crate) async fn open(
        db: SqlitePool,
        events: EventBus,
        defaults: Settings,
    ) -> anyhow::Result<Self> {
        let stored: Option<String> = sqlx::query_scalar("SELECT json FROM settings WHERE id = 1")
            .fetch_optional(&db)
            .await?;
        let current = match stored {
            Some(json) => match merge(&defaults, &serde_json::from_str(&json)?) {
                Ok(settings) => settings,
                Err(e) => {
                    tracing::warn!(error = %e, "stored settings are invalid; using defaults");
                    defaults.clone()
                }
            },
            None => defaults.clone(),
        };
        let (tx, _) = watch::channel(current);
        Ok(Self {
            db,
            events,
            defaults: Arc::new(defaults),
            tx: Arc::new(tx),
            write: Arc::default(),
        })
    }

    pub(crate) fn get(&self) -> Settings {
        self.tx.borrow().clone()
    }

    pub(crate) fn subscribe(&self) -> watch::Receiver<Settings> {
        self.tx.subscribe()
    }

    /// Applies a partial update (any subset of fields), validates, stores and publishes it.
    pub(crate) async fn update(
        &self,
        patch: &serde_json::Value,
    ) -> Result<Settings, SettingsError> {
        let _guard = self.write.lock().await;
        let next = merge(&self.get(), patch).map_err(SettingsError::Invalid)?;
        let overrides =
            diff(&self.defaults, &next).map_err(|e| SettingsError::Internal(anyhow::anyhow!(e)))?;
        sqlx::query(
            "INSERT INTO settings (id, json, updated_at_ms) VALUES (1, ?, ?)
             ON CONFLICT (id) DO UPDATE SET json = excluded.json, updated_at_ms = excluded.updated_at_ms",
        )
        .bind(overrides.to_string())
        .bind(events::now_ms())
        .execute(&self.db)
        .await
        .map_err(|e| SettingsError::Internal(e.into()))?;
        self.tx.send_replace(next.clone());
        let changed: Vec<&String> = patch
            .as_object()
            .map(|o| o.keys().collect())
            .unwrap_or_default();
        let _ = self
            .events
            .emit(kind::CHANGED, serde_json::json!({ "changed": changed }))
            .await;
        Ok(next)
    }
}

/// `base` with the fields in `patch` replaced, validated.
fn merge(base: &Settings, patch: &serde_json::Value) -> Result<Settings, String> {
    let patch = patch.as_object().ok_or("settings must be a JSON object")?;
    let mut value = serde_json::to_value(base).map_err(|e| e.to_string())?;
    let object = value
        .as_object_mut()
        .expect("settings serialise to an object");
    for (key, v) in patch {
        if !object.contains_key(key) {
            return Err(format!("unknown setting {key:?}"));
        }
        object.insert(key.clone(), v.clone());
    }
    let settings: Settings =
        serde_json::from_value(value).map_err(|e| format!("invalid settings: {e}"))?;
    settings.validate()?;
    Ok(settings)
}

/// The fields of `current` that differ from `defaults`, so defaults can change in new versions.
fn diff(defaults: &Settings, current: &Settings) -> Result<serde_json::Value, String> {
    let d = serde_json::to_value(defaults).map_err(|e| e.to_string())?;
    let c = serde_json::to_value(current).map_err(|e| e.to_string())?;
    let mut out = serde_json::Map::new();
    if let (Some(d), Some(c)) = (d.as_object(), c.as_object()) {
        for (key, value) in c {
            if d.get(key) != Some(value) {
                out.insert(key.clone(), value.clone());
            }
        }
    }
    Ok(serde_json::Value::Object(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defaults() -> Settings {
        Settings::from_config(&Config::new("/tmp/x"))
    }

    #[test]
    fn partial_updates_merge_and_validate() {
        let s = merge(
            &defaults(),
            &serde_json::json!({ "risk_threshold": 40, "daily_budget_usd": 3.5 }),
        )
        .unwrap();
        assert_eq!((s.risk_threshold, s.daily_budget_usd), (40, Some(3.5)));
        assert_eq!(
            s.poll_interval_secs, 60,
            "untouched fields keep their value"
        );

        for bad in [
            serde_json::json!({ "risk_threshold": 0 }),
            serde_json::json!({ "review_concurrency": 99 }),
            serde_json::json!({ "daily_budget_usd": -1 }),
            serde_json::json!({ "claude_model": "--dangerously-skip-permissions" }),
            serde_json::json!({ "file_rules": { "acme/api": ["[unclosed"] } }),
            serde_json::json!({ "file_rules": { "../x": ["*.rs"] } }),
            serde_json::json!({ "no_such_setting": true }),
        ] {
            assert!(
                merge(&defaults(), &bad).is_err(),
                "{bad} should be rejected"
            );
        }
    }

    #[test]
    fn only_changes_from_the_defaults_are_stored() {
        let mut changed = defaults();
        changed.skip_drafts = false;
        assert_eq!(
            diff(&defaults(), &changed).unwrap(),
            serde_json::json!({ "skip_drafts": false })
        );
    }

    #[test]
    fn file_rules_match_globs() {
        let mut s = defaults();
        s.file_rules.insert(
            "acme/api".into(),
            vec!["migrations/**".into(), "**/*.sql".into()],
        );
        let files = vec!["src/lib.rs".to_owned(), "db/schema.sql".to_owned()];
        assert_eq!(
            s.matching_rule("Acme/API", &files),
            Some(("**/*.sql".into(), "db/schema.sql".into()))
        );
        assert_eq!(s.matching_rule("acme/web", &files), None);
    }

    #[test]
    fn budget_days_start_at_midnight() {
        let s = defaults();
        assert_eq!(s.day_start_ms(86_400_000 * 3 + 5_000), 86_400_000 * 3);
        let local = Settings {
            budget_day: BudgetDay::Local,
            ..defaults()
        };
        let now = 1_792_163_045_000;
        let start = local.day_start_ms(now);
        assert!(start <= now && now - start < 86_400_000 + 3_600_000);
    }
}
