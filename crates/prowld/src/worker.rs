//! Takes queued jobs and reviews them: re-check the Gate, check out the PR, ask Claude with
//! the Review Policy, validate the answer (one repair attempt), store the review.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::auth::Auth;
use crate::claude::{Claude, ClaudeError};
use crate::decider::{self, Decision, DecisionSettings};
use crate::events::EventBus;
use crate::github::{GitHub, GitHubError};
use crate::jobs::{Job, Jobs};
use crate::policy::{PolicyStore, Scope};
use crate::poller::{Poller, Recheck};
use crate::prompt::{self, PrContext};
use crate::publications::Publications;
use crate::publish::{self, Target};
use crate::queue::Queue;
use crate::review::{self, ReviewOutput};
use crate::reviews::{NewReview, Reviews};
use crate::workspace::{Checkout, Workspace};

const TICK: Duration = Duration::from_millis(250);
/// How long a job waits before another try when it can't run right now (e.g. GitHub down).
const DEFER: Duration = Duration::from_secs(60);

pub mod kind {
    pub const STARTED: &str = "review.started";
    pub const OUTPUT: &str = "review.output";
    pub const FINISHED: &str = "review.finished";
    pub const FAILED: &str = "review.failed";
    pub const CANCELLED: &str = "job.cancelled";
    pub const CLAUDE_MISSING: &str = "claude.missing";
    pub const DECIDED: &str = "review.decided";
    pub const PUBLISHED: &str = "review.published";
    pub const RETRYING: &str = "review.retrying";
    pub const BUDGET_REACHED: &str = "budget.reached";
}

#[derive(Clone)]
pub(crate) struct Worker {
    inner: Arc<Inner>,
}

struct Inner {
    jobs: Jobs,
    reviews: Reviews,
    poller: Poller,
    auth: Auth,
    policies: PolicyStore,
    workspace: Workspace,
    claude: Claude,
    events: EventBus,
    github: GitHub,
    queue: Queue,
    publications: Publications,
    decision: DecisionSettings,
    retry: RetrySettings,
    limits: Limits,
    /// UTC day (ms at midnight) for which `budget.reached` was announced.
    budget_reported: std::sync::Mutex<Option<i64>>,
    /// Reviews running right now, read by the power manager.
    active: Arc<std::sync::atomic::AtomicUsize>,
    slots: Arc<Semaphore>,
}

pub(crate) struct WorkerParts {
    pub jobs: Jobs,
    pub reviews: Reviews,
    pub poller: Poller,
    pub auth: Auth,
    pub policies: PolicyStore,
    pub workspace: Workspace,
    pub claude: Claude,
    pub events: EventBus,
    pub github: GitHub,
    pub queue: Queue,
    pub publications: Publications,
    pub decision: DecisionSettings,
    pub retry: RetrySettings,
    pub limits: Limits,
    pub active: Arc<std::sync::atomic::AtomicUsize>,
    pub concurrency: usize,
}

impl Worker {
    pub(crate) fn new(parts: WorkerParts) -> Self {
        Self {
            inner: Arc::new(Inner {
                slots: Arc::new(Semaphore::new(parts.concurrency.max(1))),
                jobs: parts.jobs,
                reviews: parts.reviews,
                poller: parts.poller,
                auth: parts.auth,
                policies: parts.policies,
                workspace: parts.workspace,
                claude: parts.claude,
                events: parts.events,
                github: parts.github,
                queue: parts.queue,
                publications: parts.publications,
                decision: parts.decision,
                retry: parts.retry,
                limits: parts.limits,
                budget_reported: std::sync::Mutex::new(None),
                active: parts.active,
            }),
        }
    }

