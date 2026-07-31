CREATE TABLE IF NOT EXISTS runic.artifacts (
    artifact_id TEXT        PRIMARY KEY,
    tenant      TEXT        NOT NULL,
    session_id  TEXT        NOT NULL,
    mime_type   TEXT        NOT NULL,
    size        BIGINT      NOT NULL,
    source      TEXT        NOT NULL,
    storage     TEXT        NOT NULL,
    storage_key TEXT        NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    FOREIGN KEY (tenant, session_id)
        REFERENCES runic.sessions (tenant, session_id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS artifacts_session_idx
    ON runic.artifacts (tenant, session_id, created_at DESC);
