CREATE TABLE IF NOT EXISTS runic.schedules (
    schedule_id   TEXT        PRIMARY KEY,
    tenant        TEXT,
    routine       TEXT        NOT NULL,
    payload       JSONB,
    cron          TEXT        NOT NULL,
    tz            TEXT        NOT NULL DEFAULT 'UTC',
    enabled       BOOLEAN     NOT NULL DEFAULT TRUE,
    declared      BOOLEAN     NOT NULL DEFAULT FALSE,
    next_at       TIMESTAMPTZ NOT NULL,
    running_at    TIMESTAMPTZ,
    last_fired_at TIMESTAMPTZ,
    last_error    TEXT,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS schedules_due
    ON runic.schedules (next_at) WHERE enabled;

CREATE INDEX IF NOT EXISTS schedules_by_tenant
    ON runic.schedules (tenant, created_at DESC, schedule_id DESC);
