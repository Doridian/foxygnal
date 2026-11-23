use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use foxygnal_core::{DirLoader, Flowgraph, GraphSpec};
use foxygnal_server::{Security, Ui};

#[derive(Parser)]
#[command(
    name = "foxygnal",
    version,
    about = "Fast, friendly software-defined radio flowgraphs"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Start the browser UI (default: http://127.0.0.1:7355).
    Serve {
        #[arg(long, default_value = "127.0.0.1:7355")]
        bind: SocketAddr,
        /// Allow a non-loopback address. Access then needs a random token,
        /// printed at startup. Put a TLS reverse proxy in front.
        #[arg(long)]
        allow_remote: bool,
        /// Directory with the built UI. Defaults to the UI embedded in the
        /// binary if there is one, else `ui/dist`.
        #[arg(long)]
        ui_dir: Option<PathBuf>,
        /// Where the browser lists, opens and saves `.fox.toml` files.
        #[arg(long, default_value = ".")]
        project_dir: PathBuf,
    },
    /// Run a flowgraph file headless until it finishes (or Ctrl-C).
    Run { graph: PathBuf },
    /// List available blocks.
    Blocks {
        #[arg(long)]
        json: bool,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let registry = foxygnal_blocks::registry();
    match Cli::parse().cmd {
        Cmd::Serve {
            bind,
            allow_remote,
            ui_dir,
            project_dir,
        } => {
            if !bind.ip().is_loopback() && !allow_remote {
                bail!("refusing to listen on {bind}: pass --allow-remote to allow it");
            }
            if !project_dir.is_dir() {
                bail!("project dir {} does not exist", project_dir.display());
            }
            let ui = match ui_dir {
                Some(dir) if dir.is_dir() => Ui::Dir(dir),
                Some(dir) => bail!("UI dir {} does not exist", dir.display()),
                None => Ui::embedded().unwrap_or_else(|| {
                    let dir = PathBuf::from("ui/dist");
                    if dir.is_dir() {
                        Ui::Dir(dir)
                    } else {
                        tracing::warn!(
                            "UI not built (run `npm run build` in ui/); serving the API only"
                        );
                        Ui::None
                    }
                }),
            };
            let security = Security {
                allow_remote,
                token: allow_remote.then(Security::random_token),
            };
            let cfg = foxygnal_server::Config {
                bind,
                ui,
                project_dir,
                security,
            };
            foxygnal_server::serve(cfg, registry).await?;
        }
        Cmd::Run { graph } => {
            let text = std::fs::read_to_string(&graph)
                .with_context(|| format!("reading {}", graph.display()))?;
            let spec = GraphSpec::from_toml(&text)?;
            // Sub-graphs (`graph:NAME.fox.toml`) sit next to the graph file.
            let dir = graph.parent().map(PathBuf::from).unwrap_or_default();
            let running = Flowgraph::from_spec_with(&spec, &registry, &DirLoader(dir))?.start()?;
            let stop = running.stop_handle();
            tokio::spawn(async move {
                let _ = tokio::signal::ctrl_c().await;
                stop.stop();
            });
            tokio::task::spawn_blocking(move || running.wait()).await??;
        }
        Cmd::Blocks { json } => {
            if json {
                let all: Vec<_> = registry.descriptors().collect();
                println!("{}", serde_json::to_string_pretty(&all)?);
            } else {
                for d in registry.descriptors() {
                    println!("{:<16} {:<10} {}", d.kind, d.category, d.label);
                }
            }
        }
    }
    Ok(())
}
