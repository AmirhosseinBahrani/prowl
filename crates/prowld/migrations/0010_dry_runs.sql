-- Claude spend from dry runs (reviews that publish nothing), counted against the daily budget.
CREATE TABLE dry_runs (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    repo          TEXT    NOT NULL,
    pr_number     INTEGER NOT NULL,
    cost_usd      REAL,
    created_at_ms INTEGER NOT NULL
);
