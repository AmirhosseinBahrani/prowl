//! Polls GitHub for pull requests awaiting the user's review and queues those that pass the
//! Gate. Uses only the GitHub API — never Claude.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::auth::{Auth, AuthState};
use crate::events::EventBus;
use crate::gate::{self, GateContext, GateOutcome, GateReason, PrFacts};
use crate::github::{GitHub, GitHubError, PrRef, PullRequest};
use crate::jobs::Job;
use crate::jobs::{Jobs, NewJob};
use crate::queue::Queue;
use crate::repos::Repos;
use crate::settings::SettingsStore;

pub mod kind {
    pub const COMPLETED: &str = "poll.completed";
    pub const FAILED: &str = "poll.failed";
    pub const RATE_LIMITED: &str = "poll.rate_limited";
    pub const ENQUEUED: &str = "pr.enqueued";
    pub const GATED_OUT: &str = "pr.gated_out";
}

/// Text in every thread PRowl opens for a finding (see `publish::finding_comment`).
pub(crate) const PROWL_THREAD: &str = "PRowl automated review";

/// Why a review the user asked for couldn't be queued.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ManualError {
    #[error("connect a GitHub account first")]
    NotConnected,
    #[error("no such pull request (or the account can't see it)")]
    NotFound,
    #[error("the pull request isn't open")]
    NotOpen,
    #[error("could not read the pull request: {0}")]
    GitHub(String),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PollSummary {
    /// Why the poll did nothing, if it did nothing (`not_connected`, `no_watched_repos`,
    /// `rate_limited`, or an error).
    pub skipped: Option<String>,
    pub candidates: usize,
    pub enqueued: usize,
    pub gated_out: usize,
}

/// A failure while considering one PR: GitHub (handled specially) or anything else.
enum ConsiderError {
    GitHub(GitHubError),
    Internal(anyhow::Error),
}

impl From<GitHubError> for ConsiderError {
    fn from(e: GitHubError) -> Self {
        Self::GitHub(e)
    }
}

impl From<anyhow::Error> for ConsiderError {
    fn from(e: anyhow::Error) -> Self {
        Self::Internal(e)
    }
}

pub(crate) enum Recheck {
    Proceed {
        pr: Box<PullRequest>,
        reviewer: String,
    },
    Cancel(String),
}

#[derive(Clone)]
pub(crate) struct Poller {
    inner: Arc<Inner>,
}

struct Inner {
    github: GitHub,
    auth: Auth,
    repos: Repos,
    jobs: Jobs,
    events: EventBus,
    queue: Queue,
    settings: SettingsStore,
    /// Held for the duration of a poll so scheduled and on-demand polls never overlap.
    running: Mutex<()>,
    /// Last gate decision per PR, so a PR that stays gated out is reported once, not every poll.
    reported: Mutex<HashMap<(String, u64), (String, GateReason)>>,
    rate_limit_reported: Mutex<Option<u64>>,
    /// A review request this close after PRowl's last run is the one that run answered:
    /// clocks differ.
    rerequest_margin: std::time::Duration,
}

