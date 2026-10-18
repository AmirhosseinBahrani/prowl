//! Policy learning: PRowl improves each repository's Review Policy from what its collaborators
//! say and do about PRowl's reviews — replies to its threads, issues they raised that it
//! missed, and how the user resolved the pull requests it sent them — and applies the result
//! as a new, revertible policy version (`learned`).
//!
//! Only collaborators count (GitHub's author association OWNER, MEMBER or COLLABORATOR), text
//! that looks like prompt injection is dropped, a repository's feedback only changes that
//! repository's policy, and a policy without any rule left is refused.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::auth::{Auth, AuthState};
use crate::claude::{Claude, RunOptions};
use crate::events::{self, EventBus};
use crate::github::GitHub;
use crate::policy::{PolicyStore, Scope};
use crate::repos::Repos;
use crate::reviews::Reviews;
use crate::settings::SettingsStore;

pub mod kind {
    /// A learning run changed a repository's policy: `repo`, `version_id`, `changes`, `signals`.
    pub const LEARNED: &str = "policy.learned";
}

/// How far back the first run of a repository looks.
const FIRST_LOOKBACK_MS: i64 = 30 * 86_400_000;
/// Feedback kept per run, and per item.
const MAX_SIGNALS: usize = 150;
const MAX_TEXT: usize = 600;
const UNUSABLE: &str = "the agent's answer was unusable; its feedback is read again next time";
/// What PRowl writes into its own posts (never feedback about itself).
const PROWL_MARKERS: [&str; 2] = ["PRowl automated review", "<!-- prowl:review"];

/// One piece of feedback about PRowl's reviews.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Signal {
    /// `reply_to_finding`, `finding_resolved`, `issue_raised`, `review_comment`,
    /// `approved_although_escalated` or `changes_requested_after_escalation`.
    pub kind: &'static str,
    pub pr: u64,
    pub author: String,
    /// What it's about: the finding, a file and line, or why PRowl escalated.
    pub about: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyChange {
    /// `add_rule`, `tighten_rule`, `relax_rule`, `remove_rule` or `guidance`.
    pub kind: String,
    pub summary: String,
    #[serde(default)]
    pub evidence: Vec<String>,
}

/// What one learning run did.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LearningRun {
    pub repo: String,
    pub ran_at_ms: i64,
    /// Pieces of feedback it read.
    pub signals: usize,
    pub applied: bool,
    /// The policy version it saved, when it changed the policy.
    pub version_id: Option<i64>,
    pub changes: Vec<PolicyChange>,
    pub cost_usd: Option<f64>,
    /// Why nothing was applied, if nothing was.
    pub note: Option<String>,
}

#[derive(Deserialize)]
struct Answer {
    changed: bool,
    policy: String,
    #[serde(default)]
    changes: Vec<PolicyChange>,
}

#[derive(Clone)]
pub(crate) struct Learner {
    inner: Arc<Inner>,
}

struct Inner {
    github: GitHub,
    auth: Auth,
    repos: Repos,
    policies: PolicyStore,
    reviews: Reviews,
    db: SqlitePool,
    claude: Arc<Claude>,
    settings: SettingsStore,
    events: EventBus,
    /// Scratch directory Claude runs in (it needs no files).
    workdir: PathBuf,
    /// One run at a time.
    running: Mutex<()>,
}

pub(crate) struct LearnerParts {
    pub github: GitHub,
    pub auth: Auth,
    pub repos: Repos,
    pub policies: PolicyStore,
    pub reviews: Reviews,
    pub db: SqlitePool,
    pub claude: Arc<Claude>,
    pub settings: SettingsStore,
    pub events: EventBus,
    pub workdir: PathBuf,
}

impl Learner {
    pub(crate) fn new(parts: LearnerParts) -> Self {
        Self {
            inner: Arc::new(Inner {
                github: parts.github,
                auth: parts.auth,
                repos: parts.repos,
                policies: parts.policies,
                reviews: parts.reviews,
                db: parts.db,
                claude: parts.claude,
                settings: parts.settings,
                events: parts.events,
                workdir: parts.workdir,
                running: Mutex::new(()),
            }),
        }
    }

