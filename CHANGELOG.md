# Changelog

All notable changes to PRowl are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project follows
[Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.1.1] - 2026-10-17

### Fixed

- The background service now finds `claude` installed with Homebrew, npm or Claude Code's
  installer. `prowld service install` (and the installer) gives the service the PATH of the
  shell it runs from, and PRowl also looks in the usual install locations.

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

[Unreleased]: https://github.com/AmirhosseinBahrani/prowl/compare/v0.1.1...HEAD
[0.1.1]: https://github.com/AmirhosseinBahrani/prowl/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/AmirhosseinBahrani/prowl/releases/tag/v0.1.0
