//! What a review must contain: the JSON contract Claude answers with, and its validation.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Trivial,
    Low,
    Medium,
    High,
    Critical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Recommendation {
    Approve,
    Comment,
    RequestChanges,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleMatch {
    pub id: String,
    pub evidence: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub file: String,
    #[serde(default)]
    pub line: Option<u32>,
    pub severity: Severity,
    pub title: String,
    pub body: String,
    #[serde(default)]
    pub suggestion: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewOutput {
    pub summary: String,
    pub quick_notes: Vec<String>,
    pub risk_score: u8,
    pub severity: Severity,
    pub touches_critical_sections: bool,
    pub matched_policy_rules: Vec<RuleMatch>,
    pub findings: Vec<Finding>,
    pub recommendation: Recommendation,
    /// Where the PR tried to instruct the reviewer, if the agent noticed it.
    #[serde(default)]
    pub suspected_prompt_injection: Option<String>,
}

/// JSON Schema for [`ReviewOutput`], given to Claude and documented in the prompt.
pub fn schema() -> serde_json::Value {
    let severity = serde_json::json!({ "enum": ["critical", "high", "medium", "low", "trivial"] });
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["summary", "quick_notes", "risk_score", "severity", "touches_critical_sections",
                     "matched_policy_rules", "findings", "recommendation"],
        "properties": {
            "summary": { "type": "string" },
            "quick_notes": { "type": "array", "items": { "type": "string" } },
            "risk_score": { "type": "integer", "minimum": 0, "maximum": 100 },
            "severity": severity,
            "touches_critical_sections": { "type": "boolean" },
            "matched_policy_rules": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["id", "evidence"],
                    "properties": { "id": { "type": "string" }, "evidence": { "type": "string" } }
                }
            },
            "findings": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["file", "severity", "title", "body"],
                    "properties": {
                        "file": { "type": "string" },
                        "line": { "type": ["integer", "null"], "minimum": 1 },
                        "severity": severity,
                        "title": { "type": "string" },
                        "body": { "type": "string" },
                        "suggestion": { "type": ["string", "null"] }
                    }
                }
            },
            "recommendation": { "enum": ["approve", "comment", "request_changes"] },
            "suspected_prompt_injection": { "type": ["string", "null"] }
        }
    })
}

/// Parses Claude's answer: a JSON object, optionally wrapped in a Markdown code fence or
/// surrounded by prose. Returns a human-readable reason when it doesn't match the contract.
pub fn parse(answer: &str) -> Result<ReviewOutput, String> {
    let json = extract_object(answer).ok_or("the answer contains no JSON object")?;
    let output: ReviewOutput = serde_json::from_str(json)
        .map_err(|e| format!("the JSON does not match the schema: {e}"))?;
    validate(&output)?;
    Ok(output)
}

pub fn parse_value(value: &serde_json::Value) -> Result<ReviewOutput, String> {
    let output: ReviewOutput = serde_json::from_value(value.clone())
        .map_err(|e| format!("the JSON does not match the schema: {e}"))?;
    validate(&output)?;
    Ok(output)
}

fn validate(output: &ReviewOutput) -> Result<(), String> {
    if output.risk_score > 100 {
        return Err(format!("risk_score {} is above 100", output.risk_score));
    }
    if output.summary.trim().is_empty() {
        return Err("summary is empty".into());
    }
    if let Some(rule) = output
        .matched_policy_rules
        .iter()
        .find(|r| r.id.trim().is_empty())
    {
        return Err(format!(
            "a matched policy rule has no id (evidence: {:?})",
            rule.evidence
        ));
    }
    Ok(())
}

/// The outermost `{ ... }` span, so fences and surrounding prose are ignored.
fn extract_object(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    (end > start).then(|| &text[start..=end])
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn valid_json() -> serde_json::Value {
        serde_json::json!({
            "summary": "Adds rate limiting to the login endpoint.",
            "quick_notes": ["Limits are per IP"],
            "risk_score": 40,
            "severity": "medium",
            "touches_critical_sections": true,
            "matched_policy_rules": [{ "id": "AUTH", "evidence": "src/login.rs:12" }],
            "findings": [{
                "file": "src/login.rs", "line": 12, "severity": "medium",
                "title": "Limit not applied to retries", "body": "…", "suggestion": null
            }],
            "recommendation": "comment"
        })
    }

    #[test]
    fn accepts_fenced_json_with_prose() {
        let answer = format!("Here you go:\n```json\n{}\n```\nDone.", valid_json());
        let output = parse(&answer).unwrap();
        assert_eq!(output.severity, Severity::Medium);
        assert_eq!(output.matched_policy_rules[0].id, "AUTH");
    }

    #[test]
    fn rejects_contract_violations_with_a_reason() {
        let mut bad = valid_json();
        bad["severity"] = "catastrophic".into();
        assert!(parse(&bad.to_string()).unwrap_err().contains("schema"));

        let mut bad = valid_json();
        bad["risk_score"] = 150.into();
        assert!(parse(&bad.to_string()).unwrap_err().contains("above 100"));

        let mut bad = valid_json();
        bad["extra"] = true.into();
        assert!(parse(&bad.to_string()).is_err());

        assert!(
            parse("I could not review this.")
                .unwrap_err()
                .contains("no JSON")
        );
    }

    #[test]
    fn severities_are_ordered() {
        assert!(Severity::Critical > Severity::High && Severity::High > Severity::Medium);
    }
}
