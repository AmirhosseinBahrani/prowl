# The Review Policy

The Review Policy tells Claude what matters in your code and decides which pull requests must
wait for a human. It's plain Markdown that you own; PRowl includes it in the first message of
every review.

## Structure

```markdown
## ALWAYS require human review
- [AUTH] Authentication, sessions, tokens, permissions, role or access checks
- [MONEY] Payments, balances, pricing, fees, invoicing or ledger logic

## Agent may handle alone
- Documentation, comments and typo fixes

## Extra instructions for the reviewer
- We use Axum and sqlx; anything under `src/billing/` is money code.
```

- Every rule under **ALWAYS require human review** needs an **ID in square brackets**. When
  Claude reports that a change matches a rule, it cites the ID — and the PR goes to your queue,
  with the rule shown as the reason.
- If Claude cites an ID that isn't in the policy, PRowl treats it as a match anyway (better safe)
  and shows it as an unknown rule.
- The other sections are guidance; write whatever helps a reviewer understand your codebase.

The default policy ships with `AUTH`, `MONEY`, `DATA`, `INFRA`, `SECRETS` and `DEPS` rules.

## How the decision is made

Claude's review is one input; PRowl makes the decision. A PR needs a human if **any** of these
hold:

| Signal | Example reason shown |
|---|---|
| A policy rule matched | `[MONEY] invoice rounding changed` |
| Claude flagged a critical section | critical code |
| Severity is `high` or `critical` | severity high |
| Risk score ≥ `risk_threshold` (default 60) | risk 72 |
| A [file rule](configuration.md#file-rules) matched | `migrations/**` |
| The PR is larger than the size limits | too large (no Claude run) |
| Claude's answer was unusable after one repair attempt | unusable review |

Signals only ever escalate. Nothing Claude says can move a PR from "needs a human" to "agent only".

## Where it lives

Policies are files under your PRowl home (see [configuration](configuration.md#files)):

```
policies/
  global.md                    # applies everywhere
  repos/<owner>/<repo>.md      # optional override for one repository
```

An override **replaces** the global policy for that repository. You can edit the files in any
editor — PRowl notices changes within a couple of seconds and records a new version.

## Working with it in the TUI (Policy tab)

| Key | Action |
|---|---|
| `↑`/`↓` | Choose global or a repository override |
| `e` | Edit in `$EDITOR` (`$VISUAL`, falling back to a platform default) |
| `n` | Create an override for a repository |
| `x` | Delete an override (asks first) |
| `h` | Version history with diffs; `r` restores the selected version |
| `t` | Dry run: review a PR with the current policy without posting anything |

Every finished review records the policy version it used, so History always shows which rules
were in force.

## Learning from comments

PRowl can improve a repository's policy from what its people say about PRowl's reviews. Turn on
**Settings → Learn the policy from reviewers' comments**; every 6 hours it reads the new
feedback on PRs it reviewed, and `L` in the Policy tab runs it now (for the selected repository,
or every watched one when the global policy is selected). It reads:

- **Replies to PRowl's threads** — "false positive", "good catch", explanations — and threads
  resolved without the code changing;
- **Issues other reviewers raised** in their own threads and reviews — what PRowl missed;
- **Your Human Review Queue decisions** — PRs you approved as they were although PRowl sent them
  to you for a rule, and PRs where you requested changes.

A Claude run turns that into changes — new or narrower rules, extra guidance for the reviewer —
and PRowl **applies them** as a new version of that repository's override (created from the
global policy when it has none; the global policy itself only changes when you edit it). The
version is marked `learned`, the Feed lists what changed and why, and `h` → `r` restores any
earlier version.

Guard rails:

- Only **collaborators** count — GitHub's author association OWNER, MEMBER or COLLABORATOR;
  drive-by comments are ignored.
- Comments that look like prompt injection are dropped before Claude sees them, and the comments
  are given to it as untrusted data.
- A learned policy with no "ALWAYS require human review" rule left is refused.
- Each piece of feedback is read once; a failed run reads it again next time.
- Learning runs use Claude and count toward the daily budget.

## Tips

- Keep rule descriptions concrete: name directories, modules and kinds of change.
- Prefer a few precise rules over many vague ones; vague rules make everything need a human.
- Use dry runs on a handful of past PRs after changing the policy. Dry runs count toward the
  daily budget.
