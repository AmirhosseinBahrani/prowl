# Changelog

All notable changes to PRowl are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project follows
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- **Stop PRowl**: `Q` in the TUI quits and stops the background service (after asking);
  `prowld service stop` / `start` do the same from a shell. The service stays installed and
  starts again at your next login, or with `s` on the TUI's "not running" screen.
- **Cancel a review**: `x` in the Reviews tab (or `prowl cancel owner/repo#N`) stops a waiting
  or running review or re-review — a running one stops Claude at once and posts nothing more.

### Changed

- The help screen (`?`) is more compact, one line per tab, so it fits smaller terminals.

### Fixed

- Upgrading on macOS no longer fails with `"launchctl" failed: Bootstrap failed: 5: Input/output
  error` (leaving PRowl stopped): reinstalling the background service now waits for launchd to
  finish unloading the old one, and retries briefly.

### Security

- **Prompt-injection defences**: PRowl scans each PR's title, description, commit messages and
  added lines for text written to steer the AI reviewer and for hidden or reordering Unicode
  (Trojan Source), and the agent reports attempts it notices. Either makes the PR need a human —
  the comment shows where — and a suspect review is never approved, not even by a verification
  you ask for.
- **Credential-theft defences**: symlinks in a PR are checked out as plain files (so they can't
  point Claude at `~/.ssh` or other files outside the checkout); Claude runs with an allow-listed
  environment, so tokens and keys in the daemon's environment never reach it; and credentials —
  including PRowl's own GitHub token — are redacted from reviews before they're stored or
  posted, sending the PR to a human.

## [0.1.5] - 2026-10-17

### Added

- **Verification after resolved threads**: once every review thread PRowl opened is resolved, a
  verification agent checks the current code — is each finding really fixed, anything new? — and
  approves the PR if all is well, or opens threads for what remains. It runs on its own for
  agent-only PRs when *agent verdicts* are on (once per commit); for PRs that need a human, press
  `v` in the Queue or Reviews tab, or run `prowl verify owner/repo#N`, and an approval also
  takes the PR out of the queue. `GET /v1/threads` lists a PR's threads and whether each is
  resolved.
- **Review again**: `R` in the Queue, Reviews and History tabs (or `prowl rerun owner/repo#N`)
  reviews a pull request again at its current commit and posts a fresh review — even after
  PRowl reviewed it, when GitHub no longer lists you as a requested reviewer.

### Changed

- When a pull request needs a human, the agent's findings are posted as review threads — one per
  finding, on the line it's about — instead of inside one long comment. The "⏳ Waiting for
  human approval" comment keeps the reasons and links to the threads; findings that aren't on a
  changed line stay in the comment.
- A review in the Reviews tab now opens with its findings, each in its own box (severity,
  file and line, title, explanation, suggestion), and shows Claude's conversation and context
  below them. While a review runs, the findings appear as soon as Claude answers.

## [0.1.4] - 2026-10-17

### Changed

- Larger pull requests get an agent review: the size limits default to 70,000 changed lines
  (was 3,000) and 350 changed files (was 100). Limits you set yourself are kept.

## [0.1.3] - 2026-10-17

### Added

- The Settings tab shows your GitHub account: `t` signs in with a personal access token
  (replacing the current sign-in) and `l` signs out — no terminal commands needed.
- The Connect screen and README explain how to get organisation repositories: approve PRowl for
  each organisation, or sign in with a personal access token if an owner won't.

### Fixed

- The help screen (`?`) no longer cuts off or scrambles lines: it's laid out in two aligned
  columns with a section per tab, sized to fit, with the update command at the bottom.

## [0.1.2] - 2026-10-17

### Added

- **Reviews tab** (`3`): reviews running now and before, each with its full record — the context
  and prompt Claude was given, its messages, every file it read or searched (live while it
  runs), its answer, PRowl's decision and what was posted to GitHub. The daemon keeps these
  transcripts and serves them at `GET /v1/jobs/{id}/transcript`; the Feed also shows each file
  Claude reads. History, Policy, Repos and Settings move to `4`–`7`.
- **Agent verdicts** (Settings → "Agent approves / requests changes on low-risk PRs", off by
  default): on PRs that don't need a human, the agent's review approves or requests changes as
  Claude recommends, instead of only commenting. If a newer commit needs a human, PRowl
  dismisses its earlier approval.
- **Update check**: PRowl looks for a newer release every 6 hours; the TUI's bottom bar shows
  `⬆ vX.Y.Z available` and `?` shows the command to update (re-running the installer). Turn it off
  with `PROWL_UPDATE_REPO=` (empty).
- `←` and `→` switch between tabs in the TUI, alongside the number keys and `Tab` / `Shift+Tab`.

### Fixed

- PRowl no longer pauses with "GitHub rate limit nearly used up" after ordinary polls. GitHub's
  search limit (30 a minute) was mistaken for the main API limit (5000 an hour); limits are now
  tracked separately, and the Feed says which one is low and when it resets.
- Finishing setup right after watching a repository no longer says "Watch at least one
  repository" because of a stale count.
- Deleting a policy override no longer shows "no policy override exists" instead of the
  confirmation.

## [0.1.1] - 2026-10-17

### Fixed

- The background service now finds `claude` installed with Homebrew, npm or Claude Code's
  installer. `prowld service install` (and the installer) gives the service the PATH of the
  shell it runs from, and PRowl also looks in the usual install locations.
- `prowl` no longer exits with "The cursor position could not be read within a normal duration"
  in terminals that don't answer cursor-position queries (it happened on resize and after
  editing a policy).

## [0.1.0] - 2026-10-17

First public version.

### Added

- Background daemon (`prowld`) that polls GitHub every minute for pull requests requesting your
  review in the repositories you choose, without using Claude.
- Reviews with the local Claude Code CLI in a read-only, restricted session, guided by an
  editable Review Policy with per-repository overrides, version history and dry runs.
- Decisions between *agent only* (comment review with inline notes) and *needs a human*
  ("waiting for human approval" comment and the Human Review Queue), based on policy rules,
  severity, risk score, file rules and size limits.
- Human Review Queue with approve, request changes and dismiss — in the TUI and as `prowl`
  commands.
- Terminal UI (`prowl`) with Queue, Feed, History, Policy, Repos and Settings tabs, guided setup,
  colours, clickable links and live resizing.
- Sign-in with a GitHub OAuth App (device flow) or a personal access token.
- Retries with backoff, crash recovery, exactly-once publishing, keep-awake handling and catch-up
  after sleep, daily budget, size limits and desktop notifications.
- Per-user background service for macOS (launchd), Linux (systemd) and Windows (Task Scheduler).
- Release builds for macOS, Linux and Windows with Homebrew, Scoop, `.deb` and `.rpm` packages.
- One-line installers (`install.sh`, `install.ps1`) that verify checksums, install into your home
  directory with versioned upgrades, and start the background service.

[Unreleased]: https://github.com/AmirhosseinBahrani/prowl/compare/v0.1.5...HEAD
[0.1.5]: https://github.com/AmirhosseinBahrani/prowl/compare/v0.1.4...v0.1.5
[0.1.4]: https://github.com/AmirhosseinBahrani/prowl/compare/v0.1.3...v0.1.4
[0.1.3]: https://github.com/AmirhosseinBahrani/prowl/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/AmirhosseinBahrani/prowl/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/AmirhosseinBahrani/prowl/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/AmirhosseinBahrani/prowl/releases/tag/v0.1.0
