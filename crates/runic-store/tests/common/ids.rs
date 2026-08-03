//! Unique-id helpers. Every contract case mints fresh ids so backends sharing
//! one physical store (a single Postgres DB) stay logically isolated, and so a
//! rerun never collides with rows left by a previous run.

/// A process- and run-unique id with a readable prefix, e.g. `tenant-3f9c…`.
pub fn uid(prefix: &str) -> String {
    format!("{prefix}-{}", uuid::Uuid::new_v4().simple())
}

/// A fresh `(tenant, session)` pair.
pub fn tenant_session() -> (String, String) {
    (uid("tenant"), uid("sess"))
}
