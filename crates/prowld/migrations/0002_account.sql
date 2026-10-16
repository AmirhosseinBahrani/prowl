-- The connected GitHub account. The token itself lives in the credential store, never here.
CREATE TABLE account (
    id              INTEGER PRIMARY KEY CHECK (id = 1),
    login           TEXT    NOT NULL,
    name            TEXT,
    connected_at_ms INTEGER NOT NULL
);
