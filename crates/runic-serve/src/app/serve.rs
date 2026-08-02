use std::time::Duration;

use super::config::{ServeConfig, app_state};
use super::layers::with_layers;
use super::router::routes;

const SHUTDOWN_GRACE: Duration = Duration::from_secs(30);

pub async fn serve(
    config: ServeConfig,
    addr: impl tokio::net::ToSocketAddrs,
) -> anyhow::Result<()> {
    crate::store::migrate(&config.pool).await?;

    let declared = config.declared.clone();
    let (state, identity) = app_state(config);
    super::config::reconcile(state.schedules(), &declared).await?;
    state.completions.watch(state.pool.clone());
    let worker = crate::worker::spawn(state.clone());
    crate::routines::spawn(state.clone());
    let app = with_layers(crate::auth::apply(routes(state), identity));

    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(addr = %listener.local_addr()?, "runic-serve listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    worker.shutdown(SHUTDOWN_GRACE).await;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("install ctrl-c handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}
