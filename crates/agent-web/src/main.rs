//! Kaeru `agent-web`: the first frontend (C7: binds 127.0.0.1 only).
//!
//! Thin platform adapter: CLI + config + `AgentCore` + axum router. All logic
//! lives in `agent-core` (C2/ADR-010).
//!
//! Modes:
//! - (default)  live proxy to the configured OpenAI-compatible provider
//! - `--fake`   keyless UI on the fake provider (cassette `data/cassette.json`
//!   when present, built-in canned responses otherwise)
//! - `--record` live, and appends every interaction to the cassette file

mod assets;
mod bridge;
mod error;
mod markdown;
mod routes;

use std::path::PathBuf;
use std::sync::Arc;

use agent_core::{
    AgentCore, AuditLog, ClientMode, Config, ConversationRegistry, ConversationStore, MemoryStore,
    Paths, ToolRegistry,
};
use tokio::signal;
use tracing_subscriber::EnvFilter;

use routes::AppState;

const USAGE: &str = "\
kaeru agent-web

Usage: agent-web [OPTIONS]

Options:
      --config <PATH>     Config file [default: data/config.toml]
      --cassette <PATH>   Record/replay fixture file [default: data/cassette.json]
      --fake              Keyless mode: fake provider + recorded cassettes
      --record            Live mode, recording every interaction to the cassette
  -h, --help              Print this help
";

#[derive(Debug, Default)]
struct Cli {
    paths: Paths,
    fake: bool,
    record: bool,
}

fn parse_cli(args: impl Iterator<Item = String>) -> Result<Cli, String> {
    let mut cli = Cli::default();
    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Err(USAGE.to_owned()),
            "--fake" => cli.fake = true,
            "--record" => cli.record = true,
            "--config" => {
                cli.paths.config = value_of(&mut args, &arg)?;
            }
            "--cassette" => {
                cli.paths.cassette = value_of(&mut args, &arg)?;
            }
            other => return Err(format!("unknown option {other:?}; try --help")),
        }
    }
    if cli.fake && cli.record {
        return Err("--fake and --record are mutually exclusive".into());
    }
    Ok(cli)
}

fn value_of(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<PathBuf, String> {
    args.next()
        .map(PathBuf::from)
        .ok_or_else(|| format!("{flag} needs a value"))
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = match parse_cli(std::env::args().skip(1)) {
        Ok(cli) => cli,
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(if message == USAGE { 0 } else { 2 });
        }
    };

    let config = match Config::load(&cli.paths.config) {
        Ok(config) => config,
        Err(err) => {
            eprintln!("cannot load config: {err}");
            std::process::exit(1);
        }
    };

    let mode = if cli.fake {
        ClientMode::Fake {
            cassette: cli.paths.cassette.clone(),
        }
    } else if cli.record {
        ClientMode::Record {
            cassette: cli.paths.cassette.clone(),
        }
    } else {
        ClientMode::Live
    };

    if matches!(mode, ClientMode::Live)
        && config.provider.api_key.trim().is_empty()
        && config.provider.base_url == agent_core::DEFAULT_BASE_URL
    {
        tracing::warn!(
            "no [provider] api_key configured for OpenRouter; chat turns will fail. \
             Set it in {} or run with --fake for keyless development.",
            cli.paths.config.display()
        );
    }

    let core = match AgentCore::connect(config.clone(), mode) {
        Ok(core) => Arc::new(
            core.with_tools(ToolRegistry::with_defaults(
                config.search.max_results,
                Some(MemoryStore::new(cli.paths.memory.clone())),
            ))
            .with_audit(AuditLog::new(cli.paths.audit.clone())),
        ),
        Err(err) => {
            eprintln!("cannot start provider client: {err}");
            std::process::exit(1);
        }
    };

    // M2.5: threads are owned by a core registry over the plain-file store.
    // Nothing is created up front — the UI lists existing threads and creates
    // its first one on demand (reload falls back to newest/fresh, §6.6).
    let store = ConversationStore::new(cli.paths.conversations.clone());
    let registry = Arc::new(ConversationRegistry::new(Arc::clone(&core), store));
    let state = AppState::new(core, registry);

    banner(&cli, &config, &state);

    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    runtime.block_on(async move {
        // One owner process per data dir (arc42, concurrency): nothing else
        // guards it in M1; a second instance simply fails to bind the port.
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], config.port));
        let listener = match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => listener,
            Err(err) => {
                eprintln!("cannot bind {addr}: {err}");
                std::process::exit(1);
            }
        };
        let app = routes::router(state);
        match axum::serve(listener, app)
            .with_graceful_shutdown(shutdown_signal())
            .await
        {
            Ok(()) => tracing::info!("bye"),
            Err(err) => {
                eprintln!("server error: {err}");
                std::process::exit(1);
            }
        }
    });
}

fn banner(cli: &Cli, config: &Config, state: &AppState) {
    let mode = match state.core.mode() {
        ClientMode::Live => "live".to_owned(),
        ClientMode::Fake { cassette } => format!("fake (cassette: {})", cassette.display()),
        ClientMode::Record { cassette } => format!("record (cassette: {})", cassette.display()),
    };
    let auth = match config.auth_token() {
        Some(_) => "X-Auth-Token required on /api/*".to_owned(),
        None => "disabled (set auth_token in the config before tunneling, M2)".to_owned(),
    };
    println!();
    println!("  kaeru | agent-web");
    println!(
        "  listening : http://127.0.0.1:{} (localhost only)",
        config.port
    );
    println!(
        "  provider   : {} | model: {}",
        config.provider.base_url, config.provider.model
    );
    println!("  mode       : {mode}");
    let search = match config.search.kind() {
        agent_core::SearchProviderKind::Off => {
            "off (set [search] provider to enable web_search)".to_owned()
        }
        kind => format!("{kind:?}"),
    };
    println!("  search     : {search}");
    println!("  tools      : {}", state.core.tools().len());
    println!(
        "  audit      : {}",
        state
            .core
            .audit()
            .path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "disabled".into())
    );
    println!("  auth       : {auth}");
    println!(
        "  history    : {} (survives restarts)",
        cli.paths.conversations.display()
    );
    println!("  config     : {}", cli.paths.config.display());
    println!();
}

async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c().await.expect("install ctrl-c handler");
    };
    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("shutdown signal received");
}
