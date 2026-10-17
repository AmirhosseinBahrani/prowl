# Contributing to PRowl

Thanks for your interest in PRowl! Bug reports, ideas, documentation and code are all welcome.

## Before you start

- **Bugs** — open an issue with the bug template. Include your OS, `prowl --version`,
  `claude --version`, and what the Feed tab or `logs/` showed.
- **Features** — open an issue first to discuss the idea, especially for anything that changes
  how reviews are decided or what is posted to GitHub.
- **Security issues** — don't open a public issue; see [SECURITY.md](SECURITY.md).

## Making a change

1. Fork the repository and create a branch from `main`.
2. Read [docs/development.md](docs/development.md) to build and test.
3. Keep the change focused. Add or update tests — most behaviour is tested end to end in
   `crates/e2e` against fake GitHub and a fake Claude CLI.
4. Run the same checks as CI:
   ```sh
   cargo fmt --all --check
   cargo clippy --workspace --all-targets -- -D warnings
   cargo test --workspace
   ```
5. Update documentation (`README.md`, `docs/`) and add a line to `CHANGELOG.md` under
   *Unreleased* if users will notice the change.
6. Open a pull request and fill in the template.

## Style

- Follow the code around you; `rustfmt` settles formatting.
- Commit subjects are short, capitalised and imperative: `Add token sign-in to the TUI`.
- User-facing text is plain English, sentence case, without jargon.

## Ground rules PRowl keeps

Please keep these properties when changing review behaviour — PRs that weaken them won't be
merged:

- Claude never gets shell, write or network access, and nothing from the reviewed repository can
  configure it.
- PRowl never runs code from a pull request.
- Signals can only escalate a PR to a human, never downgrade it.
- The agent never approves a pull request unless the user turned on agent verdicts, and never one
  that any signal sends to a human.

## License

By contributing, you agree that your contributions are dual licensed under the
[MIT](LICENSE-MIT) and [Apache-2.0](LICENSE-APACHE) licenses, as described in the README.