    /// Learns for every watched repository every `interval`, while the setting is on.
    pub(crate) fn spawn(&self, interval: Duration, shutdown: CancellationToken) {
        let this = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = tokio::time::sleep(interval) => {}
                    () = shutdown.cancelled() => return,
                }
                if !this.inner.settings.get().policy_learning {
                    continue;
                }
                if let Err(e) = this.learn_all().await {
                    tracing::warn!(error = %format!("{e:#}"), "policy learning failed");
                }
            }
        });
    }

    /// Learns for every watched repository.
    pub(crate) async fn learn_all(&self) -> anyhow::Result<Vec<LearningRun>> {
        let repos = self
            .inner
            .repos
            .watched()
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut runs = Vec::new();
        for repo in repos {
            runs.push(self.learn(&repo.full_name).await?);
        }
        Ok(runs)
    }

    /// Reads the feedback on `repo` since its last run and, if there is any, updates its policy.
    pub(crate) async fn learn(&self, repo: &str) -> anyhow::Result<LearningRun> {
        let inner = &self.inner;
        let _one = inner.running.lock().await;
        let scope = Scope::parse_repo(repo)?;
        let AuthState::Connected { login, .. } = inner.auth.state().await else {
            anyhow::bail!("connect a GitHub account first");
        };
        let token = inner
            .auth
            .token()
            .await
            .ok_or_else(|| anyhow::anyhow!("connect a GitHub account first"))?;
        let now = events::now_ms();
        let since = self
            .last_run(repo)
            .await?
            .unwrap_or(now - FIRST_LOOKBACK_MS);
        let mut run = LearningRun {
            repo: repo.to_owned(),
            ran_at_ms: now,
            signals: 0,
            applied: false,
            version_id: None,
            changes: Vec::new(),
            cost_usd: None,
            note: None,
        };

        let signals = self.feedback(repo, &login, &token, since).await?;
        run.signals = signals.len();
        if signals.is_empty() {
            run.note = Some("no new feedback".into());
            self.save_run(&run).await?;
            return Ok(run);
        }
        let settings = inner.settings.get();
        if let Some(cap) = settings.daily_budget_usd {
            let (spent, _) = inner
                .reviews
                .spent_since(settings.day_start_ms(now))
                .await?;
            if spent >= cap {
                run.note = Some("today's Claude budget is used up".into());
                return Ok(run);
            }
        }

        let policy = inner.policies.effective(Some(&scope)).await?;
        let feedback = serde_json::to_string_pretty(&signals)?;
        let message = crate::prompt::learning_message(repo, &login, &policy, &feedback);
        let dir = inner.workdir.join(repo.replace('/', "__"));
        std::fs::create_dir_all(&dir)?;
        let answer = inner
            .claude
            .run(
                &dir,
                &dir,
                &message,
                None,
                &RunOptions {
                    model: settings.claude_model.clone(),
                    timeout: settings.review_timeout(),
                    max_budget_usd: settings.review_budget_usd,
                },
                &crate::prompt::learning_schema(),
                |_| {},
            )
            .await;
        let _ = std::fs::remove_dir_all(&dir);
        let answer = answer?;
        run.cost_usd = answer.cost_usd;
        // Its cost counts toward the daily budget, like a dry run's.
        inner
            .reviews
            .record_dry_run(repo, 0, answer.cost_usd)
            .await?;

        let parsed: Option<Answer> = answer
            .structured
            .clone()
            .and_then(|v| serde_json::from_value(v).ok())
            .or_else(|| {
                let start = answer.text.find('{')?;
                let end = answer.text.rfind('}')?;
                serde_json::from_str(answer.text.get(start..=end)?).ok()
            });
        let Some(parsed) = parsed else {
            run.note = Some(UNUSABLE.into());
            self.save_run(&run).await?;
            return Ok(run);
        };
        run.changes = parsed.changes;
        let new_policy = parsed.policy.trim();
        if !parsed.changed || new_policy == policy.content.trim() {
            run.note = Some("the feedback didn't call for changes".into());
        } else if let Err(why) = acceptable(&policy.content, new_policy) {
            run.note = Some(why.into());
        } else {
            let saved = inner
                .policies
                .save_learned(&scope, &format!("{new_policy}\n"))
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            run.applied = true;
            run.version_id = Some(saved.version_id);
            let _ = inner
                .events
                .emit(
                    kind::LEARNED,
                    serde_json::json!({
                        "repo": repo,
                        "version_id": saved.version_id,
                        "signals": run.signals,
                        "changes": run.changes,
                    }),
                )
                .await;
        }
        self.save_run(&run).await?;
        Ok(run)
    }

    /// When the last run for `repo` started reading (its feedback is read from there on).
    async fn last_run(&self, repo: &str) -> anyhow::Result<Option<i64>> {
        let last: Option<i64> = sqlx::query_scalar(
            "SELECT MAX(ran_at_ms) FROM policy_learning WHERE repo = ? AND consumed = 1",
        )
        .bind(repo)
        .fetch_one(&self.inner.db)
        .await?;
        Ok(last)
    }

    async fn save_run(&self, run: &LearningRun) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO policy_learning (repo, ran_at_ms, signals, applied, version_id, changes,
                                          cost_usd, consumed)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&run.repo)
        .bind(run.ran_at_ms)
        .bind(run.signals as i64)
        .bind(run.applied)
        .bind(run.version_id)
        .bind(serde_json::to_string(&run.changes)?)
        .bind(run.cost_usd)
        .bind(run.note.as_deref() != Some(UNUSABLE))
        .execute(&self.inner.db)
        .await?;
        Ok(())
    }

    /// The collaborators' feedback on PRowl's reviews of `repo` since `since` (Unix ms).
    async fn feedback(
        &self,
        repo: &str,
        login: &str,
        token: &str,
        since: i64,
    ) -> anyhow::Result<Vec<Signal>> {
        let inner = &self.inner;
        let mut signals = Vec::new();
        let prs: Vec<i64> = sqlx::query_scalar(
            "SELECT DISTINCT j.pr_number FROM reviews r JOIN jobs j ON j.id = r.job_id
             WHERE j.repo = ? AND r.created_at_ms >= ? ORDER BY j.pr_number",
        )
        .bind(repo)
        .bind(events::now_ms() - FIRST_LOOKBACK_MS)
        .fetch_all(&inner.db)
        .await?;
        for pr in prs.into_iter().map(|n| n as u64) {
            for thread in inner.github.review_threads(token, repo, pr).await? {
                let Some(first) = thread.comments.first() else {
                    continue;
                };
                let place = match thread.line {
                    Some(line) => format!("{}:{line}", thread.path),
                    None => thread.path.clone(),
                };
                if is_prowl(&first.body) {
                    let finding = format!("{} — {place}", headline(&first.body));
                    for reply in thread.comments.iter().skip(1) {
                        if collaborator(&reply.association)
                            && after(&reply.created_at, since)
                            && !is_prowl(&reply.body)
                        {
                            signals.push(signal(
                                "reply_to_finding",
                                pr,
                                &reply.author,
                                &finding,
                                &reply.body,
                            ));
                        }
                    }
                    if thread.resolved && !thread.outdated && after(&first.created_at, since) {
                        signals.push(signal(
                            "finding_resolved_without_changing_that_code",
                            pr,
                            "",
                            &finding,
                            "",
                        ));
                    }
                } else if collaborator(&first.association) && after(&first.created_at, since) {
                    signals.push(signal(
                        "issue_raised",
                        pr,
                        &first.author,
                        &place,
                        &first.body,
                    ));
                }
            }
            for review in inner.github.list_reviews(token, repo, pr).await? {
                let body = review.body.as_deref().unwrap_or("").trim();
                if !body.is_empty()
                    && !is_prowl(body)
                    && collaborator(review.author_association.as_deref().unwrap_or(""))
                    && after(review.submitted_at.as_deref().unwrap_or(""), since)
                {
                    let author = review.user.as_ref().map(|u| u.login.as_str()).unwrap_or("");
                    signals.push(signal("review_comment", pr, author, &review.state, body));
                }
            }
        }
        // How the user resolved the pull requests PRowl sent them.
        let decided: Vec<(i64, String, Option<String>)> = sqlx::query_as(
            "SELECT q.pr_number, q.status, r.decision_reasons FROM human_queue q
             JOIN reviews r ON r.id = q.review_id
             WHERE q.repo = ? AND q.resolved_at_ms >= ?
               AND q.status IN ('approved', 'changes_requested')
               AND COALESCE(q.resolved_via, '') != 'verified'",
        )
        .bind(repo)
        .bind(since)
        .fetch_all(&inner.db)
        .await?;
        for (pr, status, reasons) in decided {
            let reasons: Vec<crate::decider::Reason> = reasons
                .as_deref()
                .and_then(|r| serde_json::from_str(r).ok())
                .unwrap_or_default();
            let why = crate::publish::describe(crate::decider::Decision::NeedsHuman, &reasons);
            let kind = if status == "approved" {
                "approved_although_escalated"
            } else {
                "changes_requested_after_escalation"
            };
            signals.push(signal(kind, pr as u64, login, &why, ""));
        }
        // Comments are untrusted: anything written to steer an AI is left out.
        signals.retain(|s| crate::injection::scan("", &s.text, "", "").is_empty());
        signals.truncate(MAX_SIGNALS);
        Ok(signals)
    }
}

