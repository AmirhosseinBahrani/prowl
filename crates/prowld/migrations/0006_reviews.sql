-- One row per completed Claude run for a job (a repair retry is part of the same review).
CREATE TABLE reviews (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    job_id            INTEGER NOT NULL REFERENCES jobs (id),
    policy_scope      TEXT    NOT NULL,
    policy_version_id INTEGER NOT NULL,
    valid             INTEGER NOT NULL,
    -- The validated review JSON when valid; NULL otherwise.
    output            TEXT,
    -- Claude's final answer as returned, kept for auditing invalid output.
    raw_answer        TEXT    NOT NULL,
    invalid_reason    TEXT,
    cost_usd          REAL,
    duration_ms       INTEGER NOT NULL,
    created_at_ms     INTEGER NOT NULL
);
CREATE INDEX reviews_job ON reviews (job_id);
