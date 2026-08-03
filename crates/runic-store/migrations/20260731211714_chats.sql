CREATE TABLE IF NOT EXISTS runic.chats (
    tenant     TEXT        NOT NULL,
    session_id TEXT        NOT NULL,
    seq        BIGINT      NOT NULL,
    role       TEXT        NOT NULL,
    text       TEXT        NOT NULL,
    at         TIMESTAMPTZ NOT NULL,
    tsv        tsvector GENERATED ALWAYS AS (to_tsvector('english', text)) STORED,
    PRIMARY KEY (tenant, session_id, seq),
    FOREIGN KEY (tenant, session_id)
        REFERENCES runic.sessions (tenant, session_id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS chats_tsv_idx
    ON runic.chats USING GIN (tsv);

CREATE INDEX IF NOT EXISTS chats_recent_idx
    ON runic.chats (tenant, at DESC);
