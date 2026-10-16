-- Repositories the connected account can see, cached from GitHub, and whether PRowl watches them.
CREATE TABLE repos (
    id              INTEGER PRIMARY KEY,  -- GitHub repository id
    full_name       TEXT    NOT NULL UNIQUE,
    owner           TEXT    NOT NULL,
    name            TEXT    NOT NULL,
    private         INTEGER NOT NULL,
    archived        INTEGER NOT NULL,
    default_branch  TEXT    NOT NULL,
    html_url        TEXT    NOT NULL,
    watched         INTEGER NOT NULL DEFAULT 0,
    seen_at_ms      INTEGER NOT NULL
);
CREATE INDEX repos_watched ON repos (watched);
