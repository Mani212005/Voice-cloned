//! `vcd`: the local voice-clone service.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use clap::{Parser, Subcommand};
use vc_backend::{CrispCliBackend, CrispOptions, RecipeManifest, RuntimePin};
use vc_core::SystemClock;
use vc_engine::{Engine, EngineConfig, EngineContext};
use vc_store::{DataDir, Store};

mod config;

use config::Config;

#[derive(Debug, Parser)]
#[command(name = "vcd", version, about = "Local voice-clone service")]
struct Cli {
    /// Path to vcd.toml (defaults apply without one).
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Verify the models and serve the HTTP API.
    Serve {
        /// Overrides server.listen (loopback only).
        #[arg(long)]
        listen: Option<std::net::SocketAddr>,
        /// Overrides storage.data_dir.
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Overrides engine.models_dir.
        #[arg(long)]
        models_dir: Option<PathBuf>,
    },
    /// Model files and the engine runtime.
    Models {
        #[command(subcommand)]
        command: ModelsCommand,
    },
}

#[derive(Debug, Subcommand)]
enum ModelsCommand {
    /// Check sizes and sha256 of the model files and the engine binary.
    Verify {
        #[arg(long)]
        models_dir: Option<PathBuf>,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("VCD_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();
    let mut cfg = Config::load(cli.config.as_deref())?;
    match cli.command {
        Command::Serve {
            listen,
            data_dir,
            models_dir,
        } => {
            if let Some(l) = listen {
                cfg.server.listen = l;
            }
            if let Some(d) = data_dir {
                cfg.storage.data_dir = d;
            }
            if let Some(m) = models_dir {
                cfg.engine.models_dir = m;
            }
            cfg.validate()?;
            serve(cfg)
        }
        Command::Models {
            command: ModelsCommand::Verify { models_dir },
        } => {
            if let Some(m) = models_dir {
                cfg.engine.models_dir = m;
            }
            verify_models(&cfg)?;
            println!(
                "models and runtime verified in {}",
                cfg.engine.models_dir.display()
            );
            Ok(())
        }
    }
}

fn verify_models(cfg: &Config) -> anyhow::Result<()> {
    let manifest = RecipeManifest::turbo2();
    let pin = RuntimePin::crispasr();
    let root = &cfg.engine.models_dir;
    manifest
        .verify_files(&root.join("files"))
        .context("model files (run `cargo xtask fetch-models`)")?;
    pin.verify_binary(root)
        .context("engine runtime (run `cargo xtask fetch-models`)")?;
    Ok(())
}

fn serve(cfg: Config) -> anyhow::Result<()> {
    verify_models(&cfg)?;
    let manifest = RecipeManifest::turbo2();
    let pin = RuntimePin::crispasr();
    let models_dir = std::path::absolute(&cfg.engine.models_dir)?;
    let backend = CrispCliBackend::new(
        manifest,
        &pin,
        CrispOptions {
            runtime_bin: pin.binary_path(&models_dir),
            model_dir: models_dir.join("files"),
            threads: cfg.engine.threads,
            nice: cfg.engine.nice,
            extra_env: vec![],
        },
    );

    let data = DataDir::open(&cfg.storage.data_dir)
        .with_context(|| format!("data dir {}", cfg.storage.data_dir.display()))?;
    let store = Arc::new(Store::open(&data.db_path())?);
    let head = store
        .verify_consent_chain()
        .context("consent records failed verification")?;
    let ttl_ms = i64::from(cfg.storage.output_ttl_hours) * 3_600_000;
    let ctx = EngineContext {
        store,
        data,
        backend: Arc::new(backend),
        clock: Arc::new(SystemClock),
        config: EngineConfig {
            queue_capacity: cfg.engine.queue_capacity,
            output_ttl_ms: ttl_ms,
            inter_chunk_gap_ms: cfg.engine.inter_chunk_gap_ms,
            crash_retries: cfg.engine.crash_retries,
            ..EngineConfig::default()
        },
    };

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        let engine = Engine::start(ctx.clone())?;
        let sweep_every = Duration::from_secs(u64::from(cfg.storage.sweep_interval_minutes) * 60);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(sweep_every);
            loop {
                tick.tick().await;
                let ctx = ctx.clone();
                let _ = tokio::task::spawn_blocking(move || vc_engine::sweep_expired(&ctx)).await;
            }
        });
        let app = vc_api::router(vc_api::AppState {
            engine: engine.clone(),
            config: vc_api::ApiConfig {
                max_body_bytes: cfg.server.max_body_bytes,
                max_job_chars: cfg.server.max_job_chars,
            },
            app_version: env!("CARGO_PKG_VERSION"),
        });
        let listener = tokio::net::TcpListener::bind(cfg.server.listen)
            .await
            .with_context(|| format!("bind {}", cfg.server.listen))?;
        tracing::info!(
            listen = %cfg.server.listen,
            backend = "turbo2",
            runtime = %pin.version,
            consent_chain_head = %head,
            "vcd serving"
        );
        axum::serve(listener, app)
            .with_graceful_shutdown(shutdown_signal())
            .await?;
        tracing::info!("stopping the engine");
        engine.shutdown().await;
        anyhow::Ok(())
    })
}

async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(_) => return std::future::pending().await,
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}
