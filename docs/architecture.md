# Architecture

PRowl is a Rust workspace with three crates:

| Crate | Binary | Role |
|---|---|---|
| `crates/prowld` | `prowld`, `prowld-service` | The daemon: polling, reviews, publishing, storage, Local API. Also a library (`prowld::ApiClient`, API types) that the TUI uses. |
| `crates/prowl` | `prowl` | The terminal UI and the scripting CLI (`prowl queue`, `prowl approve`, …). |
| `crates/e2e` | — | End-to-end tests: the real daemon against fake GitHub, fake Claude and real git. |

```
           ┌──────────────── your machine ─────────────────┐
           │                                               │
 prowl ────┼─ Local API (socket/pipe + token) ─┐           │
 (TUI/CLI) │                                   ▼           │
           │   ┌──────────────── prowld ──────────────┐    │
           │   │ Poller → Gate → Jobs → Worker ────────┼──► claude -p (read-only)
           │   │                          │            │    │
           │   │              Decider ◄───┘            │    │
           │   │                 │                     │    │
           │   │      Publisher / Queue / Notifier     │    │
           │   │   SQLite · Policies · Power · Events  │    │
           │   └──────────────────┬────────────────────┘    │
           └──────────────────────┼─────────────────────────┘
                                  ▼
                               GitHub
```

## The daemon

- **Poller** — every `poll_interval_secs`, searches GitHub for open PRs in watched repositories
  that request your review. It uses ETags so unchanged results cost nothing against the rate limit,
  and pauses when GitHub says the limit is exhausted.
- **Gate** — a pure function that decides whether a PR should be reviewed: open, not a draft
  (unless configured), not authored by you, you (or your team, if enabled) actually requested,
  and this head commit not reviewed yet. Claude is never involved before the gate passes.
- **Jobs** — one job per *(repository, PR, head commit)*. A new push queues a new job; the old
  one is superseded. Jobs survive restarts; a crashed daemon picks up where it left off.
- **Worker** — runs up to `review_concurrency` jobs. For each: checks size limits and the daily
  budget, updates a bare clone, creates a detached worktree at the PR head, writes the diff and
  commit list *outside* the worktree, runs Claude, and validates its JSON answer against a schema
  (with one repair attempt). Failures retry with exponential backoff.
- **Decider** — combines Claude's answer with the policy, thresholds and file rules into
  *agent only* or *needs a human*, plus the reasons. Escalation only.
- **Publisher** — posts the result to GitHub exactly once (a review with inline notes — a comment,
  or an approval / change request with agent verdicts on — or the findings as review threads plus a "waiting for human approval"
  comment) and sets the `prowl:agent-reviewed` or `prowl:needs-human` label. Re-reviews of a new
  commit update PRowl's earlier comment instead of adding more.
- **Queue** — PRs that need you. Approving or requesting changes posts a real review as you and
  marks the PR `prowl:human-approved` / `prowl:changes-requested`.
- **Power** — keeps the machine awake according to `sleep_mode` and detects wake-ups.
- **Events** — everything above emits events to the database and a live stream; the Feed tab,
  notifications and the status bar are built on them.

## Local API

`prowld` serves HTTP/JSON over a Unix domain socket (`<home>/prowld.sock`, or a short path under
the temp directory when the home path is too long) or a Windows named pipe. Every request needs
`Authorization: Bearer <token>`, where the token is in `<home>/api-token`, readable only by you.

```sh
curl --unix-socket "$HOME/Library/Application Support/dev.prowl.prowl/prowld.sock" \
  -H "Authorization: Bearer $(cat "$HOME/Library/Application Support/dev.prowl.prowl/api-token")" \
  http://prowld/v1/status
```

| Endpoint | |
|---|---|
| `GET /v1/status` | Version, account, counts |
| `GET /v1/events?live=true`, `GET /v1/events/recent` | Server-sent event stream, recent events |
| `GET/DELETE /v1/auth`, `POST /v1/auth/device`, `POST /v1/auth/token` | Account, device-flow login, token sign-in |
| `GET /v1/repos`, `GET /v1/repos/watched`, `PUT/DELETE /v1/repos/{owner}/{repo}/watch` | Repositories |
| `GET /v1/queue`, `GET /v1/queue/{owner}/{repo}/{n}`, `POST …/approve`, `…/request-changes`, `…/dismiss` | Human Review Queue |
| `GET /v1/policy`, `/v1/policies/…` | Policies, versions, restore |
| `GET/PUT /v1/settings` | Settings |
| `GET /v1/jobs`, `GET /v1/jobs/{id}/transcript` | Reviews, and each one's full record: context and prompt, Claude's messages and tool calls, answer, decision, what was posted |
| `GET /v1/reviews`, `GET /v1/costs`, `GET /v1/power`, `GET /v1/doctor` | Introspection |
| `POST /v1/poll`, `POST /v1/dry-run` | Poll now, dry-run a PR |

The API is for local tools and is not yet considered stable.

## The TUI

`prowl` follows an Elm-like design: `App::update(Msg) -> Vec<Cmd>` is pure state; the runtime
executes `Cmd`s (API calls, opening the editor or browser) and feeds results back as `Msg`s; `ui`
draws the state with [ratatui](https://ratatui.rs). That keeps nearly all behaviour testable
without a terminal.

## Background service

`prowld service install` registers `prowld-service` (a thin wrapper that runs the daemon) as a
per-user service: a LaunchAgent on macOS, a systemd user unit on Linux, a Task Scheduler task on
Windows. It starts at login and restarts if it stops.
