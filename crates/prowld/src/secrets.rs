//! Keeps credentials out of what PRowl posts. Claude can only read the checkout, but a PR may
//! commit a key, or try to talk Claude into quoting one; anything that looks like a credential
//! — and PRowl's own GitHub token, exactly — is redacted from the review before it's published.

use std::sync::LazyLock;

use regex::Regex;

use crate::review::ReviewOutput;

pub(crate) const REDACTED: &str = "[redacted secret]";

static PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----[\s\S]*?(-----END [A-Z0-9 ]*PRIVATE KEY-----|\z)",
        r"\bgh[pousr]_[A-Za-z0-9]{36,}\b",
        r"\bgithub_pat_[A-Za-z0-9_]{40,}\b",
        r"\bglpat-[A-Za-z0-9_\-]{20,}\b",
        r"\b(AKIA|ASIA)[0-9A-Z]{16}\b",
        r"\bsk-ant-[A-Za-z0-9_\-]{20,}",
        r"\bsk-(proj-)?[A-Za-z0-9_\-]{32,}",
        r"\bxox[abposr]-[A-Za-z0-9\-]{10,}",
        r"\bAIza[0-9A-Za-z_\-]{35}\b",
        r"\bnpm_[A-Za-z0-9]{36}\b",
        r"\b(sk|rk)_live_[A-Za-z0-9]{20,}\b",
        r#"(?i)\b(aws_secret_access_key|client_secret|api[_-]?key|secret[_-]?key|password|passwd)\b\s*[:=]\s*["']?[A-Za-z0-9/+_\-]{16,}"#,
    ]
    .into_iter()
    .map(|p| Regex::new(p).expect("valid pattern"))
    .collect()
});

/// Replaces credentials in `text`; `known` are exact secrets PRowl holds (its tokens).
/// Returns the redacted text and how many were found.
pub(crate) fn redact(text: &str, known: &[&str]) -> (String, usize) {
    let mut out = text.to_owned();
    let mut count = 0;
    for secret in known.iter().filter(|s| s.len() >= 8) {
        let n = out.matches(secret).count();
        if n > 0 {
            count += n;
            out = out.replace(secret, REDACTED);
        }
    }
    for re in PATTERNS.iter() {
        let n = re.find_iter(&out).count();
        if n > 0 {
            count += n;
            out = re.replace_all(&out, REDACTED).into_owned();
        }
    }
    (out, count)
}

/// Redacts every text of a review that PRowl may post. Returns how many secrets it removed.
pub(crate) fn redact_review(review: &mut ReviewOutput, known: &[&str]) -> usize {
    let mut total = 0;
    let mut fix = |text: &mut String| {
        let (clean, n) = redact(text, known);
        if n > 0 {
            *text = clean;
            total += n;
        }
    };
    fix(&mut review.summary);
    review.quick_notes.iter_mut().for_each(&mut fix);
    for rule in &mut review.matched_policy_rules {
        fix(&mut rule.evidence);
    }
    for finding in &mut review.findings {
        fix(&mut finding.title);
        fix(&mut finding.body);
        if let Some(s) = finding.suggestion.as_mut() {
            fix(s);
        }
    }
    if let Some(s) = review.suspected_prompt_injection.as_mut() {
        fix(s);
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_are_redacted() {
        let text = "Found ghp_0123456789abcdefghijklmnopqrstuvwxyzAB in config, and \
                    AKIAIOSFODNN7EXAMPLE plus api_key = \"Zx9fQ2kP0aLmN8vB4cD6\".\n\
                    -----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAA\n-----END OPENSSH PRIVATE KEY-----";
        let (clean, n) = redact(text, &[]);
        assert_eq!(n, 4, "{clean}");
        for gone in ["ghp_0123", "AKIAIOSFODNN7", "Zx9fQ2kP0", "b3BlbnNzaC1"] {
            assert!(!clean.contains(gone), "{clean}");
        }
        assert!(clean.contains("Found [redacted secret] in config"));
    }

    #[test]
    fn prowls_own_token_is_redacted_exactly() {
        let (clean, n) = redact(
            "token: gho_custom_shape_token_value",
            &["gho_custom_shape_token_value"],
        );
        assert_eq!((clean.as_str(), n), ("token: [redacted secret]", 1));
    }

    #[test]
    fn ordinary_review_text_is_untouched() {
        let text =
            "Renames `api_key` to `apiKey` in src/config.rs:12; password checks are unchanged.";
        assert_eq!(redact(text, &["short"]), (text.to_owned(), 0));
    }
}
