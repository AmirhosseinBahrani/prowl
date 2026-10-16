//! Polls GitHub for pull requests awaiting the user's review and queues those that pass the
//! Gate. Uses only the GitHub API — never Claude.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

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

pub mod kind {
    pub const COMPLETED: &str = "poll.completed";
    pub const FAILED: &str = "poll.failed";
    pub const RATE_LIMITED: &str = "poll.rate_limited";
    pub const ENQUEUED: &str = "pr.enqueued";
    pub const GATED_OUT: &str = "pr.gated_out";
}

#[derive(Debug, Clone, Copy)]
pub struct PollSettings {
    pub include_team_requests: bool,
    pub skip_drafts: bool,
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
    settings: PollSettings,
    /// Held for the duration of a poll so scheduled and on-demand polls never overlap.
    running: Mutex<()>,
    /// Last gate decision per PR, so a PR that stays gated out is reported once, not every poll.
    reported: Mutex<HashMap<(String, u64), (String, GateReason)>>,
    rate_limit_reported: Mutex<Option<u64>>,
}

impl Poller {
    pub(crate) fn new(
        github: GitHub,
        auth: Auth,
        repos: Repos,
        jobs: Jobs,
        events: EventBus,
        queue: Queue,
        settings: PollSettings,
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
            }),
        }
    }

    /// Polls every `every`, skipping (not bursting) ticks missed while busy or asleep.
    pub(crate) fn spawn(&self, every: Duration, shutdown: CancellationToken) {
        let this = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(every);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = tick.tick() => {}
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

        let settings = self.inner.settings;
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
            include_team_requests: self.inner.settings.include_team_requests,
            skip_drafts: self.inner.settings.skip_drafts,
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
            // Already queued is the steady state for a PR waiting on its review; not news.
            GateOutcome::Skip(GateReason::AlreadyQueued) => Ok(false),
            GateOutcome::Skip(reason) => {
                self.report_gated(&found.repo, pr.number, &pr.head.sha, reason)
                    .await;
                Ok(false)
            }
        }
    }

    /// Re-runs the Gate for a queued job right before its review, since the request may have
    /// been withdrawn, the PR closed or a newer commit pushed while it waited.
    pub(crate) async fn recheck(&self, token: &str, job: &Job) -> Result<Recheck, GitHubError> {
        let AuthState::Connected { login, .. } = self.inner.auth.state().await else {
            return Ok(Recheck::Cancel("not_connected".into()));
        };
        let settings = self.inner.settings;
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
        let limit = self.inner.github.rate_limit();
        let Some(reset_at) = self.inner.github.paused_until() else {
            return Ok(false);
        };
        let remaining = limit.remaining.unwrap_or(0);
        let mut reported = self.inner.rate_limit_reported.lock().await;
        if *reported != Some(reset_at) {
            *reported = Some(reset_at);
            self.inner
                .events
                .emit(
                    kind::RATE_LIMITED,
                    serde_json::json!({ "remaining": remaining, "reset_at": reset_at }),
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
