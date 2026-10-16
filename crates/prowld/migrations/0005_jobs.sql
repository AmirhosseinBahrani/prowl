-- Review jobs: one per (repository, pull request, head commit). The unique key is what makes
-- a commit reviewed at most once, across polls and restarts.
CREATE TABLE jobs (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    repo           TEXT    NOT NULL,
    pr_number      INTEGER NOT NULL,
    head_sha       TEXT    NOT NULL,
    title          TEXT    NOT NULL,
    author         TEXT    NOT NULL,
    html_url       TEXT    NOT NULL,
    status         TEXT    NOT NULL DEFAULT 'queued'
                   CHECK (status IN ('queued', 'running', 'done', 'failed', 'interrupted', 'cancelled')),
    attempts       INTEGER NOT NULL DEFAULT 0,
    next_run_at_ms INTEGER NOT NULL,
    error          TEXT,
    created_at_ms  INTEGER NOT NULL,
    updated_at_ms  INTEGER NOT NULL,
    UNIQUE (repo, pr_number, head_sha)
);
CREATE INDEX jobs_runnable ON jobs (status, next_run_at_ms);
