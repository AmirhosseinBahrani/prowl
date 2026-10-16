CREATE TABLE events (
    id      INTEGER PRIMARY KEY AUTOINCREMENT,
    kind    TEXT    NOT NULL,
    payload TEXT    NOT NULL,
    at_ms   INTEGER NOT NULL
);
