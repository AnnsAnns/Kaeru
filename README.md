# Kaeru

A personal, single-user agent: a platform-agnostic **Rust core** (`agent-core`)
with thin frontends on top. The first frontend is **agent-web**, a localhost
web UI that proxies any OpenAI-compatible chat provider (OpenRouter, Ollama,
LM Studio, vLLM, llama.cpp) with token-by-token streaming.

The plan and architecture live in [`docs/arc42-architecture.md`](docs/arc42-architecture.md);
implementation notes per milestone in [`docs/milestones/`](docs/milestones/).
Status: **M5 (sandboxed Python + file flow) code complete** — see
[`docs/milestones/M5.md`](docs/milestones/M5.md). Earlier notes:
[`docs/milestones/M4.5.md`](docs/milestones/M4.5.md),
[`docs/milestones/M4.md`](docs/milestones/M4.md),
[`docs/milestones/M3.md`](docs/milestones/M3.md),
[`docs/milestones/M2.5.md`](docs/milestones/M2.5.md),
[`docs/milestones/M2.md`](docs/milestones/M2.md),
[`docs/milestones/M1.md`](docs/milestones/M1.md). Next up is **M6 — the Discord
frontend / daemon split** (planned in [`docs/arc42-architecture.md`](docs/arc42-architecture.md) §1.3).

> M5 host requirement: the Python sandbox needs **Linux** with unprivileged
> user namespaces and `bwrap` (bubblewrap) + `uv` on `PATH`
> (`pacman -S bubblewrap uv` / `apt install bubblewrap uv` /
> `dnf install bubblewrap uv`). The server checks this at startup and refuses
> to boot without them.

## Quickstart

One command: the UI is embedded in the binary, with no build step.

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

[search]                   # web_search tool (M3); "off" disables it
provider = "off"           # "off" | "brave" | "tavily" | "searxng"
api_key  = ""              # brave/tavily
base_url = ""              # searxng, e.g. http://127.0.0.1:8888

[workers.summarizer]       # tool-free model that distills fetched pages (M3/ADR-021)
model = ""                 # empty = provider default

[sandbox]                  # Python tool + workspace files (M5)
workspace = "data/sandbox/workspace"   # the one writable folder
read_paths = []                        # extra read-only dirs for scripts
timeout_secs = 60
memory_mb = 512

[files]
max_upload_mb = 50         # upload cap for POST /api/files
```

```sh
cargo run -p agent-web            # live mode
```

The server **always binds to 127.0.0.1 only**; remote access is via
Cloudflare Tunnel (+ Access, plus the `auth_token` shared secret) — see
[`docs/deployment.md`](docs/deployment.md).

## Modes

| Mode | What it does |
|---|---|
| *(default)* | Live proxy to the configured provider |
| `--fake` | Keyless UI: replays `data/cassette.json` when a request matches, else a built-in canned answer |
| `--record` | Live, and appends every interaction to `data/cassette.json` (fixtures for offline tests, ADR-020) |

`--config <path>` / `--cassette <path>` override the default `data/…` locations.

## Threads, history & context

Chats are **threads**: the sidebar lists them (newest first by `updatedAt`),
and you can create, switch, and delete one. Each thread is persisted to its own
`data/conversations/{id}.json` (schema-versioned, atomic writes) after every
turn, so a browser reload (or a full server restart) restores every thread, its
history, and its accumulated token usage; the last-opened thread is remembered
in the browser. Long conversations stay within the deterministic prompt budget
(`[context] max_prompt_tokens`, default 16000): the oldest turns are folded into
a rolling summary (ADR-018) rather than dropped silently. Backup = copy `data/`.

Assistant replies are Markdown, rendered to sanitized HTML on the server
(ADR-026); the raw Markdown is stored and the core only ever carries raw text.

## Tools & web search

The model can call **tools**; a bounded loop (`[agent] max_steps`, default 8)
runs them and feeds the results back until it answers.

- **`web_search`** (enable with `[search] provider`): fetches the top pages and
  returns a **distilled, cited summary**. A tool-free **summarizer worker** — its
  own model from `[workers.summarizer]` — reads the raw pages; raw page text
  never enters the main model's context (ADR-021).
- **`memory_write`** always asks for consent first (one tap in the UI): memory
  persists across sessions, so a silent write would be a prompt-injection vector
  (ADR-016).
- **`python`** (M5) runs a script in a **bubblewrap sandbox**: offline, with the
  workspace as its only writable path, and resource limits (wall clock, CPU,
  memory, file size, output). Scripts with no dependencies run immediately;
  requesting packages asks for a one-time `PackageInstall` consent (prepared
  dep sets are cached and never ask again), and a script that needs the network
  needs an explicit `NetworkAccess` consent. Files the script writes appear in
  the chat as artifacts — images inline, everything downloadable.
- **Workspace files** (M5): the 📎 button uploads into `data/sandbox/workspace`
  (the same folder scripts work in) and shows an inline preview for images.
  Uploads are attached to your message and artifacts to the reply, so both are
  **embedded again after a reload**; `GET /api/files/…` serves them
  authenticated with sanitized paths (no traversal, no symlink escape), images
  inline and everything else as a download (text/CSV/JSON open in a tab).
  Stored in the conversation as message `artifacts` (schema v3).

Tool outputs are **fenced as untrusted data** before the model sees them, every
tool execution and consent decision is appended to `data/audit.jsonl`, and the
UI shows collapsible tool steps plus allow/deny consent cards. Turns survive a
dropped connection: reloading the page re-attaches to a running turn and replays
it (§6.3a). The **↻** button regenerates the last answer.

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
crates/agent-core/   library: config, events, LLM client (SSE), fake provider, conversations, context, ChatSession, ConversationRegistry, agent loop, tools (web_search/memory/python), sandbox supervisor (uv + bubblewrap), search, workers, memory, audit
crates/agent-web/    axum frontend: routes, SSE bridge, markdown renderer, file flow, embedded UI (rust-embed)
data/                created at runtime: config.toml, cassette.json, conversations/, audit.jsonl, memory/, persona.md, sandbox/{workspace,envs}/
Bort/                the owner's blog, visual design reference ONLY — never built or imported
scripts/             dev tooling (mock provider server for local end-to-end runs)
```

## Development

```sh
cargo test                       # full offline suite (fake provider, no network)
cargo test -p agent-core         # core tests without compiling any frontend
cargo fmt && cargo clippy --all-targets -- -D warnings
node scripts/mock-openai-server.js   # + `--record` for a local end-to-end run
MOCK_TOOL=python node scripts/mock-openai-server.js   # scripts a sandbox run (M5)
```

## License

MIT — see [LICENSE](LICENSE).