impl Poller {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        github: GitHub,
        auth: Auth,
        repos: Repos,
        jobs: Jobs,
        events: EventBus,
        queue: Queue,
        settings: SettingsStore,
        rerequest_margin: std::time::Duration,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                github,
                auth,
                repos,
                jobs,
                events,
                queue,
                settings,
                running: Mutex::new(()),
                reported: Mutex::default(),
                rate_limit_reported: Mutex::default(),
                rerequest_margin,
            }),
        }
    }

    /// Polls every `every`, skipping (not bursting) ticks missed while busy or asleep.
    /// Polls on the interval from the settings, picking up changes to it as they happen.
    pub(crate) fn spawn(&self, shutdown: CancellationToken) {
        let this = self.clone();
        let mut changes = self.inner.settings.subscribe();
        tokio::spawn(async move {
            let mut every = this.inner.settings.get().poll_interval();
            let mut tick = tokio::time::interval(every);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = tick.tick() => {}
                    changed = changes.changed() => {
                        if changed.is_err() {
                            return;
                        }
                        let now = changes.borrow_and_update().poll_interval();
                        if now != every {
                            every = now;
                            tick = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
                            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                        }
                        continue;
                    }
                    () = shutdown.cancelled() => return,
                }
                this.poll_once().await;
            }
        });
    }

    pub(crate) async fn poll_once(&self) -> PollSummary {
        let _running = self.inner.running.lock().await;
        let summary = match self.poll().await {
            Ok(summary) => summary,
            Err(e) => {
                let message = format!("{e:#}");
                tracing::warn!(error = %message, "poll failed");
                let _ = self
                    .inner
                    .events
                    .emit(kind::FAILED, serde_json::json!({ "error": message }))
                    .await;
                PollSummary {
                    skipped: Some(message),
                    ..Default::default()
                }
            }
        };
        if summary.skipped.is_none() {
            // PRs waiting on the user may have been reviewed on GitHub or closed meanwhile.
            if let Err(e) = self.inner.queue.sync_with_github().await {
                tracing::warn!(error = %e, "checking the human review queue failed");
            }
            if let Err(e) = self.verify_resolved().await {
                tracing::warn!(error = %e, "checking resolved review threads failed");
            }
            let _ = self
                .inner
                .events
                .emit(
                    kind::COMPLETED,
                    serde_json::to_value(&summary).unwrap_or_default(),
                )
                .await;
        }
        summary
    }

    async fn poll(&self) -> anyhow::Result<PollSummary> {
        let skipped = |reason: &str| PollSummary {
            skipped: Some(reason.into()),
            ..Default::default()
        };
        let AuthState::Connected { login, .. } = self.inner.auth.state().await else {
            return Ok(skipped("not_connected"));
        };
        let Some(token) = self.inner.auth.token().await else {
            return Ok(skipped("not_connected"));
        };
        let watched: HashSet<String> = self
            .inner
            .repos
            .watched()
            .await?
            .into_iter()
            .map(|r| r.full_name.to_lowercase())
            .collect();
        if watched.is_empty() {
            return Ok(skipped("no_watched_repos"));
        }
        if self.rate_limited().await? {
            return Ok(skipped("rate_limited"));
        }

        let settings = self.inner.settings.get();
        let found = match self
            .inner
            .github
            .review_requests(&token, settings.include_team_requests)
            .await
        {
            Ok(found) => found,
            Err(e) => return self.github_failure(e).await,
        };
        let my_teams = if settings.include_team_requests {
            match self.inner.github.my_teams(&token).await {
                Ok(teams) => teams
                    .into_iter()
                    .map(|t| (t.organization.login, t.slug))
                    .collect(),
                Err(e) => return self.github_failure(e).await,
            }
        } else {
            Vec::new()
        };

        let mut summary = PollSummary {
            candidates: found.len(),
            ..Default::default()
        };
        for pr in found {
            match self
                .consider(&token, &login, &my_teams, &watched, &pr)
                .await
            {
                Ok(true) => summary.enqueued += 1,
                Ok(false) => summary.gated_out += 1,
                Err(ConsiderError::GitHub(e)) => return self.github_failure(e).await,
                Err(ConsiderError::Internal(e)) => return Err(e),
            }
        }
        Ok(summary)
    }

    /// Runs one PR through the Gate. Returns whether it was newly queued.
    async fn consider(
        &self,
        token: &str,
        me: &str,
        my_teams: &[(String, String)],
        watched: &HashSet<String>,
        found: &PrRef,
    ) -> Result<bool, ConsiderError> {
        let repo_watched = watched.contains(&found.repo.to_lowercase());
        if !repo_watched {
            // Don't spend an API call on a PR in a repo we ignore.
            self.report_gated(&found.repo, found.number, "", GateReason::RepoNotWatched)
                .await;
            return Ok(false);
        }
        let pr = self
            .inner
            .github
            .pull_request(token, &found.repo, found.number)
            .await?;
        let already_queued = self
            .inner
            .jobs
            .exists(&found.repo, pr.number, &pr.head.sha)
            .await?;
        let users: Vec<&str> = pr
            .requested_reviewers
            .iter()
            .map(|u| u.login.as_str())
            .collect();
        let teams: Vec<&str> = pr.requested_teams.iter().map(|t| t.slug.as_str()).collect();
        let facts = PrFacts {
            repo: &found.repo,
            open: pr.state == "open",
            draft: pr.draft,
            author: &pr.user.login,
            requested_users: &users,
            requested_teams: &teams,
        };
        let ctx = GateContext {
            me,
            my_teams,
            include_team_requests: self.inner.settings.get().include_team_requests,
            skip_drafts: self.inner.settings.get().skip_drafts,
            repo_watched,
            already_queued,
        };
        match gate::evaluate(&facts, &ctx) {
            GateOutcome::Enqueue => {
                let new = NewJob {
                    repo: &found.repo,
                    pr_number: pr.number,
                    head_sha: &pr.head.sha,
                    title: &pr.title,
                    author: &pr.user.login,
                    html_url: &pr.html_url,
                };
                let id = self.inner.jobs.enqueue(&new).await?;
                if let Some(id) = id {
                    let _ = self
                        .inner
                        .events
                        .emit(
                            kind::ENQUEUED,
                            serde_json::json!({
                                "job_id": id,
                                "repo": found.repo,
                                "pr": pr.number,
                                "head_sha": pr.head.sha,
                                "title": pr.title,
                            }),
                        )
                        .await;
                }
                self.inner
                    .reported
                    .lock()
                    .await
                    .remove(&(found.repo.clone(), pr.number));
                Ok(id.is_some())
            }
            // Already queued is the steady state for a PR waiting on its review; not news —
            // unless the review was requested again since.
            GateOutcome::Skip(GateReason::AlreadyQueued) => {
                self.requested_again(token, me, my_teams, &found.repo, &pr)
                    .await
            }
            GateOutcome::Skip(reason) => {
                self.report_gated(&found.repo, pr.number, &pr.head.sha, reason)
                    .await;
                Ok(false)
            }
        }
    }

    /// Reviews the pull request again when its review was requested again after PRowl's last
    /// run (the author re-requesting it after answering or fixing the threads), even without
    /// a new commit. The new run sees the threads and comments as they are now. Returns
    /// whether a job was queued.
    async fn requested_again(
        &self,
        token: &str,
        me: &str,
        my_teams: &[(String, String)],
        repo: &str,
        pr: &PullRequest,
    ) -> Result<bool, ConsiderError> {
        let Some((last_run, waiting)) = self.inner.jobs.latest_for_pr(repo, pr.number).await?
        else {
            return Ok(false);
        };
        if waiting {
            return Ok(false);
        }
        let Some(requested) = self
            .inner
            .github
            .last_review_request_ms(token, repo, pr.number, me, my_teams)
            .await?
        else {
            return Ok(false);
        };
        if requested <= last_run + self.inner.rerequest_margin.as_millis() as i64 {
            return Ok(false);
        }
        let new = NewJob {
            repo,
            pr_number: pr.number,
            head_sha: &pr.head.sha,
            title: &pr.title,
            author: &pr.user.login,
            html_url: &pr.html_url,
        };
        let id = self
            .inner
            .jobs
            .enqueue_manual(&new, crate::jobs::kinds::RERUN)
            .await?;
        tracing::info!(
            repo,
            pr = pr.number,
            job = id,
            "review requested again; re-reviewing"
        );
        let _ = self
            .inner
            .events
            .emit(
                kind::ENQUEUED,
                serde_json::json!({
                    "job_id": id,
                    "repo": repo,
                    "pr": pr.number,
                    "head_sha": pr.head.sha,
                    "title": pr.title,
                    "rerequested": true,
                }),
            )
            .await;
        Ok(true)
    }

    /// Re-runs the Gate for a queued job right before its review, since the request may have
    /// been withdrawn, the PR closed or a newer commit pushed while it waited.
    pub(crate) async fn recheck(&self, token: &str, job: &Job) -> Result<Recheck, GitHubError> {
        let AuthState::Connected { login, .. } = self.inner.auth.state().await else {
            return Ok(Recheck::Cancel("not_connected".into()));
        };
        let settings = self.inner.settings.get();
        let my_teams: Vec<(String, String)> = if settings.include_team_requests {
            self.inner
                .github
                .my_teams(token)
                .await?
                .into_iter()
                .map(|t| (t.organization.login, t.slug))
                .collect()
        } else {
            Vec::new()
        };
        let repo_watched = self
            .inner
            .repos
            .watched()
            .await
            .map(|repos| {
                repos
                    .iter()
                    .any(|r| r.full_name.eq_ignore_ascii_case(&job.repo))
            })
            .unwrap_or(false);
        let pr = self
            .inner
            .github
            .pull_request(token, &job.repo, job.pr_number)
            .await?;
        if pr.head.sha != job.head_sha {
            return Ok(Recheck::Cancel("superseded_by_newer_commit".into()));
        }
        // Asked for by the user: review requests and drafts don't matter (posting a review
        // clears the request), but GitHub won't take a review of a closed or your own PR.
        if job.manual() {
            return Ok(if pr.state != "open" {
                Recheck::Cancel("pr_not_open".into())
            } else if pr.user.login.eq_ignore_ascii_case(&login) {
                Recheck::Cancel("own_pr".into())
            } else {
                Recheck::Proceed {
                    pr: Box::new(pr),
                    reviewer: login,
                }
            });
        }
        let users: Vec<&str> = pr
            .requested_reviewers
            .iter()
            .map(|u| u.login.as_str())
            .collect();
        let teams: Vec<&str> = pr.requested_teams.iter().map(|t| t.slug.as_str()).collect();
        let facts = PrFacts {
            repo: &job.repo,
            open: pr.state == "open",
            draft: pr.draft,
            author: &pr.user.login,
            requested_users: &users,
            requested_teams: &teams,
        };
        let ctx = GateContext {
            me: &login,
            my_teams: &my_teams,
            include_team_requests: settings.include_team_requests,
            skip_drafts: settings.skip_drafts,
            repo_watched,
            already_queued: false,
        };
        Ok(match gate::evaluate(&facts, &ctx) {
            GateOutcome::Enqueue => Recheck::Proceed {
                pr: Box::new(pr),
                reviewer: login,
            },
            GateOutcome::Skip(reason) => Recheck::Cancel(
                serde_json::to_value(reason)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .unwrap_or_else(|| format!("{reason:?}")),
            ),
        })
    }

    /// For agent-only PRs (with agent verdicts on): once every thread PRowl opened for its
    /// findings is resolved, queues a verification of the current commit — once per commit.
    async fn verify_resolved(&self) -> anyhow::Result<()> {
        if !self.inner.settings.get().agent_verdicts {
            return Ok(());
        }
        let AuthState::Connected { login, .. } = self.inner.auth.state().await else {
            return Ok(());
        };
        let Some(token) = self.inner.auth.token().await else {
            return Ok(());
        };
        for (repo, number) in self.inner.jobs.agent_reviewed_prs().await? {
            let threads = self
                .inner
                .github
                .review_threads(&token, &repo, number)
                .await?;
            let mine: Vec<_> = threads
                .iter()
                .filter(|t| t.author.eq_ignore_ascii_case(&login) && t.body.contains(PROWL_THREAD))
                .collect();
            if mine.is_empty() || mine.iter().any(|t| !t.resolved) {
                continue;
            }
            let pr = self
                .inner
                .github
                .pull_request(&token, &repo, number)
                .await?;
            if pr.state != "open"
                || self
                    .inner
                    .jobs
                    .exists_of_kind(&repo, number, &pr.head.sha, crate::jobs::kinds::VERIFY)
                    .await?
            {
                continue;
            }
            tracing::info!(
                repo,
                pr = number,
                "all of PRowl's threads are resolved; verifying"
            );
            if let Err(e) = self
                .enqueue_manual(&repo, number, crate::jobs::kinds::VERIFY)
                .await
            {
                tracing::warn!(repo, pr = number, error = %e, "could not queue a verification");
            }
        }
        Ok(())
    }

    /// Queues a job for `repo#number` at its current commit, whatever happened to it before:
    /// `kind` is `kinds::RERUN` (review it again) or `kinds::VERIFY`.
    pub(crate) async fn enqueue_manual(
        &self,
        repo: &str,
        number: u64,
        kind: &str,
    ) -> Result<Job, ManualError> {
        let token = self
            .inner
            .auth
            .token()
            .await
            .ok_or(ManualError::NotConnected)?;
        let pr = self
            .inner
            .github
            .pull_request(&token, repo, number)
            .await
            .map_err(|e| match e {
                GitHubError::Status { status, .. } if status == reqwest::StatusCode::NOT_FOUND => {
                    ManualError::NotFound
                }
                other => ManualError::GitHub(other.to_string()),
            })?;
        if pr.state != "open" {
            return Err(ManualError::NotOpen);
        }
        let id = self
            .inner
            .jobs
            .enqueue_manual(
                &NewJob {
                    repo,
                    pr_number: number,
                    head_sha: &pr.head.sha,
                    title: &pr.title,
                    author: &pr.user.login,
                    html_url: &pr.html_url,
                },
                kind,
            )
            .await?;
        let _ = self
            .inner
            .events
            .emit(
                kind::ENQUEUED,
                serde_json::json!({
                    "job_id": id,
                    "repo": repo,
                    "pr": number,
                    "head_sha": pr.head.sha,
                    "title": pr.title,
                    "manual": kind,
                }),
            )
            .await;
        self.inner
            .jobs
            .get(id)
            .await?
            .ok_or_else(|| ManualError::Internal(anyhow::anyhow!("job {id} vanished")))
    }

    async fn report_gated(&self, repo: &str, number: u64, sha: &str, reason: GateReason) {
        let key = (repo.to_owned(), number);
        let mut reported = self.inner.reported.lock().await;
        if reported.get(&key) == Some(&(sha.to_owned(), reason)) {
            return;
        }
        reported.insert(key, (sha.to_owned(), reason));
        drop(reported);
        let _ = self
            .inner
            .events
            .emit(
                kind::GATED_OUT,
                serde_json::json!({ "repo": repo, "pr": number, "head_sha": sha, "reason": reason }),
            )
            .await;
    }

    async fn rate_limited(&self) -> anyhow::Result<bool> {
        let Some(pause) = self.inner.github.rate_pause() else {
            return Ok(false);
        };
        let reset_at = pause.reset_at;
        let mut reported = self.inner.rate_limit_reported.lock().await;
        if *reported != Some(reset_at) {
            *reported = Some(reset_at);
            self.inner
                .events
                .emit(
                    kind::RATE_LIMITED,
                    serde_json::json!({
                        "remaining": pause.remaining,
                        "reset_at": reset_at,
                        "resource": pause.resource,
                    }),
                )
                .await?;
        }
        Ok(true)
    }

    async fn github_failure(&self, e: GitHubError) -> anyhow::Result<PollSummary> {
        match e {
            GitHubError::Unauthorized => {
                self.inner.auth.token_rejected().await?;
                Ok(PollSummary {
                    skipped: Some("not_connected".into()),
                    ..Default::default()
                })
            }
            GitHubError::RateLimited { .. } => {
                self.rate_limited().await?;
                Ok(PollSummary {
                    skipped: Some("rate_limited".into()),
                    ..Default::default()
                })
            }
            other => Err(other.into()),
        }
    }
}
