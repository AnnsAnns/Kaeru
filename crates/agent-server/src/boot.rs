//! Shared boot wiring for the server binary (`agent-daemon`, M6): CLI parsing,
//! config load, sandbox host check, core + stores construction, banner, and
//! the tokio runtime with graceful shutdown.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use agent_core::{
    AgentCore, AuditLog, ClientMode, Config, ConversationRegistry, ConversationStore, MemoryStore,
    Paths, Reflector, Sandbox, TodoStore, ToolRegistry,
};
use tokio::signal;
use tracing_subscriber::EnvFilter;

use crate::routes::{self, AppState};
use crate::{bind, files};

/// Parsed command line shared by both server binaries.
#[derive(Debug, Default)]
pub struct Cli {
    pub paths: Paths,
    pub fake: bool,
    pub record: bool,
    /// `--bind host:port` (ADR-030 bind policy applies at startup).
    pub bind: Option<String>,
}

/// CLI parsing outcome: usage was requested, or a hard error.
#[derive(Debug)]
pub enum ParseError {
    /// `-h`/`--help`: print the text and exit 0.
    Help(String),
    /// A bad option: print the text and exit 2.
    Error(String),
}

/// The usage text for the server binary.
pub fn usage(bin: &str) -> String {
    format!(
        "\
kaeru {bin}

Usage: {bin} [OPTIONS]

Options:
      --config <PATH>     Config file [default: data/config.toml]
      --cassette <PATH>   Record/replay fixture file [default: data/cassette.json]
      --fake              Keyless mode: fake provider + recorded cassettes
      --record            Live mode, recording every interaction to the cassette
      --bind <ADDR>       Bind address (host:port): localhost (default) or a tailnet
                          address (100.64.0.0/10, fd7a:115e:a297::/48); requires
                          auth_token in the config (ADR-030)
  -h, --help              Print this help
"
    )
}

/// Parse the CLI.
pub fn parse_cli(bin: &str, args: impl Iterator<Item = String>) -> Result<Cli, ParseError> {
    let mut cli = Cli::default();
    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Err(ParseError::Help(usage(bin))),
            "--fake" => cli.fake = true,
            "--record" => cli.record = true,
            "--config" => {
                cli.paths.config = value_of(&mut args, &arg)?;
            }
            "--cassette" => {
                cli.paths.cassette = value_of(&mut args, &arg)?;
            }
            "--bind" => {
                let value = args
                    .next()
                    .ok_or_else(|| ParseError::Error(format!("{arg} needs a value")))?;
                cli.bind = Some(value);
            }
            other => {
                return Err(ParseError::Error(format!(
                    "unknown option {other:?}; try --help"
                )));
            }
        }
    }
    if cli.fake && cli.record {
        return Err(ParseError::Error(
            "--fake and --record are mutually exclusive".into(),
        ));
    }
    Ok(cli)
}

fn value_of(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<PathBuf, ParseError> {
    args.next()
        .map(PathBuf::from)
        .ok_or_else(|| ParseError::Error(format!("{flag} needs a value")))
}

/// Initialize tracing from `RUST_LOG`, defaulting to `info`.
pub fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
}

/// Everything a server binary needs after boot: the config, the resolved
/// paths, the router state, and the reflector (whose scheduler `run` spawns).
pub struct Boot {
    pub config: Config,
    pub paths: Paths,
    pub state: AppState,
    pub reflector: Arc<Reflector>,
}

