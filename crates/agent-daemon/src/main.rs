//! Kaeru `agent-daemon` (M6, ADR-011/030): owns `agent-core` + the `data/`
//! dir and serves the shared `agent-server` API + web UI to any number of
//! thin frontends. Binds localhost by default; `--bind` / `[daemon] bind` may
//! name a tailnet address (never a public one), which requires `auth_token`
//! in the config — startup fails closed without it.

use agent_server::{ParseError, bootstrap, parse_cli, run};

fn main() {
    agent_server::init_tracing();

    let cli = match parse_cli("agent-daemon", std::env::args().skip(1), true) {
        Ok(cli) => cli,
        Err(ParseError::Help(text)) => {
            eprintln!("{text}");
            std::process::exit(0);
        }
        Err(ParseError::Error(message)) => {
            eprintln!("{message}");
            std::process::exit(2);
        }
    };

    let boot = match bootstrap(&cli) {
        Ok(boot) => boot,
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(1);
        }
    };

    // ADR-030: CLI `--bind` wins over `[daemon] bind`; both are empty by
    // default, which resolves to localhost. A non-loopback bind must be a
    // tailnet address and requires the auth token (fail closed).
    let bind = cli.bind.as_deref().unwrap_or(&boot.config.daemon.bind);
    let addr = match agent_server::bind::resolve_bind(
        Some(bind),
        boot.config.port,
        boot.config.auth_token().is_some(),
    ) {
        Ok(addr) => addr,
        Err(err) => {
            eprintln!("invalid bind address: {err}");
            std::process::exit(1);
        }
    };
    run("agent-daemon", boot, addr);
}
