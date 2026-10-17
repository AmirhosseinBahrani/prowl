# Configuration

PRowl is configured from the **Settings** tab of the TUI. Changes apply immediately — no restart —
and are stored in the daemon's database. Scripts can use the Local API instead
(`GET`/`PUT /v1/settings`, see [architecture](architecture.md#local-api)).

## Settings

| Setting | Default | Range | What it does |
|---|---|---|---|
| `poll_interval_secs` | `60` | 10–86400 | How often GitHub is checked for review requests. Polling never uses Claude. |
| `review_concurrency` | `2` | 1–16 | Reviews running at the same time. |
| `claude_model` | CLI default | — | Passed to `claude --model` (e.g. `sonnet`, `opus`). |
| `review_timeout_secs` | `900` | 30–7200 | A review still running after this long is stopped and retried. |
| `review_budget_usd` | none | > 0 | Spend cap per Claude run (`claude --max-budget-usd`). |
| `daily_budget_usd` | none | > 0 | Claude spend allowed per day. When reached, reviews wait for the next day. Dry runs count too. |
| `budget_day` | `utc` | `utc`, `local` | When the daily budget resets: UTC midnight or local midnight. |
| `risk_threshold` | `60` | 1–100 | A review whose risk score reaches this needs a human. |
| `max_changed_lines` | `70000` | ≥ 1 | Larger PRs (added + deleted lines) go straight to a human, without a Claude run. |
| `max_changed_files` | `350` | ≥ 1 | Likewise for the number of changed files. |
| `sleep_mode` | `ac_only` with a battery, else `always` | `always`, `ac_only`, `during_reviews` | When PRowl keeps the machine awake. See [sleep](#sleep). |
| `skip_drafts` | `true` | — | Ignore draft PRs until they're marked ready. |
| `include_team_requests` | `false` | — | Also review PRs requested from a team you're in, not only from you personally. |
| `agent_verdicts` | `false` | — | On PRs that don't need a human, post Claude's recommendation as the verdict: **approve** or **request changes** instead of a comment. Earlier agent approvals are dismissed when a newer commit needs a human. |
| `notifications` | `true` | — | Desktop notifications when a PR needs you or a review fails. |
| `file_rules` | none | — | Per-repository globs that always need a human. See below. |

### File rules

File rules are a safety net that doesn't depend on Claude: if a PR touches a matching file, it
needs a human, whatever the review says. Add them per repository in the Settings tab (`a` to add,
`x` to remove). Patterns are globs matched against paths in the repository:

```
acme/api      migrations/**   .github/workflows/**   **/*.tf
acme/billing  src/ledger/**
```

### Sleep

Reviews can't happen while the machine sleeps. PRowl asks the OS to stay awake (the display may
still sleep) according to `sleep_mode`:

- `always` — whenever the daemon runs.
- `ac_only` — only on mains power; on battery the machine sleeps normally.
- `during_reviews` — only while a review is running.

After the machine wakes, PRowl notices the gap and polls straight away, so nothing is missed.
On Linux this uses `systemd-inhibit`; on macOS a power assertion; on Windows
`SetThreadExecutionState`.

## Files

Everything PRowl stores lives in one directory, the **PRowl home**:

| Platform | Default home |
|---|---|
| macOS | `~/Library/Application Support/dev.prowl.prowl` |
| Linux | `$XDG_DATA_HOME/prowl` (usually `~/.local/share/prowl`) |
| Windows | `%LOCALAPPDATA%\prowl\prowl\data` |

Override it with `--home <dir>` or `PROWL_HOME` (both `prowl` and `prowld` accept it). Inside:

| Path | Contents |
|---|---|
| `prowl.db` | SQLite database: jobs, reviews, queue, events, settings, policy versions |
| `api-token` | Secret for the Local API (readable only by you) |
| `prowld.sock` | Local API socket (Unix); Windows uses a named pipe |
| `github-client-id` | Optional OAuth App client id |
| `github-token` | GitHub token — only when no OS keychain is available |
| `policies/` | `global.md` and `repos/<owner>/<repo>.md` overrides |
| `git/` | Bare clones of watched repositories |
| `wt/` | Temporary worktrees for reviews in progress |
| `review/` | Temporary review material (diff, commits) for reviews in progress |
| `logs/` | Service logs |

## Environment variables

| Variable | Used by | Meaning |
|---|---|---|
| `PROWL_HOME` | `prowl`, `prowld` | PRowl home directory |
| `PROWL_GITHUB_CLIENT_ID` | `prowld` | OAuth App client id (overrides the file and the built-in one) |
| `PROWL_UPDATE_REPO` | `prowld` | Where to look for newer releases (default `AmirhosseinBahrani/prowl`); set it empty to turn the update check off |
| `RUST_LOG` | `prowld` | Log filter, e.g. `prowld=debug` |
| `VISUAL`, `EDITOR` | `prowl` | Editor for policies |
| `NO_COLOR` | `prowl` | Disable colours in the TUI |

## Updates

Every 6 hours (and soon after you connect GitHub) the daemon asks GitHub for PRowl's latest
release — one small request with your token, nothing else is sent. When a newer version exists,
the TUI's bottom bar shows `⬆ vX.Y.Z available` and the Feed says so once. To update, run the
installer again (press `?` in the TUI for the exact command); it replaces the binaries and
restarts the background service. Set `PROWL_UPDATE_REPO` to an empty value to turn the check off.
