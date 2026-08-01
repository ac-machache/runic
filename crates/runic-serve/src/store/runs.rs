use chrono::{DateTime, Utc};
use sqlx::{PgPool, Row};

use super::types::{Cancelled, ClaimedRun, RunRecord, RunSignals, RunSpec, RunStatus};

macro_rules! run_columns {
    () => {
        "run_id, tenant, session_id, agent, status, error, output, to_cancel, \
         attempt, max_attempts, created_at, started_at, finished_at, updated_at"
    };
}

const GET_RUN: &str = concat!(
    "SELECT ",
    run_columns!(),
    " FROM runic.runs WHERE tenant = $1 AND run_id = $2"
);

const LIST_BEFORE: &str = concat!(
    "SELECT ",
    run_columns!(),
    " FROM runic.runs
      WHERE tenant = $1 AND session_id = $2 AND (created_at, run_id) < ($3, $4)
      ORDER BY created_at DESC, run_id DESC LIMIT $5"
);

const LIST_LATEST: &str = concat!(
    "SELECT ",
    run_columns!(),
    " FROM runic.runs
      WHERE tenant = $1 AND session_id = $2
      ORDER BY created_at DESC, run_id DESC LIMIT $3"
);

const LATEST_RUN: &str = concat!(
    "SELECT ",
    run_columns!(),
    " FROM runic.runs
      WHERE tenant = $1 AND session_id = $2
      ORDER BY created_at DESC, run_id DESC LIMIT 1"
);

const LATEST_ACTIVE: &str = concat!(
    "SELECT ",
    run_columns!(),
    " FROM runic.runs
      WHERE tenant = $1 AND session_id = $2
        AND status IN ('idle', 'running', 'waiting')
      ORDER BY (status = 'running') DESC, created_at DESC
      LIMIT 1"
);

const OVERFETCH: i64 = 4;

pub const SIGNAL_CHANNEL: &str = "runic_signal";

#[derive(Clone)]
pub struct Runs {
    pool: PgPool,
}

fn decode(message: String) -> sqlx::Error {
    sqlx::Error::Decode(message.into())
}

