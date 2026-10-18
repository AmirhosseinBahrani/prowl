//! Takes queued jobs and reviews them: re-check the Gate, check out the PR, ask Claude with
//! the Review Policy, validate the answer (one repair attempt), store the review.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

use crate::auth::Auth;
use crate::claude::{Claude, ClaudeError, StreamItem};
use crate::decider::{self, Decision};
use crate::events::EventBus;
use crate::github::{GitHub, GitHubError, PullRequest};
use crate::jobs::{Job, Jobs};
use crate::policy::{PolicyStore, Scope};
use crate::poller::{Poller, Recheck};
use crate::prompt::{self, PrContext};
use crate::publications::Publications;
use crate::publish::{self, Target};
use crate::queue::Queue;
use crate::review::{self, ReviewOutput};
use crate::reviews::{NewReview, Reviews};
use crate::settings::SettingsStore;
use crate::transcripts::{self, Transcripts};
use crate::workspace::{Checkout, Workspace};

const TICK: Duration = Duration::from_millis(250);
/// How long a job waits before another try when it can't run right now (e.g. GitHub down).
const DEFER: Duration = Duration::from_secs(60);

pub mod kind {
    pub const STARTED: &str = "review.started";
    pub const OUTPUT: &str = "review.output";
    /// Claude used a tool: `tool`, `target` (the file or pattern).
    pub const TOOL: &str = "review.tool";
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
    transcripts: Transcripts,
    settings: SettingsStore,
    retry: RetrySettings,
    /// Budget day (its start, Unix ms) for which `budget.reached` was announced.
    budget_reported: std::sync::Mutex<Option<i64>>,
    /// Reviews running right now, read by the power manager.
    active: Arc<std::sync::atomic::AtomicUsize>,
    /// How to stop each running job, by id.
    running: std::sync::Mutex<std::collections::HashMap<i64, CancellationToken>>,
}

/// What cancelling a job did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cancelled {
    /// It was waiting and won't run.
    Waiting,
    /// It was running and is being stopped.
    Running,
}

pub(crate) const CANCELLED_BY_USER: &str = "cancelled by you";

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
    pub transcripts: Transcripts,
    pub settings: SettingsStore,
    pub retry: RetrySettings,
    pub active: Arc<std::sync::atomic::AtomicUsize>,
}

