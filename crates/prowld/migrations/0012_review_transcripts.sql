-- Everything a review run saw and did, in order: the context Claude was given, its messages and
-- tool calls, the result, and what was posted. Shown in the TUI's Reviews tab.
CREATE TABLE review_transcripts (
    id       INTEGER PRIMARY KEY AUTOINCREMENT,
    job_id   INTEGER NOT NULL,
    attempt  INTEGER NOT NULL,
    at_ms    INTEGER NOT NULL,
    kind     TEXT    NOT NULL,
    content  TEXT    NOT NULL
);
CREATE INDEX review_transcripts_job ON review_transcripts (job_id, id);
