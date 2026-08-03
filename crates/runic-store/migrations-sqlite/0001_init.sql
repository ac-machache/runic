CREATE TABLE IF NOT EXISTS sessions (
    tenant          TEXT    NOT NULL,
    session_id      TEXT    NOT NULL,
    last_seq        INTEGER NOT NULL DEFAULT 0,
    event_count     INTEGER NOT NULL DEFAULT 0,
    label           TEXT,
    agent           TEXT,
    parent_session  TEXT,
    run_count       INTEGER NOT NULL DEFAULT 0,
    errored_runs    INTEGER NOT NULL DEFAULT 0,
    input_tokens    INTEGER NOT NULL DEFAULT 0,
    output_tokens   INTEGER NOT NULL DEFAULT 0,
    last_run_status TEXT,
    last_run_at     TEXT,
    created_at      TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    last_activity   TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    PRIMARY KEY (tenant, session_id)
);

CREATE INDEX IF NOT EXISTS sessions_tenant_activity_idx
    ON sessions (tenant, last_activity DESC);

CREATE INDEX IF NOT EXISTS sessions_parent_idx
    ON sessions (tenant, parent_session, last_activity DESC, session_id DESC);

CREATE TABLE IF NOT EXISTS events (
    tenant     TEXT    NOT NULL,
    session_id TEXT    NOT NULL,
    seq        INTEGER NOT NULL,
    kind       TEXT    NOT NULL,
    run_id     TEXT,
    at         TEXT    NOT NULL,
    event      TEXT    NOT NULL,
    PRIMARY KEY (tenant, session_id, seq),
    FOREIGN KEY (tenant, session_id)
        REFERENCES sessions (tenant, session_id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS events_run_idx
    ON events (tenant, session_id, run_id, seq);

CREATE INDEX IF NOT EXISTS events_kind_idx
    ON events (tenant, session_id, kind, seq);

CREATE VIRTUAL TABLE IF NOT EXISTS chats USING fts5(
    tenant     UNINDEXED,
    session_id UNINDEXED,
    seq        UNINDEXED,
    role       UNINDEXED,
    text,
    at         UNINDEXED
);
