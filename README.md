# Kaeru

A personal, single-user agent: a platform-agnostic **Rust core** (`agent-core`)
with thin frontends on top. The first frontend is **agent-web**, a localhost
web UI that proxies any OpenAI-compatible chat provider (OpenRouter, Ollama,
LM Studio, vLLM, llama.cpp) with token-by-token streaming.

The plan and architecture live in [`docs/arc42-architecture.md`](docs/arc42-architecture.md);
implementation notes per milestone in [`docs/milestones/`](docs/milestones/).
Status: **M1 (Rust chat core) done** — see [`docs/milestones/M1.md`](docs/milestones/M1.md).

## Quickstart

```sh
cargo run -p agent-web -- --fake   # keyless: boots the full UI on the fake provider
# open http://127.0.0.1:8080
```

For a real provider, edit `data/config.toml` (created on first run):

```toml
port = 8080
auth_token = ""            # set a long random value before exposing via a tunnel (M2)

[provider]
base_url = "https://openrouter.ai/api/v1"   # or http://127.0.0.1:11434/v1 for Ollama, ...
api_key  = "sk-or-v1-..."                   # empty for local servers that need no auth
model    = "openai/gpt-4o-mini"
```

```sh
cargo run -p agent-web            # live mode
```

The server **always binds to 127.0.0.1 only**; remote access arrives in M2
via Cloudflare Tunnel (+ Access, plus the `auth_token` shared secret).

## Modes

| Mode | What it does |
|---|---|
| *(default)* | Live proxy to the configured provider |
| `--fake` | Keyless UI: replays `data/cassette.json` when a request matches, else a built-in canned answer |
| `--record` | Live, and appends every interaction to `data/cassette.json` (fixtures for offline tests, ADR-020) |

`--config <path>` / `--cassette <path>` override the default `data/…` locations.

## Security notes

- The provider API key lives **only** in `data/config.toml` (written with
  0600 permissions, git-ignored). It is never sent to the browser.
- Nothing listens beyond localhost (C7).
- When `auth_token` is set, every `/api/*` request must carry
  `X-Auth-Token: <token>`; static assets stay public and cacheable, API
  responses are never cacheable.
- `--fake`/`--record` never read your key: record talks to the provider like
  live mode, fake mode never does.

## Layout

```
crates/agent-core/   library: config, events, LLM client (SSE), fake provider, ChatSession
crates/agent-web/    axum frontend: routes, SSE bridge, embedded UI (rust-embed)
data/                created at runtime: config.toml, cassette.json, (M2+) memory/
Bort/                the owner's blog, visual design reference ONLY — never built or imported
scripts/             dev tooling (mock provider server for local end-to-end runs)
```

## Development

```sh
cargo test                       # full offline suite (fake provider, no network)
cargo test -p agent-core         # core tests without compiling any frontend
cargo fmt && cargo clippy --all-targets -- -D warnings
node scripts/mock-openai-server.js   # + `--record` for a local end-to-end run
```

## License

MIT — see [LICENSE](LICENSE).
