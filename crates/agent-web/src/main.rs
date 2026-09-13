//! Kaeru `agent-web`: a thin axum adapter over `agent-core` (C2/ADR-010;
//! binds 127.0.0.1 only, C7). Modes: live (default), `--fake` (keyless UI on
//! the cassette/built-in fake provider), `--record` (live + appends every
//! interaction to the cassette file).

mod assets;
mod bridge;
mod error;
mod files;
mod markdown;
mod routes;

use std::path::PathBuf;
use std::sync::Arc;

use agent_core::{
    AgentCore, AuditLog, ClientMode, Config, ConversationRegistry, ConversationStore, MemoryStore,
    Paths, Reflector, Sandbox, ToolRegistry,
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

    // M5: the sandbox is mandatory (C11). Fail closed with instructions when
    // the host cannot provide bubblewrap/uv or user namespaces.
    let sandbox = match Sandbox::new(&config.sandbox, cli.paths.sandbox_envs.clone()) {
        Ok(sandbox) => Arc::new(sandbox),
        Err(err) => {
            eprintln!("cannot set up the Python sandbox: {err}");
            std::process::exit(1);
        }
    };
    if let Err(err) = sandbox.check_host() {
        eprintln!("{}", err.message);
        std::process::exit(1);
    }

    let memory = MemoryStore::new(cli.paths.memory.clone());
    let core = match AgentCore::connect(config.clone(), mode) {
        Ok(core) => Arc::new(
            core.with_tools(ToolRegistry::with_defaults(
                config.search.max_results,
                Some(memory.clone()),
                Some(Arc::clone(&sandbox)),
            ))
            .with_memory(memory)
            .with_persona(cli.paths.persona.clone())
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
    let registry = Arc::new(ConversationRegistry::new(Arc::clone(&core), store.clone()));

    // M4.5: evening reflection over the same stores, plus its scheduler task.
    let reflector = Arc::new(
        Reflector::from_core(Arc::clone(&core), store, cli.paths.reflect_state.clone())
            .expect("the core always has a memory store here"),
    );
    let state = AppState::new(
        Arc::clone(&core),
        registry,
        Some(Arc::clone(&reflector)),
        Some(files::Files {
            workspace: sandbox.workspace().to_path_buf(),
            max_upload_bytes: usize::try_from(
                config.files.max_upload_mb.saturating_mul(1024 * 1024),
            )
            .unwrap_or(usize::MAX),
        }),
    );

    banner(&cli, &config, &state);

    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    runtime.block_on(async move {
        // The reflection scheduler ticks on a local clock and catches up a
        // missed evening at boot (ADR-028). It is a no-op when disabled.
        tokio::spawn(agent_core::agent::reflect::scheduler(Arc::clone(
            &reflector,
        )));
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
    let reflect = match state.reflector.as_ref() {
        Some(reflector) if reflector.enabled() => {
            format!("{} (daily, local time)", config.reflect.time)
        }
        _ => "off (set [reflect] enabled = true)".to_owned(),
    };
    println!("  search     : {search}");
    println!("  reflect    : {reflect}");
    println!(
        "  sandbox    : {} ({}s, {} MiB, workspace {})",
        if state.core.tools().get("python").is_some() {
            "on"
        } else {
            "off"
        },
        config.sandbox.timeout_secs,
        config.sandbox.memory_mb,
        config.sandbox.workspace.display()
    );
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
