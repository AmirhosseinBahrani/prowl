# PRowl Review Policy

PRowl gives this policy to Claude with every pull request it reviews. Edit it to describe what
counts as critical in your codebase. Each rule under "ALWAYS require human review" needs an ID
in square brackets; if a change matches any of them, the PR waits for your approval.

## ALWAYS require human review
- [AUTH] Authentication, sessions, tokens, permissions, role or access checks
- [MONEY] Payments, balances, pricing, fees, invoicing or ledger logic
- [DATA] Database migrations, schema changes, bulk updates or data deletion
- [INFRA] CI/CD workflows, Dockerfiles, infrastructure-as-code, deploy configuration
- [SECRETS] Keys, credentials, environment variables, cryptography
- [DEPS] New, removed or upgraded dependencies

## Agent may handle alone
- Documentation, comments and typo fixes
- Test-only changes
- UI copy and styling
- Small refactors with no change in behaviour

## Extra instructions for the reviewer
- Describe your stack and any directories that need special care here.
