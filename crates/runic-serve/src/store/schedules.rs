use std::str::FromStr;

use chrono::{DateTime, Timelike, Utc};
use croner::Cron;
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};

const COLUMNS: &str = "schedule_id, tenant, routine, payload, cron, tz, enabled, \
                       next_at, last_fired_at, last_error, created_at, updated_at";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduleRecord {
    pub schedule_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
    pub routine: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<serde_json::Value>,
    pub cron: String,
    pub tz: String,
    pub enabled: bool,
    pub next_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_fired_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct ScheduleSpec {
    pub schedule_id: String,
    pub tenant: Option<String>,
    pub routine: String,
    pub payload: Option<serde_json::Value>,
    pub cron: String,
    pub tz: String,
}

impl ScheduleSpec {
    pub fn new(
        schedule_id: impl Into<String>,
        routine: impl Into<String>,
        cron: impl Into<String>,
    ) -> Self {
        Self {
            schedule_id: schedule_id.into(),
            tenant: None,
            routine: routine.into(),
            payload: None,
            cron: cron.into(),
            tz: "UTC".to_string(),
        }
    }

    pub fn tenant(mut self, tenant: impl Into<String>) -> Self {
        self.tenant = Some(tenant.into());
        self
    }

    pub fn payload(mut self, payload: Option<serde_json::Value>) -> Self {
        self.payload = payload;
        self
    }

    pub fn tz(mut self, tz: impl Into<String>) -> Self {
        self.tz = tz.into();
        self
    }
}

#[derive(Debug, Clone)]
pub struct DueRoutine {
    pub schedule_id: String,
    pub tenant: Option<String>,
    pub routine: String,
    pub payload: Option<serde_json::Value>,
    pub cron: String,
    pub tz: String,
}

#[derive(Clone)]
pub struct Schedules {
    pool: PgPool,
}

impl Schedules {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn create(&self, spec: &ScheduleSpec) -> Result<ScheduleRecord, sqlx::Error> {
        let next_at = next_after(&spec.cron, &spec.tz, Utc::now())
            .map_err(|error| sqlx::Error::Decode(error.to_string().into()))?;
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "INSERT INTO runic.schedules
                 (schedule_id, tenant, routine, payload, cron, tz, next_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             RETURNING {COLUMNS}"
        )))
        .bind(&spec.schedule_id)
        .bind(&spec.tenant)
        .bind(&spec.routine)
        .bind(&spec.payload)
        .bind(&spec.cron)
        .bind(&spec.tz)
        .bind(next_at)
        .fetch_one(&self.pool)
        .await?;
        row_to_schedule(row)
    }

    pub async fn declare(&self, name: &str, cron: &str, tz: &str) -> Result<(), sqlx::Error> {
        let next_at = next_after(cron, tz, Utc::now())
            .map_err(|error| sqlx::Error::Decode(error.to_string().into()))?;
        sqlx::query(
            "INSERT INTO runic.schedules
                 (schedule_id, routine, cron, tz, next_at, declared, enabled)
             VALUES ($1, $1, $2, $3, $4, TRUE, TRUE)
             ON CONFLICT (schedule_id) DO UPDATE
             SET cron = EXCLUDED.cron,
                 tz = EXCLUDED.tz,
                 declared = TRUE,
                 enabled = TRUE,
                 updated_at = now(),
                 next_at = CASE
                     WHEN runic.schedules.cron IS DISTINCT FROM EXCLUDED.cron
                       OR runic.schedules.tz IS DISTINCT FROM EXCLUDED.tz
                     THEN EXCLUDED.next_at
                     ELSE runic.schedules.next_at
                 END",
        )
        .bind(name)
        .bind(cron)
        .bind(tz)
        .bind(next_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn retire_undeclared(&self, keep: &[String]) -> Result<Vec<String>, sqlx::Error> {
        sqlx::query_scalar(
            "UPDATE runic.schedules
             SET enabled = FALSE, updated_at = now()
             WHERE declared AND enabled AND NOT (schedule_id = ANY($1))
             RETURNING schedule_id",
        )
        .bind(keep)
        .fetch_all(&self.pool)
        .await
    }

    pub async fn get(&self, id: &str) -> Result<Option<ScheduleRecord>, sqlx::Error> {
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {COLUMNS} FROM runic.schedules WHERE schedule_id = $1"
        )))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(row_to_schedule).transpose()
    }

    pub async fn list(&self, tenant: Option<&str>) -> Result<Vec<ScheduleRecord>, sqlx::Error> {
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {COLUMNS} FROM runic.schedules
             WHERE $1::text IS NULL OR tenant = $1
             ORDER BY created_at DESC, schedule_id DESC"
        )))
        .bind(tenant)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(row_to_schedule).collect()
    }

    pub async fn set_enabled(&self, id: &str, on: bool) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            "UPDATE runic.schedules SET enabled = $2, updated_at = now() WHERE schedule_id = $1",
        )
        .bind(id)
        .bind(on)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn delete(&self, id: &str) -> Result<bool, sqlx::Error> {
        let result = sqlx::query("DELETE FROM runic.schedules WHERE schedule_id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn claim_due(&self, limit: i64) -> Result<Vec<DueRoutine>, sqlx::Error> {
        let rows = sqlx::query(
            "WITH due AS (
                 SELECT schedule_id FROM runic.schedules
                 WHERE enabled AND next_at <= now() AND running_at IS NULL
                 ORDER BY next_at
                 LIMIT $1
                 FOR UPDATE SKIP LOCKED
             )
             UPDATE runic.schedules target
             SET running_at = now(), updated_at = now()
             FROM due
             WHERE target.schedule_id = due.schedule_id
             RETURNING target.schedule_id, target.tenant, target.routine,
                       target.payload, target.cron, target.tz",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(row_to_due).collect()
    }

    pub async fn settle(
        &self,
        id: &str,
        next_at: DateTime<Utc>,
        error: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE runic.schedules
             SET running_at = NULL, last_fired_at = now(), last_error = $3,
                 next_at = $2, updated_at = now()
             WHERE schedule_id = $1",
        )
        .bind(id)
        .bind(next_at)
        .bind(error)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn reclaim(&self, stale_after_secs: f64) -> Result<u64, sqlx::Error> {
        let result = sqlx::query(
            "UPDATE runic.schedules
             SET running_at = NULL, updated_at = now()
             WHERE running_at IS NOT NULL
               AND running_at < now() - make_interval(secs => $1)",
        )
        .bind(stale_after_secs)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    pub async fn next_due(&self) -> Result<Option<DateTime<Utc>>, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT min(next_at) FROM runic.schedules
             WHERE enabled AND running_at IS NULL",
        )
        .fetch_one(&self.pool)
        .await
    }
}

