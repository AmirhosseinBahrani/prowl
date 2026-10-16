//! The Gate: decides, using only GitHub data, whether a pull request may be reviewed by Claude.
//! Nothing here calls Claude; a PR only reaches the review queue if every check passes.

use serde::{Deserialize, Serialize};

/// Why a pull request was not queued for review.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateReason {
    RepoNotWatched,
    NotOpen,
    OwnPullRequest,
    Draft,
    NotRequested,
    AlreadyQueued,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateOutcome {
    Enqueue,
    Skip(GateReason),
}

/// What the Gate knows about one pull request.
#[derive(Debug, Clone)]
pub struct PrFacts<'a> {
    /// `owner/repo`.
    pub repo: &'a str,
    pub open: bool,
    pub draft: bool,
    pub author: &'a str,
    pub requested_users: &'a [&'a str],
    pub requested_teams: &'a [&'a str],
}

/// What the Gate knows about the user and their settings.
#[derive(Debug, Clone)]
pub struct GateContext<'a> {
    pub me: &'a str,
    /// `(organisation, team slug)` pairs the user belongs to.
    pub my_teams: &'a [(String, String)],
    pub include_team_requests: bool,
    pub skip_drafts: bool,
    pub repo_watched: bool,
    pub already_queued: bool,
}

pub fn evaluate(pr: &PrFacts<'_>, ctx: &GateContext<'_>) -> GateOutcome {
    use GateOutcome::Skip;
    use GateReason::*;

    if !ctx.repo_watched {
        return Skip(RepoNotWatched);
    }
    if !pr.open {
        return Skip(NotOpen);
    }
    if pr.author.eq_ignore_ascii_case(ctx.me) {
        return Skip(OwnPullRequest);
    }
    if pr.draft && ctx.skip_drafts {
        return Skip(Draft);
    }
    let directly = pr
        .requested_users
        .iter()
        .any(|u| u.eq_ignore_ascii_case(ctx.me));
    let org = pr.repo.split('/').next().unwrap_or_default();
    let via_team = ctx.include_team_requests
        && pr.requested_teams.iter().any(|slug| {
            ctx.my_teams
                .iter()
                .any(|(o, s)| o.eq_ignore_ascii_case(org) && s.eq_ignore_ascii_case(slug))
        });
    if !directly && !via_team {
        return Skip(NotRequested);
    }
    if ctx.already_queued {
        return Skip(AlreadyQueued);
    }
    GateOutcome::Enqueue
}

#[cfg(test)]
mod tests {
    use super::*;
    use GateOutcome::*;
    use GateReason::*;

    fn pr<'a>(users: &'a [&'a str], teams: &'a [&'a str]) -> PrFacts<'a> {
        PrFacts {
            repo: "acme/api",
            open: true,
            draft: false,
            author: "alice",
            requested_users: users,
            requested_teams: teams,
        }
    }

    fn ctx<'a>(teams: &'a [(String, String)]) -> GateContext<'a> {
        GateContext {
            me: "Octo",
            my_teams: teams,
            include_team_requests: false,
            skip_drafts: true,
            repo_watched: true,
            already_queued: false,
        }
    }

    #[test]
    fn direct_request_is_enqueued_case_insensitively() {
        assert_eq!(evaluate(&pr(&["octo"], &[]), &ctx(&[])), Enqueue);
    }

    #[test]
    fn each_check_has_its_reason() {
        let none: &[(String, String)] = &[];
        let base = pr(&["octo"], &[]);
        let cases: Vec<(PrFacts, GateContext, GateReason)> = vec![
            (
                base.clone(),
                GateContext {
                    repo_watched: false,
                    ..ctx(none)
                },
                RepoNotWatched,
            ),
            (
                PrFacts {
                    open: false,
                    ..base.clone()
                },
                ctx(none),
                NotOpen,
            ),
            (
                PrFacts {
                    author: "OCTO",
                    ..base.clone()
                },
                ctx(none),
                OwnPullRequest,
            ),
            (
                PrFacts {
                    draft: true,
                    ..base.clone()
                },
                ctx(none),
                Draft,
            ),
            (pr(&["someone-else"], &[]), ctx(none), NotRequested),
            (
                base.clone(),
                GateContext {
                    already_queued: true,
                    ..ctx(none)
                },
                AlreadyQueued,
            ),
        ];
        for (facts, context, reason) in cases {
            assert_eq!(evaluate(&facts, &context), Skip(reason), "{reason:?}");
        }
    }

    #[test]
    fn drafts_are_allowed_when_configured() {
        let facts = PrFacts {
            draft: true,
            ..pr(&["octo"], &[])
        };
        let context = GateContext {
            skip_drafts: false,
            ..ctx(&[])
        };
        assert_eq!(evaluate(&facts, &context), Enqueue);
    }

    #[test]
    fn team_requests_count_only_when_enabled_and_in_the_same_org() {
        let teams = vec![("acme".to_string(), "backend".to_string())];
        let facts = pr(&[], &["backend"]);
        assert_eq!(evaluate(&facts, &ctx(&teams)), Skip(NotRequested));

        let on = GateContext {
            include_team_requests: true,
            ..ctx(&teams)
        };
        assert_eq!(evaluate(&facts, &on), Enqueue);

        let other_org = vec![("globex".to_string(), "backend".to_string())];
        let on = GateContext {
            include_team_requests: true,
            ..ctx(&other_org)
        };
        assert_eq!(evaluate(&facts, &on), Skip(NotRequested));
    }
}