impl Worker {
    pub(crate) fn new(parts: WorkerParts) -> Self {
        Self {
            inner: Arc::new(Inner {
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
                transcripts: parts.transcripts,
                settings: parts.settings,
                retry: parts.retry,
                budget_reported: std::sync::Mutex::new(None),
                active: parts.active,
                running: std::sync::Mutex::default(),
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
                // The limit is read live, so a settings change applies at the next claim.
                while this.inner.active.load(std::sync::atomic::Ordering::SeqCst)
                    < this.inner.settings.get().review_concurrency
                {
                    let job = match this.inner.jobs.claim_next().await {
                        Ok(Some(job)) => job,
                        Ok(None) => break,
                        Err(e) => {
                            tracing::error!(error = %format!("{e:#}"), "claiming a job failed");
                            break;
                        }
                    };
                    let active = this.inner.active.clone();
                    active.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let cancel = CancellationToken::new();
                    this.inner
                        .running
                        .lock()
                        .unwrap()
                        .insert(job.id, cancel.clone());
                    let this = this.clone();
                    let shutdown = shutdown.clone();
                    tokio::spawn(async move {
                        tokio::select! {
                            () = this.run(&job) => {}
                            // Left as `running`; marked interrupted and retried on next start.
                            () = shutdown.cancelled() => {}
                            // Dropping the review stops Claude (its process is killed on drop).
                            () = cancel.cancelled() => this.cancelled(&job).await,
                        }
                        this.inner.running.lock().unwrap().remove(&job.id);
                        active.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
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
        if let Some(reset_at) = inner.github.paused_until() {
            let wait = (reset_at as i64 * 1000 - crate::events::now_ms()).max(0) as u64;
            inner
                .jobs
                .defer_without_attempt(
                    job.id,
                    Duration::from_millis(wait),
                    "GitHub rate limit nearly used up",
                )
                .await?;
            return Ok(());
        }
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
                // A verification that escalated was stored with its reasons and is published
                // like any review that needs a human.
                if job.kind == crate::jobs::kinds::VERIFY && stored.decision_reasons.is_empty() {
                    let checked = inner
                        .reviews
                        .latest_valid_for_pr(&job.repo, job.pr_number, job.id)
                        .await?
                        .map_or(0, |o| o.findings.len());
                    let policy = inner
                        .policies
                        .effective(Some(&Scope::parse_repo(&job.repo)?))
                        .await?;
                    return self
                        .finish_verification(
                            job, &token, &reviewer, stored.id, &policy, output, checked,
                        )
                        .await;
                }
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
        let settings = inner.settings.get();
        let changed_lines = pr.additions + pr.deletions;
        // A PR waiting in the Human Review Queue is verified only because the user asked: the
        // approval is theirs to request. Anything else goes through every policy gate again,
        // since the fixes are new code.
        let verify_for_human = job.kind == crate::jobs::kinds::VERIFY
            && inner.queue.is_pending(&job.repo, job.pr_number).await?;
        if !verify_for_human
            && (changed_lines > settings.max_changed_lines
                || pr.changed_files > settings.max_changed_files)
        {
            return self
                .too_large(job, &token, &reviewer, changed_lines, pr.changed_files)
                .await;
        }
        let Some(budget_left) = self.budget_left(job).await? else {
            return Ok(());
        };
        let file_rule = self
            .file_rule(&settings, &job.repo, job.pr_number, &token)
            .await?;

        if let Err(e) = inner.claude.locate() {
            self.emit(
                kind::CLAUDE_MISSING,
                job,
                serde_json::json!({ "error": e.to_string() }),
            )
            .await;
            return Err(e.into());
        }
        // A verification checks the latest review's findings against the code as it is now.
        let verify = if job.kind == crate::jobs::kinds::VERIFY {
            let previous = inner
                .reviews
                .latest_valid_for_pr(&job.repo, job.pr_number, job.id)
                .await?
                .map(|o| o.findings)
                .unwrap_or_default();
            let threads = inner
                .github
                .review_threads(&token, &job.repo, job.pr_number)
                .await?;
            Some(VerifyInput { previous, threads })
        } else {
            None
        };
        // A review sees what was said on the PR so far, so it doesn't repeat it.
        let discussion = match verify {
            Some(_) => prompt::Discussion::default(),
            None => self.discussion(job, &token).await,
        };

        let ClaudeRun {
            policy,
            answer,
            output,
            cost,
            duration_ms,
            alarms,
        } = self
            .run_claude(
                job,
                &pr,
                &reviewer,
                &token,
                budget_left,
                verify.as_ref(),
                &discussion,
            )
            .await?;

        let review_id = inner
            .reviews
            .insert(&NewReview {
                job_id: job.id,
                policy_scope: &policy.scope,
                policy_version_id: policy.version_id,
                output: output.as_ref().map_err(String::as_str),
                raw_answer: &answer,
                cost_usd: cost,
                duration_ms,
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
        let mut verdict = decider::decide(
            output.as_ref().map_err(String::as_str),
            &rule_ids,
            decider::DecisionSettings {
                risk_threshold: settings.risk_threshold,
            },
        );
        // File-pattern rules only escalate.
        if let Some(rule) = file_rule {
            verdict.decision = Decision::NeedsHuman;
            verdict.reasons.insert(0, rule);
        }
        sound_alarms(&mut verdict, &alarms);
        // A verification approves only what the policy would leave to the agent (or what the
        // user asked to approve, for a PR waiting on them) — and never when the PR may have
        // manipulated the agent. Otherwise the result is published like any review that needs
        // a human: threads, comment, queue.
        if let Some(verify) = &verify
            && ((verify_for_human && !compromised(&verdict))
                || verdict.decision == Decision::AgentOnly)
        {
            return self
                .finish_verification(
                    job,
                    &token,
                    &reviewer,
                    review_id,
                    &policy,
                    output.as_ref().map_err(String::as_str),
                    verify.previous.len(),
                )
                .await;
        }
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

    /// The PR's review threads and conversation, without PRowl's own conversation comments.
    /// Context only — what is already posted is checked again before publishing — so a part
    /// GitHub can't give is left out rather than failing the review.
    async fn discussion(&self, job: &Job, token: &str) -> prompt::Discussion {
        let github = &self.inner.github;
        let threads = github
            .review_threads(token, &job.repo, job.pr_number)
            .await
            .unwrap_or_else(|e| {
                tracing::warn!(job = job.id, error = %e, "couldn't load the review threads");
                Vec::new()
            });
        let comments = github
            .list_issue_comments(token, &job.repo, job.pr_number)
            .await
            .unwrap_or_else(|e| {
                tracing::warn!(job = job.id, error = %e, "couldn't load the conversation");
                Vec::new()
            })
            .into_iter()
            .filter_map(|c| {
                let body = c.body?;
                (!body.contains("<!-- prowl:")).then(|| crate::github::ThreadComment {
                    author: c.user.map(|u| u.login).unwrap_or_else(|| "ghost".into()),
                    body,
                    association: String::new(),
                    created_at: String::new(),
                })
            })
            .collect();
        prompt::Discussion { threads, comments }
    }

    /// Checks out the PR, asks Claude with the effective policy (one repair attempt) and
    /// returns the answer. Publishes and stores nothing.
    #[allow(clippy::too_many_arguments)]
    async fn run_claude(
        &self,
        job: &Job,
        pr: &PullRequest,
        reviewer: &str,
        token: &str,
        budget: Option<f64>,
        verify: Option<&VerifyInput>,
        discussion: &prompt::Discussion,
    ) -> anyhow::Result<ClaudeRun> {
        let inner = &self.inner;
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
                token,
            )
            .await?;
        let material = checkout.material.display().to_string().replace('\\', "/");
        // Look for text written to steer the reviewer before trusting the reviewer with it.
        let read =
            |name: &str| std::fs::read_to_string(checkout.material.join(name)).unwrap_or_default();
        let mut alarms = crate::injection::scan(
            &pr.title,
            pr.body.as_deref().unwrap_or(""),
            &read("commits.txt"),
            &read("pr.diff"),
        );
        if !alarms.is_empty() {
            tracing::warn!(
                job = job.id,
                found = alarms.len(),
                "possible prompt injection in the PR"
            );
        }
        let context = PrContext {
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
            reviewer,
            material_dir: &material,
        };
        let message = match verify {
            Some(v) => prompt::verify_message(&context, &policy, &v.previous, &v.threads),
            None => prompt::first_message(&context, &policy, discussion),
        };
        self.emit(
            kind::STARTED,
            job,
            serde_json::json!({ "policy_version_id": policy.version_id }),
        )
        .await;
        let mut material_files: Vec<String> = std::fs::read_dir(&checkout.material)
            .map(|dir| {
                dir.filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        material_files.sort();
        self.record(
            job,
            transcripts::kind::CONTEXT,
            serde_json::json!({
                "prompt": message,
                "model": inner.settings.get().claude_model,
                "policy_scope": policy.scope,
                "policy_version_id": policy.version_id,
                "head_sha": job.head_sha,
                "base": format!("{}@{}", pr.base.ref_name, pr.base.sha),
                "material_dir": material,
                "material_files": material_files,
                "tools": ["Read", "Grep", "Glob"],
            }),
        )
        .await;
        let started = Instant::now();
        let outcome = self.ask(job, &checkout, &message, budget).await;
        inner.workspace.remove(&job.repo, &checkout).await;
        if let Err(e) = &outcome {
            self.record(
                job,
                transcripts::kind::ERROR,
                serde_json::json!({ "message": e.to_string() }),
            )
            .await;
        }
        let (answer, mut output, cost) = outcome?;
        // Nothing credential-like leaves PRowl in a review, least of all its own token.
        if let Ok(review) = output.as_mut() {
            let removed = crate::secrets::redact_review(review, &[token]);
            if removed > 0 {
                tracing::warn!(
                    job = job.id,
                    removed,
                    "redacted credentials from the review"
                );
                alarms.push(decider::Reason::SecretRedacted {
                    count: removed as u32,
                });
            }
        }
        Ok(ClaudeRun {
            policy,
            answer,
            output,
            cost,
            duration_ms: started.elapsed().as_millis() as i64,
            alarms,
        })
    }

    /// The safety rule a changed file matches, if the repository has file rules.
    async fn file_rule(
        &self,
        settings: &crate::settings::Settings,
        repo: &str,
        number: u64,
        token: &str,
    ) -> anyhow::Result<Option<decider::Reason>> {
        if !settings
            .file_rules
            .keys()
            .any(|r| r.eq_ignore_ascii_case(repo))
        {
            return Ok(None);
        }
        let files: Vec<String> = self
            .inner
            .github
            .pull_files(token, repo, number)
            .await?
            .into_iter()
            .map(|f| f.filename)
            .collect();
        Ok(settings
            .matching_rule(repo, &files)
            .map(|(pattern, file)| decider::Reason::FilePattern { pattern, file }))
    }

    /// Reviews a PR exactly like a real run — same policy, prompt, checks and decision — but
    /// posts nothing to GitHub and touches no queue. Its cost counts against the budget.
    pub(crate) async fn dry_run(&self, repo: &str, number: u64) -> anyhow::Result<DryRun> {
        let inner = &self.inner;
        Scope::parse_repo(repo)?;
        let token = inner
            .auth
            .token()
            .await
            .ok_or_else(|| anyhow::anyhow!("connect a GitHub account first"))?;
        let reviewer = match inner.auth.state().await {
            crate::auth::AuthState::Connected { login, .. } => login,
            _ => anyhow::bail!("connect a GitHub account first"),
        };
        let pr = inner.github.pull_request(&token, repo, number).await?;
        // A throwaway job: negative ids never collide with real ones (checkout dirs, events).
        let job = Job {
            id: -(crate::events::now_ms()),
            repo: repo.to_owned(),
            pr_number: number,
            head_sha: pr.head.sha.clone(),
            title: pr.title.clone(),
            author: pr.user.login.clone(),
            html_url: pr.html_url.clone(),
            status: "dry_run".into(),
            attempts: 1,
            error: None,
            created_at_ms: crate::events::now_ms(),
            kind: crate::jobs::kinds::RERUN.into(),
        };
        let settings = inner.settings.get();
        let changed_lines = pr.additions + pr.deletions;
        if changed_lines > settings.max_changed_lines
            || pr.changed_files > settings.max_changed_files
        {
            let policy = inner
                .policies
                .effective(Some(&Scope::parse_repo(repo)?))
                .await?;
            return Ok(DryRun {
                decision: Decision::NeedsHuman,
                reasons: vec![decider::Reason::TooLarge {
                    changed_lines,
                    changed_files: pr.changed_files,
                    max_lines: settings.max_changed_lines,
                    max_files: settings.max_changed_files,
                }],
                review: None,
                invalid_reason: Some("too large for an agent review".into()),
                cost_usd: None,
                policy_scope: policy.scope,
                policy_version_id: policy.version_id,
            });
        }
        let budget = match settings.daily_budget_usd {
            None => None,
            Some(cap) => {
                let (spent, _) = inner
                    .reviews
                    .spent_since(settings.day_start_ms(crate::events::now_ms()))
                    .await?;
                anyhow::ensure!(spent < cap, "today's Claude budget is used up");
                Some(cap - spent)
            }
        };
        inner.claude.locate()?;
        let discussion = self.discussion(&job, &token).await;
        let run = self
            .run_claude(&job, &pr, &reviewer, &token, budget, None, &discussion)
            .await?;
        inner.reviews.record_dry_run(repo, number, run.cost).await?;
        let rule_ids: Vec<String> = run.policy.rules.iter().map(|r| r.id.clone()).collect();
        let mut verdict = decider::decide(
            run.output.as_ref().map_err(String::as_str),
            &rule_ids,
            decider::DecisionSettings {
                risk_threshold: settings.risk_threshold,
            },
        );
        if let Some(rule) = self.file_rule(&settings, repo, number, &token).await? {
            verdict.decision = Decision::NeedsHuman;
            verdict.reasons.insert(0, rule);
        }
        sound_alarms(&mut verdict, &run.alarms);
        Ok(DryRun {
            decision: verdict.decision,
            reasons: verdict.reasons,
            invalid_reason: run.output.as_ref().err().cloned(),
            review: run.output.ok(),
            cost_usd: run.cost,
            policy_scope: run.policy.scope,
            policy_version_id: run.policy.version_id,
        })
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
        let settings = inner.settings.get();
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
                max_lines: settings.max_changed_lines,
                max_files: settings.max_changed_files,
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
        let settings = inner.settings.get();
        let Some(cap) = settings.daily_budget_usd else {
            return Ok(Some(None));
        };
        let day = settings.day_start_ms(crate::events::now_ms());
        let (spent, _) = inner.reviews.spent_since(day).await?;
        if spent < cap {
            return Ok(Some(Some(cap - spent)));
        }
        // The next day's start (computed, not +24h, so local days across DST changes work).
        let tomorrow = settings.day_start_ms(day + DAY_MS + 3_600_000);
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

    /// Stops a job: one that waits won't run, a running one is stopped where it is (nothing more
    /// is posted). `None` if the job isn't waiting or running.
    pub(crate) async fn cancel(&self, id: i64) -> anyhow::Result<Option<Cancelled>> {
        if let Some(token) = self.inner.running.lock().unwrap().get(&id) {
            // A job already being cancelled is no longer "running" for a second request.
            if token.is_cancelled() {
                return Ok(None);
            }
            token.cancel();
            return Ok(Some(Cancelled::Running));
        }
        if self
            .inner
            .jobs
            .cancel_waiting(id, CANCELLED_BY_USER)
            .await?
        {
            if let Some(job) = self.inner.jobs.get(id).await? {
                self.emit(
                    kind::CANCELLED,
                    &job,
                    serde_json::json!({ "reason": CANCELLED_BY_USER }),
                )
                .await;
            }
            return Ok(Some(Cancelled::Waiting));
        }
        Ok(None)
    }

    /// Cleans up after a running job was cancelled.
    async fn cancelled(&self, job: &Job) {
        // The transcript first: once the job reads as cancelled, its record is complete.
        self.record(
            job,
            transcripts::kind::ERROR,
            serde_json::json!({ "message": "Cancelled by you; nothing more was posted." }),
        )
        .await;
        self.inner.workspace.discard(&job.repo, job.id).await;
        let _ = self
            .inner
            .jobs
            .finish(job.id, "cancelled", Some(CANCELLED_BY_USER))
            .await;
        self.emit(
            kind::CANCELLED,
            job,
            serde_json::json!({ "reason": CANCELLED_BY_USER }),
        )
        .await;
    }

    /// Posts a verification's result: an approval when everything was addressed (and, for a
    /// PR waiting in the queue, takes it out as approved), otherwise threads for what remains.
    #[allow(clippy::too_many_arguments)]
    async fn finish_verification(
        &self,
        job: &Job,
        token: &str,
        reviewer: &str,
        review_id: i64,
        policy: &crate::policy::EffectivePolicy,
        output: Result<&ReviewOutput, &str>,
        checked: usize,
    ) -> anyhow::Result<()> {
        let inner = &self.inner;
        let passed = output.is_ok_and(publish::verification_passed);
        let decision = if passed {
            Decision::AgentOnly
        } else {
            Decision::NeedsHuman
        };
        inner.reviews.set_decision(review_id, decision, &[]).await?;
        let summary = if passed {
            "verified: everything was addressed; approving"
        } else {
            "verification found issues that remain"
        };
        self.record(
            job,
            transcripts::kind::DECISION,
            serde_json::json!({ "decision": decision, "reasons": [], "summary": summary, "verification": true }),
        )
        .await;
        self.emit(
            kind::DECIDED,
            job,
            serde_json::json!({
                "review_id": review_id, "decision": decision, "reasons": [],
                "summary": summary, "verification": true,
            }),
        )
        .await;
        let target = Target {
            repo: &job.repo,
            number: job.pr_number,
            head_sha: &job.head_sha,
            reviewer,
            policy_scope: &policy.scope,
            policy_version_id: policy.version_id,
            agent_verdicts: inner.settings.get().agent_verdicts,
            run: Some(job.id),
        };
        let submitted =
            publish::publish_verification(&inner.github, token, &target, output, checked).await?;
        let mut prior = inner.publications.prior(&job.repo, job.pr_number).await?;
        prior.agent_review = Some((submitted.id, submitted.html_url.clone()));
        inner
            .publications
            .save(
                &job.repo,
                job.pr_number,
                &job.head_sha,
                prior.last_decision.unwrap_or(decision),
                &prior,
            )
            .await?;
        inner
            .reviews
            .set_published(review_id, submitted.id, &submitted.html_url)
            .await?;
        self.record(
            job,
            transcripts::kind::PUBLISHED,
            serde_json::json!({
                "as": "review",
                "state": submitted.state,
                "url": submitted.html_url,
                "body": submitted.body,
            }),
        )
        .await;
        if passed {
            inner
                .queue
                .approved_by_verification(&job.repo, job.pr_number)
                .await
                .map_err(|e| anyhow::anyhow!(e))?;
        }
        self.emit(
            kind::PUBLISHED,
            job,
            serde_json::json!({
                "review_id": review_id, "decision": decision, "url": submitted.html_url,
                "verification": true, "approved": passed,
            }),
        )
        .await;
        inner.jobs.finish(job.id, "done", None).await?;
        Ok(())
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
            agent_verdicts: inner.settings.get().agent_verdicts,
            run: job.manual().then_some(job.id),
        };
        self.record(
            job,
            transcripts::kind::DECISION,
            serde_json::json!({
                "decision": verdict.decision,
                "reasons": verdict.reasons,
                "summary": publish::describe(verdict.decision, &verdict.reasons),
            }),
        )
        .await;
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
        self.record(
            job,
            transcripts::kind::PUBLISHED,
            serde_json::json!({
                "as": if verdict.decision == Decision::AgentOnly { "review" } else { "comment" },
                "state": submitted.state,
                "url": submitted.html_url,
                "body": submitted.body,
            }),
        )
        .await;
        // Only once the comment link is stored, so the queue never shows an item without it.
        if verdict.decision == Decision::NeedsHuman {
            inner
                .queue
                .add(
                    &job.repo,
                    job.pr_number,
                    &job.head_sha,
                    review_id,
                    &job.title,
                )
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
        self.record_result(job, &first, &parsed).await;
        let Err(problem) = parsed else {
            return Ok((first.text, parsed, cost));
        };
        tracing::info!(job = job.id, %problem, "review answer invalid; asking Claude to repair it");
        let repair = prompt::repair_message(&problem);
        self.record(
            job,
            transcripts::kind::REPAIR,
            serde_json::json!({ "prompt": repair }),
        )
        .await;
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
        self.record_result(job, &second, &parsed).await;
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
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<StreamItem>();
        let forward = {
            let this = self.clone();
            let job = job.clone();
            tokio::spawn(async move {
                while let Some(item) = rx.recv().await {
                    this.on_stream_item(&job, item).await;
                }
            })
        };
        let settings = self.inner.settings.get();
        let options = crate::claude::RunOptions {
            model: settings.claude_model.clone(),
            timeout: settings.review_timeout(),
            max_budget_usd: match (settings.review_budget_usd, budget) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            },
        };
        let result = self
            .inner
            .claude
            .run(
                &checkout.worktree,
                &checkout.material,
                prompt,
                resume,
                &options,
                &crate::review::schema(),
                |item| {
                    let _ = tx.send(item);
                },
            )
            .await;
        drop(tx);
        let _ = forward.await;
        result
    }

    /// Records what Claude did in the transcript and tells the Feed.
    async fn on_stream_item(&self, job: &Job, item: StreamItem) {
        match item {
            StreamItem::Text(text) => {
                self.record(
                    job,
                    transcripts::kind::ASSISTANT,
                    serde_json::json!({ "text": text }),
                )
                .await;
                let text: String = text.chars().take(4000).collect();
                self.emit(kind::OUTPUT, job, serde_json::json!({ "text": text }))
                    .await;
            }
            StreamItem::ToolUse { id, name, input } => {
                let target = ["file_path", "path", "pattern"]
                    .iter()
                    .find_map(|key| input.get(*key).and_then(|v| v.as_str()))
                    .unwrap_or_default()
                    .to_owned();
                self.record(
                    job,
                    transcripts::kind::TOOL_USE,
                    serde_json::json!({ "id": id, "name": name, "input": input }),
                )
                .await;
                self.emit(
                    kind::TOOL,
                    job,
                    serde_json::json!({ "tool": name, "target": target }),
                )
                .await;
            }
            StreamItem::ToolResult {
                id,
                content,
                is_error,
            } => {
                self.record(
                    job,
                    transcripts::kind::TOOL_RESULT,
                    serde_json::json!({ "id": id, "content": content, "is_error": is_error }),
                )
                .await;
            }
        }
    }

    async fn record(&self, job: &Job, kind: &str, content: serde_json::Value) {
        self.inner
            .transcripts
            .append(job.id, job.attempts, kind, content)
            .await;
    }

    async fn record_result(
        &self,
        job: &Job,
        answer: &crate::claude::ClaudeAnswer,
        parsed: &Result<ReviewOutput, String>,
    ) {
        let mut content = serde_json::json!({
            "cost_usd": answer.cost_usd,
            "duration_ms": answer.duration_ms,
            "valid": parsed.is_ok(),
        });
        match parsed {
            Ok(output) => content["output"] = serde_json::to_value(output).unwrap_or_default(),
            Err(reason) => {
                content["invalid_reason"] = reason.clone().into();
                content["text"] = answer.text.clone().into();
            }
        }
        self.record(job, transcripts::kind::RESULT, content).await;
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

/// What a verification run gets beyond a normal review.
struct VerifyInput {
    /// The findings of the review being verified.
    previous: Vec<crate::review::Finding>,
    threads: Vec<crate::github::ReviewThread>,
}

/// What a run of Claude produced, before anything is stored or published.
struct ClaudeRun {
    policy: crate::policy::EffectivePolicy,
    answer: String,
    /// With anything credential-like already redacted.
    output: Result<ReviewOutput, String>,
    cost: Option<f64>,
    duration_ms: i64,
    /// Why this review can't be trusted on its own: prompt injection or hidden characters in
    /// the PR, credentials in the answer.
    alarms: Vec<decider::Reason>,
}

/// Escalates `verdict` for the run's alarms (first, as they matter most).
fn sound_alarms(verdict: &mut decider::Verdict, alarms: &[decider::Reason]) {
    if !alarms.is_empty() {
        verdict.decision = Decision::NeedsHuman;
        verdict.reasons.splice(0..0, alarms.iter().cloned());
    }
}

/// Whether the review may have been manipulated, so nothing — not even an approval the user
/// asked for — may rest on it.
fn compromised(verdict: &decider::Verdict) -> bool {
    verdict.reasons.iter().any(|r| {
        matches!(
            r,
            decider::Reason::PromptInjection { .. }
                | decider::Reason::HiddenCharacters { .. }
                | decider::Reason::SecretRedacted { .. }
        )
    })
}

/// The result of a dry run.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct DryRun {
    pub decision: Decision,
    pub reasons: Vec<decider::Reason>,
    pub review: Option<ReviewOutput>,
    pub invalid_reason: Option<String>,
    pub cost_usd: Option<f64>,
    pub policy_scope: String,
    pub policy_version_id: i64,
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