fn row_to_schedule(row: sqlx::postgres::PgRow) -> Result<ScheduleRecord, sqlx::Error> {
    Ok(ScheduleRecord {
        schedule_id: row.try_get("schedule_id")?,
        tenant: row.try_get("tenant")?,
        routine: row.try_get("routine")?,
        payload: row.try_get("payload")?,
        cron: row.try_get("cron")?,
        tz: row.try_get("tz")?,
        enabled: row.try_get("enabled")?,
        next_at: row.try_get("next_at")?,
        last_fired_at: row.try_get("last_fired_at")?,
        last_error: row.try_get("last_error")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

fn row_to_due(row: sqlx::postgres::PgRow) -> Result<DueRoutine, sqlx::Error> {
    Ok(DueRoutine {
        schedule_id: row.try_get("schedule_id")?,
        tenant: row.try_get("tenant")?,
        routine: row.try_get("routine")?,
        payload: row.try_get("payload")?,
        cron: row.try_get("cron")?,
        tz: row.try_get("tz")?,
    })
}

#[derive(Debug, thiserror::Error)]
pub enum CronError {
    #[error("{0:?} is not a valid cron expression: {1}")]
    Expression(String, String),
    #[error("{0:?} is not a known timezone")]
    Timezone(String),
    #[error("{0:?} has no next occurrence")]
    Never(String),
}

pub fn next_after(cron: &str, tz: &str, after: DateTime<Utc>) -> Result<DateTime<Utc>, CronError> {
    let parsed = Cron::from_str(cron)
        .map_err(|error| CronError::Expression(cron.to_string(), error.to_string()))?;
    let zone: chrono_tz::Tz = tz
        .parse()
        .map_err(|_| CronError::Timezone(tz.to_string()))?;
    let next = parsed
        .find_next_occurrence(&after.with_timezone(&zone), false)
        .map_err(|_| CronError::Never(cron.to_string()))?;
    Ok(next
        .with_timezone(&Utc)
        .with_nanosecond(0)
        .unwrap_or_else(|| next.with_timezone(&Utc)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn a_daily_expression_lands_on_the_next_day_in_utc() {
        let next = next_after("0 9 * * *", "UTC", at("2026-08-02T10:00:00Z")).unwrap();
        assert_eq!(next, at("2026-08-03T09:00:00Z"));
    }

    #[test]
    fn the_timezone_decides_when_nine_is() {
        let noon_in_tokyo = at("2026-08-02T03:00:00Z");
        let utc = next_after("0 9 * * *", "UTC", noon_in_tokyo).unwrap();
        let tokyo = next_after("0 9 * * *", "Asia/Tokyo", noon_in_tokyo).unwrap();

        assert_eq!(utc, at("2026-08-02T09:00:00Z"), "09:00 UTC is later today");
        assert_eq!(
            tokyo,
            at("2026-08-03T00:00:00Z"),
            "09:00 Tokyo already passed today; the next one is tomorrow, which is midnight UTC"
        );
    }

    #[test]
    fn a_bad_expression_is_rejected_rather_than_silently_never_firing() {
        let error = next_after("not a cron", "UTC", Utc::now()).unwrap_err();
        assert!(matches!(error, CronError::Expression(..)), "got {error:?}");
    }

    #[test]
    fn an_unknown_timezone_is_rejected() {
        let error = next_after("0 9 * * *", "Mars/Olympus", Utc::now()).unwrap_err();
        assert!(matches!(error, CronError::Timezone(_)), "got {error:?}");
    }
}
