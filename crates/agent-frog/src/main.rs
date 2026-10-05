//! Kaeru Wayland frog — a thin M6 frontend (ADR-011/030/031).
//!
//! A small, always-on-top 🐸 widget for quickly asking the daemon something.
//! It is a pure client of the same `/api/*` wire API the web UI uses, so it
//! sees the same threads, memory, tools and consent cards. Point it at a
//! localhost or tailnet daemon URL and give it the shared token; nothing else
//! is required (no core, no `data/` on the client host).
//!
//! ```text
//! agent-frog --url http://100.64.0.2:8080 --token "$(cat token)"
//! ```

mod client;
mod config;
mod sprite;
mod theme;
mod ui;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use gtk::prelude::*;

use client::DaemonClient;
use config::FrogConfig;

/// Parsed command line. Options override the config file.
#[derive(Debug, Default)]
struct Cli {
    url: Option<String>,
    token: Option<String>,
    thread: Option<String>,
    theme: Option<String>,
    corner: Option<String>,
    no_layer_shell: bool,
    expanded: bool,
    config: Option<PathBuf>,
}

#[derive(Debug)]
enum ParseError {
    Help(String),
    Error(String),
}

fn usage(bin: &str) -> String {
    format!(
        "\
kaeru {bin} — a Wayland desktop frog for the Kaeru daemon

Usage: {bin} [OPTIONS]

Options:
      --url <URL>         Daemon base URL [default: {default_url}]
                          A tailnet address reaches a remote daemon (ADR-030).
      --token <TOKEN>     X-Auth-Token shared secret (required beyond localhost)
      --thread <ID>       Pin a thread id [default: newest, created on demand]
      --theme <NAME>      Start theme (latenightbath, ayy4, curiosities,
                          sunnyswamp, standard_og, werwolvdark, nostalgia)
      --corner <CORNER>   bottom-right, bottom-left, top-right, top-left
      --config <PATH>     Config file [default: ~/.config/kaeru/frog.toml]
      --expanded          Start with the chat panel open
      --no-layer-shell    Use a normal window (compositors without wlr-layer-shell)
  -h, --help              Print this help
",
        default_url = config::DEFAULT_URL,
    )
}

fn parse_cli(bin: &str, args: impl Iterator<Item = String>) -> Result<Cli, ParseError> {
    let mut cli = Cli::default();
    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        let mut value = |flag: &str| -> Result<String, ParseError> {
            args.next()
                .ok_or_else(|| ParseError::Error(format!("{flag} needs a value")))
        };
        match arg.as_str() {
            "-h" | "--help" => return Err(ParseError::Help(usage(bin))),
            "--url" => cli.url = Some(value("--url")?),
            "--token" => cli.token = Some(value("--token")?),
            "--thread" => cli.thread = Some(value("--thread")?),
            "--theme" => cli.theme = Some(value("--theme")?),
            "--corner" => cli.corner = Some(value("--corner")?),
            "--config" => cli.config = Some(PathBuf::from(value("--config")?)),
            "--expanded" => cli.expanded = true,
            "--no-layer-shell" => cli.no_layer_shell = true,
            other => {
                return Err(ParseError::Error(format!(
                    "unknown option {other:?}; try --help"
                )));
            }
        }
    }
    Ok(cli)
}

fn load_config(cli: &Cli) -> Result<(FrogConfig, PathBuf), String> {
    let path = cli.config.clone().unwrap_or_else(config::default_path);
    let mut config = FrogConfig::load(&path)?;
    if let Some(url) = &cli.url {
        config.url = url.clone();
    }
    if let Some(token) = &cli.token {
        config.token = token.clone();
    }
    if let Some(thread) = &cli.thread {
        config.thread = thread.clone();
    }
    if let Some(theme) = &cli.theme {
        config.theme = theme.clone();
    }
    if let Some(corner) = &cli.corner {
        config.corner = corner.clone();
    }
    Ok((config, path))
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let bin = args.first().cloned().unwrap_or_else(|| "agent-frog".into());
    let cli = match parse_cli(&bin, args.into_iter().skip(1)) {
        Ok(cli) => cli,
        Err(ParseError::Help(text)) => {
            print!("{text}");
            return ExitCode::SUCCESS;
        }
        Err(ParseError::Error(text)) => {
            eprintln!("{text}");
            return ExitCode::from(2);
        }
    };
    init_tracing();

    let (config, config_path) = match load_config(&cli) {
        Ok(loaded) => loaded,
        Err(err) => {
            eprintln!("agent-frog: {err}");
            return ExitCode::FAILURE;
        }
    };
    let token = config.token().map(str::to_owned);
    let client = Arc::new(DaemonClient::new(config.base_url(), token));

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("agent-frog: cannot start the runtime: {err}");
            return ExitCode::FAILURE;
        }
    };
    let handle = runtime.handle().clone();

    let app = gtk::Application::builder()
        .application_id("dev.kaeru.Frog")
        .build();
    let use_layer_shell = !cli.no_layer_shell;
    let start_expanded = cli.expanded;
    app.connect_activate(move |app| {
        ui::build(
            app,
            Arc::clone(&client),
            handle.clone(),
            config.clone(),
            config_path.clone(),
            use_layer_shell,
            start_expanded,
        );
    });

    // Pass no extra argv to GTK: our own flags were parsed above.
    let code = app.run_with_args(&["agent-frog"]);
    ExitCode::from(code.get())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, ParseError> {
        parse_cli("agent-frog", args.iter().map(|arg| (*arg).to_owned()))
    }

    #[test]
    fn parses_flags_and_values() {
        let cli = parse(&["--url", "http://100.64.0.2:8080", "--token", "t"]).unwrap();
        assert_eq!(cli.url.as_deref(), Some("http://100.64.0.2:8080"));
        assert_eq!(cli.token.as_deref(), Some("t"));
        assert!(!cli.no_layer_shell);
    }

    #[test]
    fn missing_value_is_an_error() {
        assert!(matches!(parse(&["--url"]), Err(ParseError::Error(_))));
    }

    #[test]
    fn unknown_flag_is_an_error() {
        assert!(matches!(parse(&["--wat"]), Err(ParseError::Error(_))));
    }

    #[test]
    fn help_short_circuits() {
        assert!(matches!(parse(&["--help"]), Err(ParseError::Help(_))));
    }

    #[test]
    fn cli_overrides_the_config_file() {
        let cli = parse(&[
            "--url",
            "http://100.64.0.9:9000",
            "--theme",
            "nostalgia",
            "--config",
            "/tmp/kaeru-frog-does-not-exist/frog.toml",
        ])
        .unwrap();
        let (config, _path) = load_config(&cli).unwrap();
        assert_eq!(config.base_url(), "http://100.64.0.9:9000");
        assert_eq!(config.theme, "nostalgia");
    }
}
