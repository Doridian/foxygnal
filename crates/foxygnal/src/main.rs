use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use foxygnal_core::{Flowgraph, GraphSpec};

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
        /// Required to listen on a non-loopback address. There is no auth yet!
        #[arg(long)]
        allow_remote: bool,
        /// Directory with the built UI.
        #[arg(long, default_value = "ui/dist")]
        ui_dir: PathBuf,
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
        } => {
            if !bind.ip().is_loopback() && !allow_remote {
                bail!("refusing to listen on {bind}: pass --allow-remote (no authentication yet)");
            }
            let ui_dir = ui_dir.is_dir().then_some(ui_dir).or_else(|| {
                tracing::warn!("UI not built (run `npm run build` in ui/); serving API only");
                None
            });
            foxygnal_server::serve(foxygnal_server::Config { bind, ui_dir }, registry).await?;
        }
        Cmd::Run { graph } => {
            let text = std::fs::read_to_string(&graph)
                .with_context(|| format!("reading {}", graph.display()))?;
            let spec = GraphSpec::from_toml(&text)?;
            let running = Flowgraph::from_spec(&spec, &registry)?.start()?;
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
