mod sessions;

pub use sessions::SqliteSessionStore;

use sqlx::SqlitePool;

use crate::{Error, Result};

pub(super) fn db(error: sqlx::Error) -> Error {
    Error::Database(error.to_string())
}

pub(super) fn serde(error: serde_json::Error) -> Error {
    Error::Serde(error.to_string())
}

pub(super) async fn migrate(pool: &SqlitePool) -> Result<()> {
    let mut migrator = sqlx::migrate!("./migrations-sqlite");
    migrator.dangerous_set_table_name("_sqlx_substrate_migrations");
    migrator
        .run(pool)
        .await
        .map_err(|error| Error::Database(error.to_string()))?;
    Ok(())
}
