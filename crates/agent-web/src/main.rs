//! Kaeru `agent-web`: the localhost standalone binary (C7) — the shared
//! `agent-server` machinery bound to 127.0.0.1. Modes: live (default),
//! `--fake` (keyless UI on the cassette/built-in fake provider), `--record`
//! (live + appends every interaction to the cassette file).

use agent_server::{ParseError, bootstrap, parse_cli, run};

fn main() {
    agent_server::init_tracing();

    let cli = match parse_cli("agent-web", std::env::args().skip(1), false) {
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

    // C7: agent-web always binds localhost; a tailnet bind is the daemon's
    // job (ADR-030). The default can never fail the policy check.
    let addr = agent_server::bind::resolve_bind(None, boot.config.port, false)
        .expect("the default bind is loopback");
    run("agent-web", boot, addr);
}