    pub(crate) fn spawn(&self, shutdown: CancellationToken) {
        let this = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(TICK);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = tick.tick() => {}
                    () = shutdown.cancelled() => return,
                }
                while let Ok(slot) = this.inner.slots.clone().try_acquire_owned() {
                    let job = match this.inner.jobs.claim_next().await {
                        Ok(Some(job)) => job,
                        Ok(None) => break,
                        Err(e) => {
                            tracing::error!(error = %format!("{e:#}"), "claiming a job failed");
                            break;
                        }
                    };
                    let this = this.clone();
                    let shutdown = shutdown.clone();
                    tokio::spawn(async move {
                        let active = this.inner.active.clone();
                        active.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        tokio::select! {
                            () = this.run(&job) => {}
                            // Left as `running`; marked interrupted and retried on next start.
                            () = shutdown.cancelled() => {}
                        }
                        active.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                        drop(slot);
                    });
                }
            }
        });
    }

    async fn run(&self, job: &Job) {
        let Err(e) = self.try_run(job).await else {
            return;
        };
        let message = format!("{e:#}");
        let attempts = u32::try_from(job.attempts).unwrap_or(u32::MAX);
        if is_transient(&e) && attempts < self.inner.retry.max_attempts {
            let delay = self.inner.retry.delay(attempts);
            tracing::info!(job = job.id, attempts, ?delay, error = %message, "review will be retried");
            let _ = self.inner.jobs.defer(job.id, delay, &message).await;
            self.emit(
                kind::RETRYING,
                job,
                serde_json::json!({
                    "error": message,
                    "attempt": attempts,
                    "retry_in_ms": delay.as_millis() as u64,
                }),
            )
            .await;
            return;
        }
        tracing::warn!(job = job.id, error = %message, "review failed");
        let _ = self
            .inner
            .jobs
            .finish(job.id, "failed", Some(&message))
            .await;
        self.emit(kind::FAILED, job, serde_json::json!({ "error": message }))
            .await;
    }

    async fn try_run(&self, job: &Job) -> anyhow::Result<()> {
        let inner = &self.inner;
        let Some(token) = inner.auth.token().await else {
            // Waiting for the user to reconnect isn't a failed attempt.
            inner
                .jobs
                .defer_without_attempt(job.id, DEFER, "GitHub account not connected")
                .await?;
            return Ok(());
        };
        let (pr, reviewer) = match inner.poller.recheck(&token, job).await {
            Ok(Recheck::Proceed { pr, reviewer }) => (pr, reviewer),
            Ok(Recheck::Cancel(reason)) => {
                inner
                    .jobs
                    .finish(job.id, "cancelled", Some(&reason))
                    .await?;
                self.emit(
                    kind::CANCELLED,
                    job,
                    serde_json::json!({ "reason": reason }),
                )
                .await;
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        };
        // A previous attempt may already have reviewed (and even published) this commit.
        if let Some(stored) = inner.reviews.list(Some(job.id)).await?.into_iter().next() {
            if stored.published_url.is_some() {
                inner.jobs.finish(job.id, "done", None).await?;
                return Ok(());
            }
            if let Some(decision) = stored.decision {
                tracing::info!(
                    job = job.id,
                    "publishing the review stored by an earlier attempt"
                );
                let output = stored
                    .output
                    .as_ref()
                    .ok_or(stored.invalid_reason.as_deref().unwrap_or("invalid output"));
                return self
                    .publish(
                        job,
                        &token,
                        &reviewer,
                        stored.id,
                        &stored.policy_scope,
                        stored.policy_version_id,
                        decider::Verdict {
                            decision,
                            reasons: stored.decision_reasons.clone(),
                        },
                        output,
                    )
                    .await;
            }
        }
        let changed_lines = pr.additions + pr.deletions;
        if changed_lines > inner.limits.max_changed_lines
            || pr.changed_files > inner.limits.max_changed_files
        {
            return self
                .too_large(job, &token, &reviewer, changed_lines, pr.changed_files)
                .await;
        }
        let Some(budget_left) = self.budget_left(job).await? else {
            return Ok(());
        };

        if let Err(e) = inner.claude.locate() {
            self.emit(
                kind::CLAUDE_MISSING,
                job,
                serde_json::json!({ "error": e.to_string() }),
            )
            .await;
            return Err(e.into());
        }

        let scope = Scope::parse_repo(&job.repo)?;
        let policy = inner.policies.effective(Some(&scope)).await?;
        let labels: Vec<String> = pr.labels.iter().map(|l| l.name.clone()).collect();
        let checkout = inner
            .workspace
            .checkout(
                job.id,
                &job.repo,
                job.pr_number,
                &pr.base.ref_name,
                &pr.base.sha,
                &job.head_sha,
                &token,
            )
            .await?;
        let material = checkout.material.display().to_string().replace('\\', "/");
        let message = prompt::first_message(
            &PrContext {
                repo: &job.repo,
                number: job.pr_number,
                title: &pr.title,
                author: &pr.user.login,
                description: pr.body.as_deref().unwrap_or(""),
                base_ref: &pr.base.ref_name,
                base_sha: &pr.base.sha,
                head_sha: &job.head_sha,
                changed_files: pr.changed_files,
                additions: pr.additions,
                deletions: pr.deletions,
                labels: &labels,
                reviewer: &reviewer,
                material_dir: &material,
            },
            &policy,
        );

        self.emit(
            kind::STARTED,
            job,
            serde_json::json!({ "policy_version_id": policy.version_id }),
        )
        .await;
        let started = Instant::now();
        let outcome = self.ask(job, &checkout, &message, budget_left).await;
        inner.workspace.remove(&job.repo, &checkout).await;
        let (answer, output, cost) = outcome?;

        let review_id = inner
            .reviews
            .insert(&NewReview {
                job_id: job.id,
                policy_scope: &policy.scope,
                policy_version_id: policy.version_id,
                output: output.as_ref().map_err(String::as_str),
                raw_answer: &answer,
                cost_usd: cost,
                duration_ms: started.elapsed().as_millis() as i64,
            })
            .await?;
        let details = match &output {
            Ok(o) => serde_json::json!({
                "review_id": review_id, "valid": true, "severity": o.severity,
                "risk_score": o.risk_score, "cost_usd": cost,
                "policy_version_id": policy.version_id,
            }),
            Err(reason) => serde_json::json!({
                "review_id": review_id, "valid": false, "invalid_reason": reason,
                "cost_usd": cost, "policy_version_id": policy.version_id,
            }),
        };
        self.emit(kind::FINISHED, job, details).await;

        let rule_ids: Vec<String> = policy.rules.iter().map(|r| r.id.clone()).collect();
        let verdict = decider::decide(
            output.as_ref().map_err(String::as_str),
            &rule_ids,
            inner.decision,
        );
        inner
            .reviews
            .set_decision(review_id, verdict.decision, &verdict.reasons)
            .await?;
        self.emit(
            kind::DECIDED,
            job,
            serde_json::json!({
                "review_id": review_id,
                "decision": verdict.decision,
                "reasons": verdict.reasons,
                "summary": publish::describe(verdict.decision, &verdict.reasons),
            }),
        )
        .await;

        self.publish(
            job,
            &token,
            &reviewer,
            review_id,
            &policy.scope,
            policy.version_id,
            verdict,
            output.as_ref().map_err(String::as_str),
        )
        .await
    }

    /// Sends an oversized PR straight to a human: a stored NEEDS_HUMAN review with no Claude run.
    async fn too_large(
        &self,
        job: &Job,
        token: &str,
        reviewer: &str,
        changed_lines: u64,
        changed_files: u64,
    ) -> anyhow::Result<()> {
        let inner = &self.inner;
        let policy = inner
            .policies
            .effective(Some(&Scope::parse_repo(&job.repo)?))
            .await?;
        let detail = "too large for an agent review";
        let review_id = inner
            .reviews
            .insert(&NewReview {
                job_id: job.id,
                policy_scope: &policy.scope,
                policy_version_id: policy.version_id,
                output: Err(detail),
                raw_answer: "",
                cost_usd: None,
                duration_ms: 0,
            })
            .await?;
        let verdict = decider::Verdict {
            decision: Decision::NeedsHuman,
            reasons: vec![decider::Reason::TooLarge {
                changed_lines,
                changed_files,
                max_lines: inner.limits.max_changed_lines,
                max_files: inner.limits.max_changed_files,
            }],
        };
        inner
            .reviews
            .set_decision(review_id, verdict.decision, &verdict.reasons)
            .await?;
        self.emit(
            kind::DECIDED,
            job,
            serde_json::json!({
                "review_id": review_id,
                "decision": verdict.decision,
                "reasons": verdict.reasons,
                "summary": publish::describe(verdict.decision, &verdict.reasons),
            }),
        )
        .await;
        self.publish(
            job,
            token,
            reviewer,
            review_id,
            &policy.scope,
            policy.version_id,
            verdict,
            Err(detail),
        )
        .await
    }

    /// What today's budget still allows for this run: `Some(None)` when there is no cap,
    /// `None` when it is used up (the job then waits for tomorrow, UTC).
    async fn budget_left(&self, job: &Job) -> anyhow::Result<Option<Option<f64>>> {
        let inner = &self.inner;
        let Some(cap) = inner.limits.daily_budget_usd else {
            return Ok(Some(None));
        };
        let day = utc_midnight_ms(crate::events::now_ms());
        let (spent, _) = inner.reviews.spent_since(day).await?;
        if spent < cap {
            return Ok(Some(Some(cap - spent)));
        }
        let tomorrow = day + DAY_MS;
        let wait = Duration::from_millis((tomorrow - crate::events::now_ms()).max(0) as u64);
        inner
            .jobs
            .defer_without_attempt(job.id, wait, "daily Claude budget reached")
            .await?;
        let announce = {
            let mut reported = inner.budget_reported.lock().unwrap();
            let first = *reported != Some(day);
            *reported = Some(day);
            first
        };
        if announce {
            self.emit(
                kind::BUDGET_REACHED,
                job,
                serde_json::json!({ "spent_usd": spent, "daily_budget_usd": cap, "resumes_at_ms": tomorrow }),
            )
            .await;
        }
        Ok(None)
    }

    /// Posts a decided review to GitHub and records it. Safe to repeat: posts made by an
    /// earlier, interrupted attempt are found by their marker and reused.
    #[allow(clippy::too_many_arguments)]
    async fn publish(
        &self,
        job: &Job,
        token: &str,
        reviewer: &str,
        review_id: i64,
        policy_scope: &str,
        policy_version_id: i64,
        verdict: decider::Verdict,
        output: Result<&ReviewOutput, &str>,
    ) -> anyhow::Result<()> {
        let inner = &self.inner;
        let target = Target {
            repo: &job.repo,
            number: job.pr_number,
            head_sha: &job.head_sha,
            reviewer,
            policy_scope,
            policy_version_id,
        };
        let mut prior = inner.publications.prior(&job.repo, job.pr_number).await?;
        let submitted = publish::publish_result(
            &inner.github,
            token,
            &target,
            verdict.decision,
            output,
            &verdict.reasons,
            &mut prior,
        )
        .await?;
        inner
            .publications
            .save(
                &job.repo,
                job.pr_number,
                &job.head_sha,
                verdict.decision,
                &prior,
            )
            .await?;
        inner
            .reviews
            .set_published(review_id, submitted.id, &submitted.html_url)
            .await?;
        // Only once the comment link is stored, so the queue never shows an item without it.
        if verdict.decision == Decision::NeedsHuman {
            inner
                .queue
                .add(&job.repo, job.pr_number, &job.head_sha, review_id)
                .await?;
        } else {
            // A newer commit no longer needs a human: it leaves the queue.
            inner.queue.supersede(&job.repo, job.pr_number).await?;
        }
        self.emit(
            kind::PUBLISHED,
            job,
            serde_json::json!({
                "review_id": review_id,
                "decision": verdict.decision,
                "url": submitted.html_url,
            }),
        )
        .await;
        inner.jobs.finish(job.id, "done", None).await?;
        Ok(())
    }

    /// Asks Claude, and once more with a repair request if the answer breaks the contract.
    /// Returns the final answer text, the parsed review (or why it's invalid) and total cost.
    async fn ask(
        &self,
        job: &Job,
        checkout: &Checkout,
        message: &str,
        budget: Option<f64>,
    ) -> Result<(String, Result<ReviewOutput, String>, Option<f64>), ClaudeError> {
        let first = self.ask_once(job, checkout, message, None, budget).await?;
        let mut cost = first.cost_usd;
        let parsed = parse_answer(&first);
        let Err(problem) = parsed else {
            return Ok((first.text, parsed, cost));
        };
        tracing::info!(job = job.id, %problem, "review answer invalid; asking Claude to repair it");
        let repair = prompt::repair_message(&problem);
        let second = match &first.session_id {
            Some(session) => {
                self.ask_once(job, checkout, &repair, Some(session), budget)
                    .await?
            }
            None => {
                self.ask_once(
                    job,
                    checkout,
                    &format!("{message}\n\n{repair}"),
                    None,
                    budget,
                )
                .await?
            }
        };
        cost = match (cost, second.cost_usd) {
            (Some(a), Some(b)) => Some(a + b),
            (a, b) => a.or(b),
        };
        let parsed = parse_answer(&second);
        Ok((second.text, parsed, cost))
    }

    async fn ask_once(
        &self,
        job: &Job,
        checkout: &Checkout,
        prompt: &str,
        resume: Option<&str>,
        budget: Option<f64>,
    ) -> Result<crate::claude::ClaudeAnswer, ClaudeError> {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let forward = {
            let this = self.clone();
            let job = job.clone();
            tokio::spawn(async move {
                while let Some(text) = rx.recv().await {
                    let text: String = text.chars().take(4000).collect();
                    this.emit(kind::OUTPUT, &job, serde_json::json!({ "text": text }))
                        .await;
                }
            })
        };
        let result = self
            .inner
            .claude
            .run(
                &checkout.worktree,
                &checkout.material,
                prompt,
                resume,
                budget,
                |text| {
                    let _ = tx.send(text.to_owned());
                },
            )
            .await;
        drop(tx);
        let _ = forward.await;
        result
    }

    async fn emit(&self, kind: &str, job: &Job, mut payload: serde_json::Value) {
        if let Some(map) = payload.as_object_mut() {
            map.insert("job_id".into(), job.id.into());
            map.insert("repo".into(), job.repo.clone().into());
            map.insert("pr".into(), job.pr_number.into());
        }
        let _ = self.inner.events.emit(kind, payload).await;
    }
}

