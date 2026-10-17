# PRowl

PRowl watches GitHub around the clock for pull requests where you're a requested reviewer,
reviews them with your local `claude` CLI against a review policy you write, and either posts
the review straight away or flags the PR for human approval.

It runs as a per-user background daemon (`prowld`) on macOS, Linux and Windows, with a terminal
UI (`prowl`) as its client — locally or over SSH on an always-on machine.

## Install

You need the [Claude Code CLI](https://claude.com/claude-code) (`claude`, logged in) and `git`.

- **macOS / Linux (Homebrew):** download `prowl.rb` from the
  [latest release](https://github.com/AmirhosseinBahrani/prowl/releases/latest) and run
  `brew install ./prowl.rb`.
- **Debian / Ubuntu:** `sudo apt install ./prowl_<version>_amd64.deb`
- **Fedora / RHEL:** `sudo dnf install ./prowl-<version>-1.x86_64.rpm`
- **Windows (Scoop):** `scoop install https://github.com/AmirhosseinBahrani/prowl/releases/latest/download/prowl.json`
- **Anywhere:** unpack the archive for your platform and put `prowl`, `prowld` and
  `prowld-service` on your `PATH`. On macOS, keep them out of `~/Desktop`, `~/Documents` and
  `~/Downloads`: background services aren't allowed to run programs from those folders.

Then start the background service, which also starts at every login, and set PRowl up:

```sh
prowld service install
prowl
```

`prowl` walks you through connecting GitHub, choosing repositories and your review policy.
Releases need a GitHub OAuth App client id built in or set as `PROWL_GITHUB_CLIENT_ID`.

## Uninstall

```sh
prowld service uninstall
```

Then remove the binaries (or the package), and optionally PRowl's data: `~/Library/Application
Support/dev.prowl.prowl` (macOS), `~/.local/share/prowl` (Linux) or
`%LOCALAPPDATA%\prowl\prowl\data` (Windows). Disconnecting first (`prowld logout`) also removes
the GitHub token from your keychain.

## Development

On Linux, install the D-Bus headers first (used for the Secret Service credential store):
`sudo apt-get install libdbus-1-dev pkg-config`.

```sh
cargo test --workspace     # unit tests + end-to-end tests against fake GitHub and fake claude
cargo run -p prowld        # run the daemon in the foreground
cargo run -p prowld -- status
```

`--home <dir>` (or `PROWL_HOME`) points the daemon at a different data directory.

### Layout

- `crates/prowld` — the daemon and its Local API client.
- `crates/prowl` — the terminal UI and the `prowl queue` / `approve` / … commands.
- `packaging/` — release packaging (archives, .deb, .rpm, Homebrew formula, Scoop manifest);
  `.github/workflows/release.yml` runs it for every `v*` tag.
- `crates/e2e` — the end-to-end harness: the real daemon wired to a fake GitHub HTTP server and a
  `fake-claude` binary with scripted responses. Tests assert only on what reaches GitHub and what
  the Local API reports.