/// Refuses a learned policy that is empty, oversized, or that drops every rule the current
/// one has.
fn acceptable(current: &str, new: &str) -> Result<(), &'static str> {
    if new.is_empty() {
        return Err("the learned policy was empty");
    }
    if new.len() > 64 * 1024 {
        return Err("the learned policy was too long");
    }
    let rules = |text: &str| crate::policy::parse_rules(text).len();
    if rules(current) > 0 && rules(new) == 0 {
        return Err("the learned policy removed every rule, so it wasn't applied");
    }
    Ok(())
}

fn signal(kind: &'static str, pr: u64, author: &str, about: &str, text: &str) -> Signal {
    Signal {
        kind,
        pr,
        author: author.to_owned(),
        about: clip(about),
        text: clip(text),
    }
}

fn clip(text: &str) -> String {
    text.chars()
        .take(MAX_TEXT)
        .collect::<String>()
        .trim()
        .to_owned()
}

fn is_prowl(body: &str) -> bool {
    PROWL_MARKERS.iter().any(|m| body.contains(m))
}

/// People with write access: owners, organisation members and collaborators.
fn collaborator(association: &str) -> bool {
    matches!(association, "OWNER" | "MEMBER" | "COLLABORATOR")
}

/// Whether a GitHub timestamp is after `since` (Unix ms) — strictly, so feedback from the
/// millisecond a run started is never read twice.
fn after(timestamp: &str, since: i64) -> bool {
    chrono::DateTime::parse_from_rfc3339(timestamp).is_ok_and(|t| t.timestamp_millis() > since)
}

