-- Each run of policy learning for a repository: how much feedback it read, what it changed,
-- and the policy version it saved. The latest run is where the next one starts reading.
CREATE TABLE policy_learning (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    repo          TEXT    NOT NULL,
    ran_at_ms     INTEGER NOT NULL,
    signals       INTEGER NOT NULL,
    applied       INTEGER NOT NULL,
    version_id    INTEGER,
    changes       TEXT    NOT NULL,
    cost_usd      REAL,
    -- 0 when the run failed, so its feedback is read again next time.
    consumed      INTEGER NOT NULL DEFAULT 1
);
CREATE INDEX policy_learning_repo ON policy_learning (repo, ran_at_ms);

-- Learned policy versions get their own source. The CHECK can't be altered, so the table is
-- rebuilt (nothing references it by foreign key); ids are kept, as reviews record them.
CREATE TABLE policy_versions_new (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    scope         TEXT    NOT NULL,
    content       TEXT    NOT NULL,
    source        TEXT    NOT NULL
                  CHECK (source IN ('default', 'api', 'file', 'restore', 'delete', 'learned')),
    created_at_ms INTEGER NOT NULL
);
INSERT INTO policy_versions_new (id, scope, content, source, created_at_ms)
    SELECT id, scope, content, source, created_at_ms FROM policy_versions;
DROP TABLE policy_versions;
ALTER TABLE policy_versions_new RENAME TO policy_versions;
CREATE INDEX policy_versions_scope ON policy_versions (scope, id);
