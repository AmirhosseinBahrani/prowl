//! Non-interactive commands for scripts and quick checks: `prowl queue`, `prowl approve`, …

use prowld::ApiClient;

use crate::policy_tab::parse_pr_ref;
use crate::policy_ui::reason_text;
use crate::queue_tab::QueueAction;

/// The Human Review Queue as plain text, riskiest first.
pub async fn queue(client: &ApiClient) -> anyhow::Result<String> {
    let items = client.queue().await?;
    if items.is_empty() {
        return Ok("Nothing is waiting for your review.\n".into());
    }
    let mut out = String::new();
    for item in items {
        let severity = item
            .severity
            .map(|s| format!("{s:?}").to_lowercase())
            .unwrap_or_else(|| "unusable".into());
        out.push_str(&format!(
            "{}#{}  [{severity}]  {}\n",
            item.repo, item.pr_number, item.title
        ));
        for reason in &item.reasons {
            out.push_str(&format!("    {}\n", reason_text(reason)));
        }
        out.push_str(&format!("    {}\n", item.html_url));
    }
    Ok(out)
}

/// Queues `owner/repo#n` to be reviewed again at its current commit.
pub async fn rerun(client: &ApiClient, pr: &str) -> anyhow::Result<String> {
    let (repo, number) = parse_pr_ref(pr)
        .ok_or_else(|| anyhow::anyhow!("expected owner/repo#number, got {pr:?}"))?;
    let job = client.rerun(&repo, number).await?;
    Ok(format!(
        "Re-reviewing {repo}#{number} at {} (job {}). Follow it in prowl's Reviews tab.\n",
        job.head_sha.chars().take(7).collect::<String>(),
        job.id
    ))
}

/// Checks `owner/repo#n`'s findings were addressed, approving it if so.
pub async fn verify(client: &ApiClient, pr: &str) -> anyhow::Result<String> {
    let (repo, number) = parse_pr_ref(pr)
        .ok_or_else(|| anyhow::anyhow!("expected owner/repo#number, got {pr:?}"))?;
    let job = client.verify(&repo, number).await?;
    Ok(format!(
        "Verifying {repo}#{number} at {} (job {}): PRowl approves it if every finding was \
         addressed. Follow it in prowl's Reviews tab.\n",
        job.head_sha.chars().take(7).collect::<String>(),
        job.id
    ))
}

/// Stops every waiting or running review of `owner/repo#n`.
pub async fn cancel(client: &ApiClient, pr: &str) -> anyhow::Result<String> {
    let (repo, number) = parse_pr_ref(pr)
        .ok_or_else(|| anyhow::anyhow!("expected owner/repo#number, got {pr:?}"))?;
    let active: Vec<prowld::Job> = client
        .jobs()
        .await?
        .into_iter()
        .filter(|j| {
            j.repo.eq_ignore_ascii_case(&repo)
                && j.pr_number == number
                && matches!(j.status.as_str(), "queued" | "running" | "interrupted")
        })
        .collect();
    if active.is_empty() {
        return Ok(format!(
            "Nothing is waiting or running for {repo}#{number}.\n"
        ));
    }
    let mut out = String::new();
    for job in active {
        let what = match client.cancel_job(job.id).await? {
            prowld::Cancelled::Waiting => "cancelled before it started",
            prowld::Cancelled::Running => "stopped",
        };
        out.push_str(&format!(
            "{repo}#{number} ({}, job {}): {what}.\n",
            job.kind, job.id
        ));
    }
    Ok(out)
}

/// Approves, requests changes on, or dismisses `owner/repo#n` from the queue.
pub async fn resolve(
    client: &ApiClient,
    pr: &str,
    action: QueueAction,
    message: &str,
) -> anyhow::Result<String> {
    let (repo, number) = parse_pr_ref(pr)
        .ok_or_else(|| anyhow::anyhow!("expected owner/repo#number, got {pr:?}"))?;
    match action {
        QueueAction::Approve => client.approve(&repo, number, message).await?,
        QueueAction::RequestChanges => {
            anyhow::ensure!(
                !message.trim().is_empty(),
                "say what needs to change with -m"
            );
            client.request_changes(&repo, number, message).await?
        }
        QueueAction::Dismiss => client.dismiss(&repo, number).await?,
    };
    Ok(format!("{} {repo}#{number}.\n", action.past_tense()))
}
