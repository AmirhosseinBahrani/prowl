-- Pull requests waiting for the user's own review, one row per PR (the latest decision wins).
CREATE TABLE human_queue (
    repo            TEXT    NOT NULL,
    pr_number       INTEGER NOT NULL,
    head_sha        TEXT    NOT NULL,
    review_id       INTEGER NOT NULL REFERENCES reviews (id),
    status          TEXT    NOT NULL DEFAULT 'pending'
                    CHECK (status IN ('pending', 'approved', 'changes_requested', 'dismissed', 'closed')),
    -- 'app' or 'github'
    resolved_via    TEXT,
    queued_at_ms    INTEGER NOT NULL,
    resolved_at_ms  INTEGER,
    PRIMARY KEY (repo, pr_number)
);
CREATE INDEX human_queue_status ON human_queue (status);
