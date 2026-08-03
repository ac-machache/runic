#![allow(dead_code)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::Router;
use axum::body::Body;
use axum::http::Request;
use serde_json::{Value, json};
use sqlx::postgres::PgPoolOptions;
use tower::ServiceExt;

use runic::store::{ArtifactStore, PostgresSessionStore, SessionStore, Store};
use runic::{Agent, Llm};
use runic_provider::Provider;
use runic_serve::{HostedAgents, PgPool, Runs, ServeConfig, router};

pub fn uid(prefix: &str) -> String {
    format!("{prefix}-{}", uuid::Uuid::new_v4().simple())
}

pub async fn test_pool() -> Option<PgPool> {
    let Ok(base_url) = std::env::var("RUNIC_TEST_DATABASE_URL") else {
        static NOTED: AtomicBool = AtomicBool::new(false);
        if !NOTED.swap(true, Ordering::Relaxed) {
            eprintln!(
                "\n⚠  RUNIC_TEST_DATABASE_URL not set — runic-serve tests SKIPPED (NOT verified). \
                 Point it at a scratch Postgres to verify (see scripts/test-postgres.sh).\n"
            );
        }
        return None;
    };
    let (prefix, _) = base_url
        .rsplit_once('/')
        .expect("RUNIC_TEST_DATABASE_URL must include a database name");
    let scratch_db = format!("runic_test_{}", uuid::Uuid::new_v4().simple());

    let maintenance = PgPoolOptions::new()
        .max_connections(1)
        .connect(&base_url)
        .await
        .expect("RUNIC_TEST_DATABASE_URL is set but unreachable");
    sqlx::query(sqlx::AssertSqlSafe(format!(
        r#"CREATE DATABASE "{scratch_db}""#
    )))
    .execute(&maintenance)
    .await
    .expect("create per-test scratch database");
    maintenance.close().await;

    Some(
        PgPoolOptions::new()
            .max_connections(5)
            .connect(&format!("{prefix}/{scratch_db}"))
            .await
            .expect("connect to the freshly created scratch database"),
    )
}

pub struct Harness {
    pub pool: PgPool,
    pub durable: Store,
    pub tenant: String,
}

impl Harness {
    pub fn config(&self) -> ServeConfig {
        ServeConfig::new(self.durable.clone(), self.pool.clone())
    }

    pub fn router_with(&self, name: &str, agent: impl Into<HostedAgents>) -> Router {
        router(self.config().agent(name, agent))
    }

    pub fn single_router(&self, agent: impl Into<HostedAgents>) -> Router {
        self.router_with("main", agent)
    }

    pub fn store(&self) -> Arc<dyn SessionStore> {
        self.durable.sessions()
    }

    pub fn runs(&self) -> Runs {
        Runs::new(self.pool.clone())
    }

    pub fn schedules(&self) -> runic_serve::store::Schedules {
        runic_serve::store::Schedules::new(self.pool.clone())
    }

    pub fn artifacts(&self) -> Arc<dyn ArtifactStore> {
        self.durable.artifacts()
    }
}

pub async fn harness() -> Option<Harness> {
    let pool = test_pool().await?;
    PostgresSessionStore::from_pool(pool.clone())
        .await
        .expect("substrate schema setup");
    runic_serve::store::migrate(&pool)
        .await
        .expect("serve run schema setup");
    Some(Harness {
        pool,
        durable: Store::memory().expect("in-memory store"),
        tenant: uid("tenant"),
    })
}

pub struct PanicProvider;

#[async_trait::async_trait]
impl Provider for PanicProvider {
    async fn complete(
        &self,
        _req: runic_provider::CompletionRequest,
    ) -> Result<runic_provider::CompletionResponse, runic_provider::ProviderError> {
        panic!("no agent should run in the routine tests");
    }
}

pub fn agent(provider: Arc<dyn Provider>) -> Agent {
    Agent::new(Llm::new(provider, "test-model").instructions("test"))
}

pub fn get(uri: &str, tenant: &str) -> Request<Body> {
    Request::builder()
        .uri(uri)
        .header("x-runic-tenant", tenant)
        .body(Body::empty())
        .unwrap()
}

pub fn post_json(uri: &str, tenant: &str, body: impl Into<String>) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-runic-tenant", tenant)
        .body(Body::from(body.into()))
        .unwrap()
}

pub fn patch_json(uri: &str, tenant: &str, body: impl Into<String>) -> Request<Body> {
    Request::builder()
        .method("PATCH")
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-runic-tenant", tenant)
        .body(Body::from(body.into()))
        .unwrap()
}

pub fn delete(uri: &str, tenant: &str) -> Request<Body> {
    Request::builder()
        .method("DELETE")
        .uri(uri)
        .header("x-runic-tenant", tenant)
        .body(Body::empty())
        .unwrap()
}

pub fn wait_request(session: &str, tenant: &str, message: &str) -> Request<Body> {
    wait_request_agent(session, tenant, None, message)
}

pub fn wait_request_agent(
    session: &str,
    tenant: &str,
    agent: Option<&str>,
    message: &str,
) -> Request<Body> {
    let mut body = json!({ "message": message });
    if let Some(agent) = agent {
        body["agent"] = json!(agent);
    }
    post_json(
        &format!("/sessions/{session}/runs/wait"),
        tenant,
        body.to_string(),
    )
}

pub async fn body_json(resp: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(resp.into_body(), 10_000_000)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

pub async fn body_string(resp: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), 10_000_000)
        .await
        .unwrap();
    String::from_utf8_lossy(&bytes).into_owned()
}

pub async fn create_session(app: &Router, tenant: &str, session_id: &str) {
    let resp = app
        .clone()
        .oneshot(post_json(
            "/sessions",
            tenant,
            json!({ "session_id": session_id }).to_string(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::CREATED);
}

pub async fn poll_until<F, Fut>(attempts: u32, delay: std::time::Duration, mut check: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..attempts {
        if check().await {
            return true;
        }
        tokio::time::sleep(delay).await;
    }
    false
}
