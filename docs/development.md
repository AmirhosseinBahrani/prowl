# Development

## Prerequisites

- Rust 1.88 or newer (`rustup` recommended)
- `git`
- Linux only: `libdbus-1-dev` and `pkg-config` (Secret Service support)

You don't need GitHub credentials or the Claude CLI to run the test suite.

## Build and run

```sh
cargo build --workspace
# run a throwaway daemon in the foreground with its own home
cargo run -p prowld -- --home /tmp/prowl-dev run
# in another terminal, the TUI against it
cargo run -p prowl -- --home /tmp/prowl-dev
```

## Checks (what CI runs)

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

CI runs these on macOS, Linux and Windows. Packaging is checked on PRs that touch `packaging/`.

## Tests

Most behaviour is tested end to end in `crates/e2e`: each test starts the **real daemon** in a
temporary home with

- **FakeGitHub** — an in-process HTTP server with scripted routes, ETags, rate limits and request
  recording;
- **fake-claude** — a stand-in for the `claude` CLI that records its arguments, input and working
  directory and answers with a scripted review;
- **real git repositories** with `refs/pull/N/head`;
- fake power and notification backends.

TUI tests drive `App` with messages and render to ratatui's `TestBackend`. Shared helpers live in
`crates/e2e/tests/common/mod.rs`.

When fixing a bug, add a test that fails without the fix.

## Code layout

See [architecture](architecture.md). Database migrations are in `crates/prowld/migrations`
(never edit a released migration; add a new one).

## Releasing

1. Update `version` in the root `Cargo.toml` and `CHANGELOG.md`.
2. Tag and push: `git tag v0.2.0 && git push origin v0.2.0`.
3. The Release workflow builds all targets, creates the GitHub release with archives, `.deb`,
   `.rpm`, checksums, a Homebrew formula, a Scoop manifest and the `install.sh` / `install.ps1`
   installers, then installs the new release with them on macOS, Linux and Windows.

To try the installer against your own builds, serve a directory laid out like
`download/<tag>/<archive>` and point `PROWL_RELEASES_URL` at it.

Set the repository variable `PROWL_GITHUB_CLIENT_ID` to build the project's OAuth App client id
into release binaries. Optional macOS signing uses the `MACOS_CERTIFICATE`,
`MACOS_CERTIFICATE_PASSWORD` and `MACOS_SIGNING_IDENTITY` secrets.