/// Load the config, build the provider client, run the mandatory sandbox host
/// check (C11: fail closed), and wire the core, stores, registry, reflector
/// and router state. Identical for both server binaries.
pub fn bootstrap(cli: &Cli) -> Result<Boot, String> {
    let config =
        Config::load(&cli.paths.config).map_err(|err| format!("cannot load config: {err}"))?;

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
        Err(err) => return Err(format!("cannot set up the Python sandbox: {err}")),
    };
    if let Err(err) = sandbox.check_host() {
        return Err(err.message);
    }

    let memory = MemoryStore::new(cli.paths.memory.clone());
    // M7: named TODO lists, shared by the `todo` tool and the web TODO tab.
    let todos = TodoStore::new(cli.paths.todos.clone());
    let core = match AgentCore::connect(config.clone(), mode) {
        Ok(core) => Arc::new(
            core.with_tools(ToolRegistry::with_defaults(
                config.search.max_results,
                Some(memory.clone()),
                Some(Arc::clone(&sandbox)),
                Some(todos.clone()),
            ))
            .with_memory(memory)
            .with_persona(cli.paths.persona.clone())
            .with_audit(AuditLog::new(cli.paths.audit.clone())),
        ),
        Err(err) => return Err(format!("cannot start provider client: {err}")),
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
        Some(todos),
    );

    Ok(Boot {
        config,
        paths: cli.paths.clone(),
        state,
        reflector,
    })
}

/// Print the banner, build the tokio runtime, spawn the reflection scheduler,
/// bind, and serve until a shutdown signal. Blocks forever; exits the process
/// on bind/serve failure.
pub fn run(bin: &str, boot: Boot, addr: SocketAddr) -> ! {
    banner(bin, &addr, &boot);

    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    runtime.block_on(async move {
        // The reflection scheduler ticks on a local clock and catches up a
        // missed evening at boot (ADR-028). It is a no-op when disabled.
        tokio::spawn(agent_core::agent::reflect::scheduler(Arc::clone(
            &boot.reflector,
        )));
        // One owner process per data dir (arc42, concurrency): nothing else
        // guards it; a second instance simply fails to bind the port.
        let listener = match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => listener,
            Err(err) => {
                eprintln!("cannot bind {addr}: {err}");
                std::process::exit(1);
            }
        };
        let app = routes::router(boot.state);
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
    std::process::exit(0);
}

fn banner(bin: &str, addr: &SocketAddr, boot: &Boot) {
    let mode = match boot.state.core.mode() {
        ClientMode::Live => "live".to_owned(),
        ClientMode::Fake { cassette } => format!("fake (cassette: {})", cassette.display()),
        ClientMode::Record { cassette } => format!("record (cassette: {})", cassette.display()),
    };
    let auth = match boot.config.auth_token() {
        Some(_) => "X-Auth-Token required on /api/*".to_owned(),
        None => "disabled (set auth_token in the config before tunneling, M2)".to_owned(),
    };
    println!();
    println!("  kaeru | {bin}");
    println!("  listening : http://{addr} ({})", bind::scope_label(addr));
    println!(
        "  provider   : {} | model: {}",
        boot.config.provider.base_url, boot.config.provider.model
    );
    println!("  mode       : {mode}");
    let search = match boot.config.search.kind() {
        agent_core::SearchProviderKind::Off => {
            "off (set [search] provider to enable web_search)".to_owned()
        }
        kind => format!("{kind:?}"),
    };
    let reflect = match boot.state.reflector.as_ref() {
        Some(reflector) if reflector.enabled() => {
            format!("{} (daily, local time)", boot.config.reflect.time)
        }
        _ => "off (set [reflect] enabled = true)".to_owned(),
    };
    println!("  search     : {search}");
    println!("  reflect    : {reflect}");
    println!(
        "  sandbox    : {} ({}s, {} MiB, workspace {})",
        if boot.state.core.tools().get("python").is_some() {
            "on"
        } else {
            "off"
        },
        boot.config.sandbox.timeout_secs,
        boot.config.sandbox.memory_mb,
        boot.config.sandbox.workspace.display()
    );
    println!("  tools      : {}", boot.state.core.tools().len());
    println!(
        "  todos      : {} ({})",
        if boot.state.todos.is_some() {
            "on"
        } else {
            "off"
        },
        boot.paths.todos.display()
    );
    println!(
        "  audit      : {}",
        boot.state
            .core
            .audit()
            .path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "disabled".into())
    );
    println!("  auth       : {auth}");
    println!(
        "  history    : {} (survives restarts)",
        boot.paths.conversations.display()
    );
    println!("  config     : {}", boot.paths.config.display());
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
