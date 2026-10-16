-- Every saved Review Policy, per scope ('global' or 'owner/repo'). Reviews reference the id.
CREATE TABLE policy_versions (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    scope         TEXT    NOT NULL,
    content       TEXT    NOT NULL,
    source        TEXT    NOT NULL CHECK (source IN ('default', 'api', 'file', 'restore', 'delete')),
    created_at_ms INTEGER NOT NULL
);
CREATE INDEX policy_versions_scope ON policy_versions (scope, id);
