//! Postgres backends (feature `postgres`): the session store + the artifact
//! metadata index, sharing one pool, schema, and migration set.

mod artifacts;
mod sessions;

pub use artifacts::PostgresArtifactStore;
pub use sessions::PostgresSessionStore;

use sqlx::PgPool;

use crate::{Error, Result};

pub(super) fn db(e: sqlx::Error) -> Error {
    Error::Database(e.to_string())
}

pub(super) fn serde(e: serde_json::Error) -> Error {
    Error::Serde(e.to_string())
}

pub(super) async fn migrate(pool: &PgPool) -> Result<()> {
    sqlx::migrate!("./migrations")
        .run(pool)
        .await
        .map_err(|error| Error::Database(error.to_string()))?;
    Ok(())
}
