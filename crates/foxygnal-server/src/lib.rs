//! HTTP + WebSocket server hosting the browser UI and the control API.
//!
//! M0: serves the block catalog and the built UI. The WebSocket session
//! protocol (graph editing, run control, scope taps) is specified in
//! docs/ARCHITECTURE.md and lands in M1.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use foxygnal_core::{BlockDescriptor, Registry};
use serde::Serialize;
use tower_http::services::{ServeDir, ServeFile};

pub struct Config {
    pub bind: SocketAddr,
    /// Directory containing the built UI (`ui/dist`). `None` = API only.
    pub ui_dir: Option<PathBuf>,
}

struct AppState {
    registry: Registry,
}

#[derive(Serialize)]
struct Health {
    name: &'static str,
    version: &'static str,
}

async fn health() -> Json<Health> {
    Json(Health {
        name: "foxygnal",
        version: env!("CARGO_PKG_VERSION"),
    })
}

async fn blocks(State(s): State<Arc<AppState>>) -> Json<Vec<BlockDescriptor>> {
    Json(s.registry.descriptors().cloned().collect())
}

pub fn router(registry: Registry, ui_dir: Option<PathBuf>) -> Router {
    let state = Arc::new(AppState { registry });
    let api = Router::new()
        .route("/health", get(health))
        .route("/blocks", get(blocks))
        .with_state(state);
    let app = Router::new().nest("/api", api);
    match ui_dir {
        Some(dir) => {
            let index = dir.join("index.html");
            app.fallback_service(ServeDir::new(dir).fallback(ServeFile::new(index)))
        }
        None => app,
    }
}

pub async fn serve(cfg: Config, registry: Registry) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(cfg.bind).await?;
    tracing::info!("foxygnal listening on http://{}", listener.local_addr()?);
    axum::serve(listener, router(registry, cfg.ui_dir))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
}
