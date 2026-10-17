//! Every review run leaves a transcript: the context Claude got, what it did, and the outcome.

mod common;

use common::*;
use prowl_e2e::ClaudeResponse;

fn kinds(entries: &[prowld::transcripts::TranscriptEntry]) -> Vec<&str> {
    entries.iter().map(|e| e.kind.as_str()).collect()
}

#[tokio::test]
async fn a_review_records_its_context_conversation_and_outcome() {
    let (h, _) = setup(&[7]).await;
    h.claude
        .respond_with(vec![ClaudeResponse::review_after_reading(
            &calm_review(),
            "pr.diff",
            "diff --git a/src/login_7.rs b/src/login_7.rs",
        )])
        .unwrap();

    h.client().poll_now().await.unwrap();
    let job = wait_for_job(&h, finished("done")).await;
    let transcript = h.client().transcript(job.id).await.unwrap();

    assert_eq!(
        kinds(&transcript),
        [
            "context",
            "assistant",
            "tool_use",
            "tool_result",
            "assistant",
            "result",
            "decision",
            "published"
        ]
    );
    let context = &transcript[0].content;
    let prompt = context["prompt"].as_str().unwrap();
    assert!(
        prompt.contains("repository: acme/api\nnumber: 7"),
        "{prompt}"
    );
    assert!(
        prompt.contains("ALWAYS require human review"),
        "the policy is in the prompt"
    );
    assert_eq!(context["policy_scope"], "global");
    assert!(
        context["material_files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f == "pr.diff")
    );
    assert_eq!(transcript[2].content["name"], "Read");
    assert_eq!(transcript[2].content["input"]["file_path"], "pr.diff");
    assert!(
        transcript[3].content["content"]
            .as_str()
            .unwrap()
            .starts_with("diff --git")
    );
    let result = &transcript[5].content;
    assert_eq!(result["valid"], true);
    assert_eq!(result["cost_usd"], 0.07);
    assert_eq!(result["duration_ms"], 4200);
    assert_eq!(result["output"]["summary"], calm_review()["summary"]);
    assert_eq!(transcript[6].content["decision"], "AGENT_ONLY");
    let published = &transcript[7].content;
    assert_eq!(published["as"], "review");
    assert!(
        published["url"]
            .as_str()
            .unwrap()
            .contains("pullrequestreview")
    );
    assert!(transcript.iter().all(|e| e.job_id == job.id));

    let tools: Vec<_> = events(&h)
        .await
        .into_iter()
        .filter(|e| e.kind == "review.tool")
        .collect();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].payload["tool"], "Read");
    assert_eq!(tools[0].payload["target"], "pr.diff");
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_repaired_answer_shows_both_tries() {
    let (h, _) = setup(&[7]).await;
    h.claude
        .respond_with(vec![
            ClaudeResponse::answer("not json at all"),
            ClaudeResponse::review(&calm_review()),
        ])
        .unwrap();

    h.client().poll_now().await.unwrap();
    let job = wait_for_job(&h, finished("done")).await;
    let transcript = h.client().transcript(job.id).await.unwrap();

    let kinds = kinds(&transcript);
    let first = kinds.iter().position(|k| *k == "result").unwrap();
    assert_eq!(transcript[first].content["valid"], false);
    assert_eq!(transcript[first].content["text"], "not json at all");
    assert_eq!(kinds[first + 1], "repair");
    let last = kinds.iter().rposition(|k| *k == "result").unwrap();
    assert!(last > first);
    assert_eq!(transcript[last].content["valid"], true);
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_unknown_job_has_an_empty_transcript() {
    let (h, _) = setup(&[7]).await;
    assert!(h.client().transcript(424242).await.unwrap().is_empty());
    h.shutdown().await.unwrap();
}
