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
use crate::github::GitHub;
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
                        tokio::select! {
                            () = this.run(&job) => {}
                            // Left as `running`; marked interrupted and retried on next start.
                            () = shutdown.cancelled() => {}
                        }
                        drop(slot);
                    });
                }
            }
        });
    }

    async fn run(&self, job: &Job) {
        if let Err(e) = self.try_run(job).await {
            let message = format!("{e:#}");
            tracing::warn!(job = job.id, error = %message, "review failed");
            let _ = self
                .inner
                .jobs
                .finish(job.id, "failed", Some(&message))
                .await;
            self.emit(kind::FAILED, job, serde_json::json!({ "error": message }))
                .await;
        }
    }

    async fn try_run(&self, job: &Job) -> anyhow::Result<()> {
        let inner = &self.inner;
        let Some(token) = inner.auth.token().await else {
            inner
                .jobs
                .defer(job.id, DEFER, "GitHub account not connected")
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
            Err(e) => {
                inner.jobs.defer(job.id, DEFER, &e.to_string()).await?;
                return Ok(());
            }
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
        let outcome = self.ask(job, &checkout, &message).await;
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

        let target = Target {
            repo: &job.repo,
            number: job.pr_number,
            head_sha: &job.head_sha,
            reviewer: &reviewer,
            policy_scope: &policy.scope,
            policy_version_id: policy.version_id,
        };
        let mut prior = inner.publications.prior(&job.repo, job.pr_number).await?;
        let submitted = publish::publish_result(
            &inner.github,
            &token,
            &target,
            verdict.decision,
            output.as_ref().map_err(String::as_str),
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
    ) -> Result<(String, Result<ReviewOutput, String>, Option<f64>), ClaudeError> {
        let first = self.ask_once(job, checkout, message, None).await?;
        let mut cost = first.cost_usd;
        let parsed = parse_answer(&first);
        let Err(problem) = parsed else {
            return Ok((first.text, parsed, cost));
        };
        tracing::info!(job = job.id, %problem, "review answer invalid; asking Claude to repair it");
        let repair = prompt::repair_message(&problem);
        let second = match &first.session_id {
            Some(session) => self.ask_once(job, checkout, &repair, Some(session)).await?,
            None => {
                self.ask_once(job, checkout, &format!("{message}\n\n{repair}"), None)
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

fn parse_answer(answer: &crate::claude::ClaudeAnswer) -> Result<ReviewOutput, String> {
    match &answer.structured {
        Some(value) => review::parse_value(value),
        None => review::parse(&answer.text),
    }
}