const DAY_MS: i64 = 86_400_000;

pub(crate) fn utc_midnight_ms(now_ms: i64) -> i64 {
    now_ms - now_ms.rem_euclid(DAY_MS)
}

/// Spending and size limits.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Limits {
    /// Total Claude spend per UTC day; no new reviews start once reached.
    pub daily_budget_usd: Option<f64>,
    /// Added + deleted lines above which a PR goes to a human without a Claude run.
    pub max_changed_lines: u64,
    pub max_changed_files: u64,
}

/// How failed reviews are retried.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RetrySettings {
    /// Wait before the second attempt; doubles each time after.
    pub base: Duration,
    /// Attempts in total, including the first.
    pub max_attempts: u32,
}

impl RetrySettings {
    const MAX_DELAY: Duration = Duration::from_secs(30 * 60);

    /// Delay after `attempts` failed attempts: base, 2×base, 4×base… capped at 30 minutes.
    fn delay(&self, attempts: u32) -> Duration {
        let factor = 2u32.saturating_pow(attempts.saturating_sub(1));
        self.base.saturating_mul(factor).min(Self::MAX_DELAY)
    }
}

/// Whether trying again later could succeed. Missing tools and requests GitHub rejects as
/// invalid won't fix themselves; network trouble, 5xx, rate limits, timeouts and git
/// failures might.
fn is_transient(e: &anyhow::Error) -> bool {
    for cause in e.chain() {
        if let Some(e) = cause.downcast_ref::<ClaudeError>() {
            return !matches!(e, ClaudeError::NotFound);
        }
        if let Some(e) = cause.downcast_ref::<GitHubError>() {
            return match e {
                GitHubError::Status { status, .. } => {
                    status.is_server_error() || matches!(status.as_u16(), 408 | 409 | 429)
                }
                GitHubError::Unauthorized
                | GitHubError::RateLimited { .. }
                | GitHubError::Transport(_) => true,
            };
        }
    }
    true
}

fn parse_answer(answer: &crate::claude::ClaudeAnswer) -> Result<ReviewOutput, String> {
    match &answer.structured {
        Some(value) => review::parse_value(value),
        None => review::parse(&answer.text),
    }
}
