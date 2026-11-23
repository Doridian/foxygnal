//! HTTP + WebSocket server hosting the browser UI and the control API.
//! The protocol is described in docs/ARCHITECTURE.md ("Server API").

mod api;
pub mod graphs;
pub mod probe;
mod security;
pub mod session;
mod ws;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::routing::{get, post};
use axum::{Router, middleware};
use foxygnal_core::Registry;
use tower_http::services::{ServeDir, ServeFile};

pub use security::Security;
use session::Session;

/// Where the browser UI comes from.
pub enum Ui {
    /// API only.
    None,
    /// A directory with the built UI (`ui/dist`).
    Dir(PathBuf),
    /// The UI compiled into the binary (`embed-ui` feature).
    #[cfg(feature = "embed-ui")]
    Embedded,
}

impl Ui {
    /// The UI compiled into this binary, if any.
    pub fn embedded() -> Option<Self> {
        #[cfg(feature = "embed-ui")]
        return Some(Self::Embedded);
        #[cfg(not(feature = "embed-ui"))]
        None
    }
}

pub struct Config {
    pub bind: SocketAddr,
    pub ui: Ui,
    /// Where graphs are listed, opened and saved from the browser.
    pub project_dir: PathBuf,
    pub security: Security,
}

pub(crate) struct AppState {
    pub session: Arc<Session>,
    pub project_dir: PathBuf,
}

pub fn router(cfg: &Config, session: Arc<Session>) -> Router {
    let state = Arc::new(AppState {
        session,
        project_dir: cfg.project_dir.clone(),
    });
    let api = Router::new()
        .route("/health", get(api::health))
        .route("/blocks", get(api::blocks))
        .route("/blocks/{kind}/response", post(api::block_response))
        .route("/graph", get(api::get_graph).put(api::put_graph))
        .route("/graph.toml", get(api::get_graph_toml))
        .route("/toml/parse", post(api::parse_toml))
        .route("/toml/render", post(api::render_toml))
        .route("/examples", get(api::examples))
        .route("/examples/{name}", get(api::example))
        .route("/files", get(api::list_files))
        .route("/files/{name}", get(api::get_file).put(api::put_file))
        .route("/ws", get(ws::upgrade))
        .with_state(state);
    let app = Router::new().nest("/api", api);
    let app = match &cfg.ui {
        Ui::None => app,
        Ui::Dir(dir) => {
            let index = dir.join("index.html");
            app.fallback_service(ServeDir::new(dir).fallback(ServeFile::new(index)))
        }
        #[cfg(feature = "embed-ui")]
        Ui::Embedded => app.fallback(embedded::serve),
    };
    let security = Arc::new(Security {
        allow_remote: cfg.security.allow_remote,
        token: cfg.security.token.clone(),
    });
    app.layer(middleware::from_fn_with_state(security, security::guard))
}

pub async fn serve(cfg: Config, registry: Registry) -> std::io::Result<()> {
    let session = Session::new(Arc::new(registry), cfg.project_dir.clone());
    let app = router(&cfg, session.clone());
    let listener = tokio::net::TcpListener::bind(cfg.bind).await?;
    let addr = listener.local_addr()?;
    match &cfg.security.token {
        Some(t) => tracing::info!("foxygnal listening on http://{addr}/?token={t}"),
        None => tracing::info!("foxygnal listening on http://{addr}"),
    }
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    session.stop_and_wait().await;
    Ok(())
}

#[cfg(feature = "embed-ui")]
mod embedded {
    use axum::http::{StatusCode, Uri, header};
    use axum::response::{IntoResponse, Response};

    #[derive(rust_embed::Embed)]
    #[folder = "../../ui/dist"] // relative to this crate
    struct Assets;

    pub async fn serve(uri: Uri) -> Response {
        let path = uri.path().trim_start_matches('/');
        // Unknown paths get the SPA entry point.
        let (path, file) = match Assets::get(path) {
            Some(f) if !path.is_empty() => (path, f),
            _ => match Assets::get("index.html") {
                Some(f) => ("index.html", f),
                None => return StatusCode::NOT_FOUND.into_response(),
            },
        };
        let cache = if path.starts_with("assets/") {
            "public, max-age=31536000, immutable"
        } else {
            "no-cache"
        };
        (
            [
                (header::CONTENT_TYPE, file.metadata.mimetype().to_owned()),
                (header::CACHE_CONTROL, cache.to_owned()),
            ],
            file.data,
        )
            .into_response()
    }
}
