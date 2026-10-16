# PRowl

PRowl watches GitHub around the clock for pull requests where you're a requested reviewer,
reviews them with your local `claude` CLI against a review policy you write, and either posts
the review straight away or flags the PR for human approval.

It runs as a per-user background daemon (`prowld`) on macOS, Linux and Windows, with a desktop
app as its client.

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
- `crates/e2e` — the end-to-end harness: the real daemon wired to a fake GitHub HTTP server and a
  `fake-claude` binary with scripted responses. Tests assert only on what reaches GitHub and what
  the Local API reports.
