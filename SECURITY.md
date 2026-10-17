# Security policy

PRowl processes untrusted pull requests with an AI agent on your machine, so we take security
reports seriously. The intended guarantees are described in
[docs/security-model.md](docs/security-model.md).

## Reporting a vulnerability

Please **do not open a public issue**. Instead, report it privately through
[GitHub security advisories](https://github.com/AmirhosseinBahrani/prowl/security/advisories/new).

Include what you found, how to reproduce it, and the impact you expect. You'll get an
acknowledgement within a few days; we'll keep you updated and credit you in the release notes
unless you'd rather stay anonymous.

Examples of what we're interested in:

- A pull request that gets Claude to run commands, write files or reach the network.
- A pull request that makes PRowl run its code, or write outside its own directories.
- Ways for a pull request to avoid a human review that the policy, file rules or size limits
  should have required.
- Exposure of the GitHub token or the Local API token.

## Supported versions

Security fixes are made for the latest release.
