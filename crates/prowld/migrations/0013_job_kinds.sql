-- Jobs get a kind: 'review' (found by polling), 'rerun' (asked for by the user) or 'verify'
-- (checking that resolved findings were fixed). Only polled reviews stay one per commit, so a
-- commit can be reviewed again on request. SQLite can't drop a table constraint, so the table
-- is rebuilt. Migrations run in a transaction, where foreign keys can't be switched off, so
-- their checks are deferred to the commit instead — by then the rebuilt table has every id that
-- reviews point at.
PRAGMA defer_foreign_keys = ON;
CREATE TABLE jobs_backup AS SELECT * FROM jobs;
DROP TABLE jobs;
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
    kind           TEXT    NOT NULL DEFAULT 'review'
                   CHECK (kind IN ('review', 'rerun', 'verify'))
);
-- Re-inserting the rows into `jobs` is what settles the deferred checks for reviews.
INSERT INTO jobs (id, repo, pr_number, head_sha, title, author, html_url, status, attempts,
                  next_run_at_ms, error, created_at_ms, updated_at_ms)
    SELECT id, repo, pr_number, head_sha, title, author, html_url, status, attempts,
           next_run_at_ms, error, created_at_ms, updated_at_ms
    FROM jobs_backup;
DROP TABLE jobs_backup;
CREATE INDEX jobs_runnable ON jobs (status, next_run_at_ms);
CREATE UNIQUE INDEX jobs_one_review_per_commit ON jobs (repo, pr_number, head_sha)
    WHERE kind = 'review';
