//! The Decision: can the agent's review stand alone, or must a human approve the PR?
//! Pure, and escalation-only: every signal can push a PR to a human, none can pull it back.

use serde::{Deserialize, Serialize};

use crate::review::{ReviewOutput, Severity};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Decision {
    AgentOnly,
    NeedsHuman,
}

/// Why a PR needs a human.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Reason {
    /// Claude's answer never matched the contract.
    InvalidOutput {
        detail: String,
    },
    /// A rule from the policy's ALWAYS section matched.
    PolicyRule {
        id: String,
        evidence: String,
    },
    /// Claude cited a rule ID that isn't in the policy; treated as a match to stay safe.
    UnknownPolicyRule {
        id: String,
    },
    /// Claude flagged the change as touching critical code.
    CriticalSection,
    Severity {
        severity: Severity,
    },
    RiskScore {
        score: u8,
        threshold: u8,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verdict {
    pub decision: Decision,
    pub reasons: Vec<Reason>,
}

#[derive(Debug, Clone, Copy)]
pub struct DecisionSettings {
    /// A risk score at or above this needs a human.
    pub risk_threshold: u8,
}

/// `review` is the validated output, or why it was invalid. `policy_rules` are the IDs under
/// the policy's ALWAYS heading.
pub fn decide(
    review: Result<&ReviewOutput, &str>,
    policy_rules: &[String],
    settings: DecisionSettings,
) -> Verdict {
    let output = match review {
        Ok(output) => output,
        Err(detail) => {
            return Verdict {
                decision: Decision::NeedsHuman,
                reasons: vec![Reason::InvalidOutput {
                    detail: detail.to_owned(),
                }],
            };
        }
    };
    let mut reasons = Vec::new();
    for rule in &output.matched_policy_rules {
        let id = rule.id.trim().trim_start_matches('[').trim_end_matches(']');
        if policy_rules
            .iter()
            .any(|known| known.eq_ignore_ascii_case(id))
        {
            reasons.push(Reason::PolicyRule {
                id: id.to_owned(),
                evidence: rule.evidence.clone(),
            });
        } else {
            reasons.push(Reason::UnknownPolicyRule { id: id.to_owned() });
        }
    }
    if output.touches_critical_sections {
        reasons.push(Reason::CriticalSection);
    }
    if output.severity >= Severity::High {
        reasons.push(Reason::Severity {
            severity: output.severity,
        });
    }
    if output.risk_score >= settings.risk_threshold {
        reasons.push(Reason::RiskScore {
            score: output.risk_score,
            threshold: settings.risk_threshold,
        });
    }
    Verdict {
        decision: if reasons.is_empty() {
            Decision::AgentOnly
        } else {
            Decision::NeedsHuman
        },
        reasons,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::review::{Recommendation, RuleMatch};

    fn calm() -> ReviewOutput {
        ReviewOutput {
            summary: "Docs only.".into(),
            quick_notes: vec![],
            risk_score: 5,
            severity: Severity::Trivial,
            touches_critical_sections: false,
            matched_policy_rules: vec![],
            findings: vec![],
            recommendation: Recommendation::Approve,
        }
    }

    const SETTINGS: DecisionSettings = DecisionSettings { risk_threshold: 60 };

    fn rules() -> Vec<String> {
        vec!["AUTH".into(), "MONEY".into()]
    }

    #[test]
    fn a_calm_review_is_agent_only() {
        let verdict = decide(Ok(&calm()), &rules(), SETTINGS);
        assert_eq!(
            verdict,
            Verdict {
                decision: Decision::AgentOnly,
                reasons: vec![]
            }
        );
    }

    #[test]
    fn every_signal_escalates_with_its_reason() {
        let matched = |id: &str| ReviewOutput {
            matched_policy_rules: vec![RuleMatch {
                id: id.into(),
                evidence: "a.rs:1".into(),
            }],
            ..calm()
        };
        let cases: Vec<(ReviewOutput, Reason)> = vec![
            (
                matched("AUTH"),
                Reason::PolicyRule {
                    id: "AUTH".into(),
                    evidence: "a.rs:1".into(),
                },
            ),
            (
                matched("[auth]"),
                Reason::PolicyRule {
                    id: "auth".into(),
                    evidence: "a.rs:1".into(),
                },
            ),
            (
                matched("DOCS"),
                Reason::UnknownPolicyRule { id: "DOCS".into() },
            ),
            (
                ReviewOutput {
                    touches_critical_sections: true,
                    ..calm()
                },
                Reason::CriticalSection,
            ),
            (
                ReviewOutput {
                    severity: Severity::High,
                    ..calm()
                },
                Reason::Severity {
                    severity: Severity::High,
                },
            ),
            (
                ReviewOutput {
                    severity: Severity::Critical,
                    ..calm()
                },
                Reason::Severity {
                    severity: Severity::Critical,
                },
            ),
            (
                ReviewOutput {
                    risk_score: 60,
                    ..calm()
                },
                Reason::RiskScore {
                    score: 60,
                    threshold: 60,
                },
            ),
        ];
        for (output, reason) in cases {
            let verdict = decide(Ok(&output), &rules(), SETTINGS);
            assert_eq!(verdict.decision, Decision::NeedsHuman, "{reason:?}");
            assert_eq!(verdict.reasons, vec![reason]);
        }
    }

    #[test]
    fn medium_severity_below_threshold_stays_with_the_agent() {
        let output = ReviewOutput {
            severity: Severity::Medium,
            risk_score: 59,
            ..calm()
        };
        assert_eq!(
            decide(Ok(&output), &rules(), SETTINGS).decision,
            Decision::AgentOnly
        );
    }

    #[test]
    fn invalid_output_always_needs_a_human() {
        let verdict = decide(Err("no JSON"), &rules(), SETTINGS);
        assert_eq!(verdict.decision, Decision::NeedsHuman);
        assert_eq!(
            verdict.reasons,
            vec![Reason::InvalidOutput {
                detail: "no JSON".into()
            }]
        );
    }
}
