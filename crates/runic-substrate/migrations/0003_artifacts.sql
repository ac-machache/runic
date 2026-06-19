-- runic-substrate: artifact metadata index (bytes live in an ArtifactStore).
-- FKs to `sessions` so deleting a session cascades its artifact rows.

CREATE TABLE IF NOT EXISTS artifacts (
    artifact_id  TEXT        PRIMARY KEY,
    tenant       TEXT        NOT NULL,
    session_id   TEXT        NOT NULL,
    mime_type    TEXT        NOT NULL,
    size         BIGINT      NOT NULL,
    source       TEXT        NOT NULL,
    storage      TEXT        NOT NULL,   -- where the bytes live: local | s3 | gcs
    storage_key  TEXT        NOT NULL,   -- key/uri inside that store
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    FOREIGN KEY (tenant, session_id)
        REFERENCES sessions (tenant, session_id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS artifacts_session_idx
    ON artifacts (tenant, session_id, created_at DESC);
