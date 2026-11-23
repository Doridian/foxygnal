use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use foxygnal_core::{DirLoader, Flowgraph, GraphSpec, Registry};
use foxygnal_server::{Security, Ui};

mod bench;
mod procstat;

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
        /// Allow graphs that transmit. Each run still has to be armed in the
        /// browser ("Arm TX and run"). Transmitting needs a licence and must
        /// be legal where you are.
        #[arg(long)]
        allow_tx: bool,
        /// Async worker threads for the web server. The DSP runs on its own
        /// threads; the server only shuffles small messages.
        #[arg(long, default_value_t = 2)]
        threads: usize,
    },
    /// Run a flowgraph file headless until it finishes (or Ctrl-C).
    Run {
        graph: PathBuf,
        /// Arm TX for this run: needed to run a graph with blocks that
        /// transmit. Transmitting needs a licence and must be legal where
        /// you are.
        #[arg(long)]
        arm_tx: bool,
    },
    /// Run a flowgraph headless for a while and report what it costs: CPU
    /// (process, per block, per thread), items/s per wire, memory.
    Bench {
        graph: PathBuf,
        /// Seconds to measure (stops earlier if the graph ends).
        #[arg(long, default_value_t = 10.0)]
        seconds: f64,
        /// Seconds to run before measuring.
        #[arg(long, default_value_t = 1.0)]
        warmup: f64,
        /// Maximum throughput: bypass throttles, replace SDR sources with an
        /// unpaced generator at the same rate. Default: run the graph as it
        /// is (real time if an SDR, throttle or audio block paces it).
        #[arg(long)]
        max: bool,
        /// Buffer size per output, in items (default: the engine's default).
        /// For experiments: a smaller buffer means less memory, a larger one
        /// fewer wake-ups.
        #[arg(long)]
        buffer: Option<usize>,
        /// Print the report as JSON.
        #[arg(long)]
        json: bool,
    },
    /// List available blocks.
    Blocks {
        #[arg(long)]
        json: bool,
    },
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let cli = Cli::parse();
    // The DSP runs on its own threads, so tokio only needs a few: one per
    // core (the default) would be 4 idle threads on a Pi, 32 on a big desktop.
    let rt = match &cli.cmd {
        Cmd::Serve { threads, .. } => tokio::runtime::Builder::new_multi_thread()
            .worker_threads((*threads).max(1))
            .enable_all()
            .build()?,
        _ => tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?,
    };
    rt.block_on(run(cli))
}

/// Read a graph file and resolve it (sub-graphs sit next to the file).
fn load(graph: &Path, registry: &Registry) -> anyhow::Result<GraphSpec> {
    let text =
        std::fs::read_to_string(graph).with_context(|| format!("reading {}", graph.display()))?;
    let spec = GraphSpec::from_toml(&text)?;
    let dir = graph.parent().map(PathBuf::from).unwrap_or_default();
    Ok(foxygnal_core::resolve(&spec, registry, &DirLoader(dir))?.graph)
}

/// The ids of blocks in `graph` that transmit.
fn tx_blocks<'a>(graph: &'a GraphSpec, registry: &Registry) -> Vec<&'a str> {
    graph
        .blocks
        .iter()
        .filter(|b| registry.is_tx(&b.kind))
        .map(|b| b.id.as_str())
        .collect()
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    let registry = foxygnal_blocks::registry();
    match cli.cmd {
        Cmd::Serve {
            bind,
            allow_remote,
            ui_dir,
            project_dir,
            allow_tx,
            threads: _,
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
                allow_tx,
            };
            foxygnal_server::serve(cfg, registry).await?;
        }
        Cmd::Run { graph, arm_tx } => {
            let resolved = load(&graph, &registry)?;
            let tx = tx_blocks(&resolved, &registry);
            if !tx.is_empty() && !arm_tx {
                bail!(
                    "this graph transmits (block {}); pass --arm-tx to run it. \
                     Transmitting needs a licence and must be legal where you are.",
                    tx.iter()
                        .map(|b| format!("`{b}`"))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
            // Armed for exactly this run; dropped (disarmed) when it returns.
            let _armed = (!tx.is_empty()).then(|| {
                tracing::warn!("TX armed: {} will transmit", tx.join(", "));
                foxygnal_sdr::arm_tx()
            });
            let running = Flowgraph::from_resolved(&resolved, &registry)?.start()?;
            let stop = running.stop_handle();
            tokio::spawn(async move {
                let _ = tokio::signal::ctrl_c().await;
                stop.stop();
            });
            tokio::task::spawn_blocking(move || running.wait()).await??;
        }
        Cmd::Bench {
            graph,
            seconds,
            warmup,
            max,
            buffer,
            json,
        } => {
            let mut resolved = load(&graph, &registry)?;
            let tx = tx_blocks(&resolved, &registry);
            if !tx.is_empty() {
                bail!(
                    "this graph transmits (block {}); bench doesn't transmit",
                    tx.join(", ")
                );
            }
            let notes = if max {
                bench::unpace(&mut resolved)?
            } else {
                Vec::new()
            };
            let mut fg = Flowgraph::from_resolved(&resolved, &registry)?;
            if let Some(items) = buffer {
                fg = fg.with_buffer_capacity(items);
            }
            let meta = fg.configure()?;
            let opts = bench::Options {
                seconds,
                warmup,
                max,
            };
            if !json {
                eprintln!(
                    "benchmarking {} for {:.1} s (+{:.1} s warm-up)…",
                    graph.display(),
                    seconds,
                    warmup
                );
            }
            let running = fg.start()?;
            let interrupted = Arc::new(AtomicBool::new(false));
            let flag = interrupted.clone();
            tokio::spawn(async move {
                let _ = tokio::signal::ctrl_c().await;
                flag.store(true, Ordering::Relaxed);
            });
            let name = graph.display().to_string();
            let report = tokio::task::spawn_blocking(move || {
                bench::measure(&name, &resolved, running, &meta, &opts, notes, &interrupted)
            })
            .await??;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print!("{}", bench::render(&report));
            }
        }
        Cmd::Blocks { json } => {
            if json {
                let all: Vec<_> = registry.catalog().collect();
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
