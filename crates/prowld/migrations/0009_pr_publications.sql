-- What PRowl has posted on each PR, so later reviews update it instead of adding more.
CREATE TABLE pr_publications (
    repo             TEXT    NOT NULL,
    pr_number        INTEGER NOT NULL,
    -- The awaiting-approval comment (NEEDS_HUMAN), edited in place on every later review.
    comment_id       INTEGER,
    comment_url      TEXT,
    -- The latest agent COMMENT review (AGENT_ONLY); older ones are marked superseded.
    agent_review_id  INTEGER,
    agent_review_url TEXT,
    last_decision    TEXT,
    last_head_sha    TEXT,
    updated_at_ms    INTEGER NOT NULL,
    PRIMARY KEY (repo, pr_number)
);