/// A finding thread's first line, without Markdown emphasis.
fn headline(body: &str) -> String {
    body.lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or_default()
        .replace("**", "")
        .trim()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_learned_policy_must_keep_some_rule() {
        let current = "## ALWAYS require human review\n- [AUTH] Auth\n";
        assert!(
            acceptable(
                current,
                "## ALWAYS require human review\n- [AUTH] Auth, narrower\n"
            )
            .is_ok()
        );
        assert_eq!(
            acceptable(current, "Anything goes."),
            Err("the learned policy removed every rule, so it wasn't applied")
        );
        assert!(acceptable(current, "").is_err());
    }

    #[test]
    fn only_collaborators_count() {
        for yes in ["OWNER", "MEMBER", "COLLABORATOR"] {
            assert!(collaborator(yes));
        }
        for no in ["CONTRIBUTOR", "FIRST_TIME_CONTRIBUTOR", "NONE", ""] {
            assert!(!collaborator(no));
        }
    }

    #[test]
    fn timestamps_are_compared_to_the_last_run() {
        // 1_792_238_400_000 is 2026-10-17T12:00:00Z.
        assert!(after("2026-10-17T12:00:00.001Z", 1_792_238_400_000));
        assert!(
            !after("2026-10-17T12:00:00Z", 1_792_238_400_000),
            "the same moment was read"
        );
        assert!(!after("2026-10-16T12:00:00Z", 1_792_238_400_000));
        assert!(!after("", 0));
    }
}
