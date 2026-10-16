-- Settings changed by the user, as a JSON object over the built-in defaults.
CREATE TABLE settings (
    id            INTEGER PRIMARY KEY CHECK (id = 1),
    json          TEXT    NOT NULL,
    updated_at_ms INTEGER NOT NULL
);
