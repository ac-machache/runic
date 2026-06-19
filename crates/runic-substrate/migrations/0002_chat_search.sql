-- runic-sessions: full-text search projection over conversational messages.
-- A read-model of `Message` events; populated on append, queried by search_chats.
-- Tenant-scoped textual (NOT semantic) search.

CREATE TABLE IF NOT EXISTS chat_messages (
    tenant      TEXT        NOT NULL,
    session_id  TEXT        NOT NULL,
    seq         BIGINT      NOT NULL,
    role        TEXT        NOT NULL,
    text        TEXT        NOT NULL,
    at          TIMESTAMPTZ NOT NULL,
    tsv         tsvector GENERATED ALWAYS AS (to_tsvector('english', text)) STORED,
    PRIMARY KEY (tenant, session_id, seq),
    FOREIGN KEY (tenant, session_id)
        REFERENCES sessions (tenant, session_id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS chat_messages_tsv_idx
    ON chat_messages USING GIN (tsv);

CREATE INDEX IF NOT EXISTS chat_messages_recent_idx
    ON chat_messages (tenant, at DESC);