fn row_to_run(row: sqlx::postgres::PgRow) -> Result<RunRecord, sqlx::Error> {
    let status: String = row.try_get("status")?;
    Ok(RunRecord {
        run_id: row.try_get("run_id")?,
        tenant: row.try_get("tenant")?,
        session_id: row.try_get("session_id")?,
        agent: row.try_get("agent")?,
        status: RunStatus::parse(&status)
            .ok_or_else(|| decode(format!("unknown run status {status:?}")))?,
        error: row.try_get("error")?,
        output: row.try_get("output")?,
        to_cancel: row.try_get("to_cancel")?,
        attempt: row.try_get("attempt")?,
        max_attempts: row.try_get("max_attempts")?,
        created_at: row.try_get("created_at")?,
        started_at: row.try_get("started_at")?,
        finished_at: row.try_get("finished_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

#[derive(Debug, Clone, Default)]
pub struct Claim {
    pub runs: Vec<ClaimedRun>,
    pub next_due: Option<DateTime<Utc>>,
}

fn row_to_outcome(row: sqlx::postgres::PgRow) -> Result<(String, RunStatus), sqlx::Error> {
    let run_id: String = row.try_get("run_id")?;
    let status: String = row.try_get("status")?;
    let status = RunStatus::parse(&status)
        .ok_or_else(|| decode(format!("unknown run status {status:?}")))?;
    Ok((run_id, status))
}

fn row_to_claim(row: sqlx::postgres::PgRow) -> Result<ClaimedRun, sqlx::Error> {
    Ok(ClaimedRun {
        run_id: row.try_get("run_id")?,
        tenant: row.try_get("tenant")?,
        session_id: row.try_get("session_id")?,
        agent: row.try_get("agent")?,
        input: row.try_get("input")?,
        context: row.try_get("context")?,
        attempt: row.try_get("attempt")?,
        max_attempts: row.try_get("max_attempts")?,
        to_cancel: row.try_get("to_cancel")?,
        steering: steering_texts(row.try_get("steering")?),
        answer: row.try_get("answer")?,
        hook: row.try_get("hook")?,
    })
}

fn steering_texts(value: Option<serde_json::Value>) -> Vec<String> {
    value
        .and_then(|value| serde_json::from_value::<Vec<String>>(value).ok())
        .unwrap_or_default()
}

impl Runs {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    pub async fn enqueue(&self, spec: &RunSpec) -> Result<(), sqlx::Error> {
        sqlx::query(
            "WITH session AS (
                 INSERT INTO runic.sessions (tenant, session_id)
                 SELECT $2, $3 WHERE $3 IS NOT NULL
                 ON CONFLICT DO NOTHING
             )
             INSERT INTO runic.runs
                 (run_id, tenant, session_id, agent, input, context, hook, execute_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, COALESCE($8, now()))",
        )
        .bind(&spec.run_id)
        .bind(&spec.tenant)
        .bind(&spec.session_id)
        .bind(&spec.agent)
        .bind(&spec.input)
        .bind(&spec.context)
        .bind(&spec.hook)
        .bind(spec.execute_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn claim(&self, worker_id: &str, limit: i64) -> Result<Claim, sqlx::Error> {
        let rows = sqlx::query(
            "WITH due AS (
                 SELECT run_id, tenant, session_id, created_at
                 FROM runic.runs
                 WHERE status = 'idle' AND execute_at <= now()
                 ORDER BY created_at, run_id
                 LIMIT $3
             ),
             ready AS (
                 SELECT due.run_id, due.created_at
                 FROM due
                 WHERE NOT EXISTS (
                         SELECT 1 FROM runic.runs busy
                         WHERE busy.tenant = due.tenant
                           AND busy.session_id = due.session_id
                           AND busy.status = 'running')
                   AND NOT EXISTS (
                         SELECT 1 FROM runic.runs earlier
                         WHERE earlier.tenant = due.tenant
                           AND earlier.session_id = due.session_id
                           AND earlier.status = 'idle'
                           AND (earlier.created_at, earlier.run_id)
                             < (due.created_at, due.run_id))
             ),
             winners AS (
                 SELECT target.run_id, target.to_cancel, target.steering, target.answer,
                        target.hook
                 FROM ready
                 JOIN runic.runs target ON target.run_id = ready.run_id
                 WHERE target.status = 'idle'
                 ORDER BY ready.created_at, ready.run_id
                 LIMIT $2
                 FOR UPDATE OF target SKIP LOCKED
             ),
             claimed AS (
                 UPDATE runic.runs
                 SET status = 'running', started_at = now(), updated_at = now(),
                     alive_at = now(), worker_id = $1, attempt = attempt + 1,
                     steering = NULL
                 FROM winners
                 WHERE runic.runs.run_id = winners.run_id
                   AND runic.runs.status = 'idle'
                 RETURNING runic.runs.run_id, runic.runs.tenant,
                           runic.runs.session_id, runic.runs.agent,
                           runic.runs.input, runic.runs.context,
                           runic.runs.attempt, runic.runs.max_attempts,
                           winners.to_cancel, winners.steering, winners.answer,
                           winners.hook
             ),
             sleep_until AS (
                 SELECT min(execute_at) AS next_at
                 FROM runic.runs
                 WHERE status = 'idle' AND execute_at > now()
             )
             SELECT run_id, tenant, session_id, agent, input, context,
                    attempt, max_attempts, to_cancel, steering, answer, hook,
                    NULL::timestamptz AS next_at
             FROM claimed
             UNION ALL
             SELECT NULL::text, NULL::text, NULL::text, NULL::text,
                    NULL::jsonb, NULL::jsonb, NULL::int, NULL::int,
                    NULL::boolean, NULL::jsonb, NULL::jsonb, NULL::text,
                    sleep_until.next_at
             FROM sleep_until
             WHERE NOT EXISTS (SELECT 1 FROM claimed)",
        )
        .bind(worker_id)
        .bind(limit)
        .bind(limit.saturating_mul(OVERFETCH))
        .fetch_all(&self.pool)
        .await?;

        let mut claim = Claim::default();
        for row in rows {
            let run_id: Option<String> = row.try_get("run_id")?;
            match run_id {
                Some(_) => claim.runs.push(row_to_claim(row)?),
                None => claim.next_due = row.try_get("next_at")?,
            }
        }
        Ok(claim)
    }

    pub async fn heartbeat(&self, worker_id: &str, run_ids: &[String]) -> Result<(), sqlx::Error> {
        if run_ids.is_empty() {
            return Ok(());
        }
        sqlx::query(
            "UPDATE runic.runs SET alive_at = now()
             WHERE worker_id = $1 AND status = 'running' AND run_id = ANY($2)",
        )
        .bind(worker_id)
        .bind(run_ids)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn reclaim(
        &self,
        worker_id: &str,
        live: &[String],
        stale_after_secs: f64,
        delay_secs: f64,
    ) -> Result<Vec<(String, RunStatus)>, sqlx::Error> {
        let rows = sqlx::query(
            "UPDATE runic.runs
             SET status = CASE WHEN attempt >= max_attempts THEN 'failed' ELSE 'idle' END,
                 error = CASE WHEN attempt >= max_attempts
                              THEN 'the worker running this went away'
                              ELSE error END,
                 finished_at = CASE WHEN attempt >= max_attempts
                                    THEN now() ELSE finished_at END,
                 execute_at = now() + make_interval(secs => $3),
                 started_at = NULL, worker_id = NULL, alive_at = NULL,
                 updated_at = now()
             WHERE status = 'running'
               AND alive_at < now() - make_interval(secs => $2)
               AND (worker_id IS DISTINCT FROM $1 OR NOT (run_id = ANY($4)))
             RETURNING run_id, status",
        )
        .bind(worker_id)
        .bind(stale_after_secs)
        .bind(delay_secs)
        .bind(live)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(row_to_outcome).collect()
    }

    pub async fn release_all(&self, worker_id: &str) -> Result<Vec<String>, sqlx::Error> {
        sqlx::query_scalar(
            "UPDATE runic.runs
             SET status = 'idle', execute_at = now(), started_at = NULL,
                 worker_id = NULL, alive_at = NULL, updated_at = now()
             WHERE worker_id = $1 AND status = 'running'
             RETURNING run_id",
        )
        .bind(worker_id)
        .fetch_all(&self.pool)
        .await
    }

    pub async fn finish(
        &self,
        run_id: &str,
        status: RunStatus,
        error: Option<&str>,
        output: Option<&serde_json::Value>,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            "UPDATE runic.runs
             SET status = $2, error = $3, output = COALESCE($5, output),
                 updated_at = now(), worker_id = NULL, alive_at = NULL,
                 finished_at = CASE WHEN $4 THEN now() ELSE finished_at END
             WHERE run_id = $1",
        )
        .bind(run_id)
        .bind(status.as_str())
        .bind(error)
        .bind(status.is_terminal())
        .bind(output)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn retry(&self, run_id: &str, delay_secs: f64) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            "UPDATE runic.runs
             SET status = 'idle', started_at = NULL, updated_at = now(),
                 worker_id = NULL, alive_at = NULL,
                 execute_at = now() + make_interval(secs => $2)
             WHERE run_id = $1 AND status = 'running'",
        )
        .bind(run_id)
        .bind(delay_secs)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn get(&self, tenant: &str, run_id: &str) -> Result<Option<RunRecord>, sqlx::Error> {
        let row = sqlx::query(GET_RUN)
            .bind(tenant)
            .bind(run_id)
            .fetch_optional(&self.pool)
            .await?;
        row.map(row_to_run).transpose()
    }

    pub async fn list(
        &self,
        tenant: &str,
        session_id: &str,
        limit: i64,
        before: Option<(DateTime<Utc>, String)>,
    ) -> Result<Vec<RunRecord>, sqlx::Error> {
        let rows = match before {
            Some((cut_at, cut_id)) => {
                sqlx::query(LIST_BEFORE)
                    .bind(tenant)
                    .bind(session_id)
                    .bind(cut_at)
                    .bind(cut_id)
                    .bind(limit)
                    .fetch_all(&self.pool)
                    .await?
            }
            None => {
                sqlx::query(LIST_LATEST)
                    .bind(tenant)
                    .bind(session_id)
                    .bind(limit)
                    .fetch_all(&self.pool)
                    .await?
            }
        };
        rows.into_iter().map(row_to_run).collect()
    }

    pub async fn latest(
        &self,
        tenant: &str,
        session_id: &str,
    ) -> Result<Option<RunRecord>, sqlx::Error> {
        let row = sqlx::query(LATEST_RUN)
            .bind(tenant)
            .bind(session_id)
            .fetch_optional(&self.pool)
            .await?;
        row.map(row_to_run).transpose()
    }

    pub async fn latest_active(
        &self,
        tenant: &str,
        session_id: &str,
    ) -> Result<Option<RunRecord>, sqlx::Error> {
        let row = sqlx::query(LATEST_ACTIVE)
            .bind(tenant)
            .bind(session_id)
            .fetch_optional(&self.pool)
            .await?;
        row.map(row_to_run).transpose()
    }

    pub async fn take_signals(&self, run_id: &str) -> Result<Option<RunSignals>, sqlx::Error> {
        let row = sqlx::query(
            "UPDATE runic.runs r
             SET steering = NULL, updated_at = now()
             FROM (SELECT run_id, steering FROM runic.runs
                   WHERE run_id = $1 FOR UPDATE) old
             WHERE r.run_id = old.run_id
             RETURNING r.to_cancel, old.steering",
        )
        .bind(run_id)
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        Ok(Some(RunSignals {
            to_cancel: row.try_get("to_cancel")?,
            steering: steering_texts(row.try_get("steering")?),
        }))
    }

    pub async fn request_cancel(
        &self,
        tenant: &str,
        run_id: &str,
    ) -> Result<Cancelled, sqlx::Error> {
        let dropped = sqlx::query(
            "WITH dropped AS (
                 UPDATE runic.runs
                 SET status = 'cancelled', finished_at = now(), updated_at = now()
                 WHERE run_id = $1 AND tenant = $2 AND status IN ('idle', 'waiting')
                 RETURNING run_id
             )
             SELECT pg_notify($3, run_id) FROM dropped",
        )
        .bind(run_id)
        .bind(tenant)
        .bind(crate::completion::CHANNEL)
        .fetch_optional(&self.pool)
        .await?;
        if dropped.is_some() {
            return Ok(Cancelled::Dropped);
        }
        let flagged = sqlx::query(
            "WITH flagged AS (
                 UPDATE runic.runs SET to_cancel = TRUE, updated_at = now()
                 WHERE run_id = $1 AND tenant = $2 AND status = 'running'
                 RETURNING run_id
             )
             SELECT pg_notify($3, run_id) FROM flagged",
        )
        .bind(run_id)
        .bind(tenant)
        .bind(SIGNAL_CHANNEL)
        .fetch_optional(&self.pool)
        .await?;
        Ok(match flagged.is_some() {
            true => Cancelled::Flagged,
            false => Cancelled::Gone,
        })
    }

    pub async fn push_steering(
        &self,
        tenant: &str,
        run_id: &str,
        text: &str,
    ) -> Result<bool, sqlx::Error> {
        let pushed = sqlx::query(
            "WITH pushed AS (
                 UPDATE runic.runs
                 SET steering = COALESCE(steering, '[]'::jsonb) || to_jsonb($3::text),
                     updated_at = now()
                 WHERE run_id = $1 AND tenant = $2
                   AND status IN ('idle', 'running', 'waiting')
                 RETURNING run_id
             )
             SELECT pg_notify($4, run_id) FROM pushed",
        )
        .bind(run_id)
        .bind(tenant)
        .bind(text)
        .bind(SIGNAL_CHANNEL)
        .fetch_optional(&self.pool)
        .await?;
        Ok(pushed.is_some())
    }

    pub async fn resume(
        &self,
        tenant: &str,
        run_id: &str,
        answer: &serde_json::Value,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            "UPDATE runic.runs
             SET status = 'idle', input = NULL, answer = $3,
                 execute_at = now(), updated_at = now()
             WHERE run_id = $1 AND tenant = $2 AND status = 'waiting'",
        )
        .bind(run_id)
        .bind(tenant)
        .bind(answer)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }
}
