# AGENTS.md — working in Kaeru

Kaeru is a personal, single-user agent. A platform-agnostic Rust library
(`agent-core`) is embedded by thin frontends; the first and only frontend so
far is `agent-web`, an axum server that embeds a vanilla-JS chat UI and
streams from any OpenAI-compatible provider.

`docs/arc42-architecture.md` is the **authoritative spec** (constraints C1–C17,
ADRs, milestone roadmap M0–M6, acceptance checks). Read it before large
changes; per-milestone implementation notes live in `docs/milestones/`.

## Commands

```sh
cargo run -p agent-web -- --fake     # keyless UI on the fake provider (http://127.0.0.1:8080)
cargo run -p agent-web               # live provider
cargo test                           # full offline suite (fake provider, no network)
cargo test -p agent-core             # core tests without compiling the frontend
cargo fmt --all --check && cargo clippy --all-targets -- -D warnings
node scripts/mock-openai-server.js   # dev-only OpenAI-compatible server for live-path e2e
```

`cargo run` works from the repo root: the server resolves `data/…` relative to
the **current working directory** (not the binary), and the fake provider
paints a delay so streaming is visible. Run from the repo root so `data/`
lands where you expect.

CI (`.github/workflows/ci.yml`) runs fmt check, clippy `-D warnings`,
`cargo test -p agent-core`, `cargo test`, then a release build. Match that
locally before finishing. Toolchain is Rust **edition 2024**.

## Layout

```
crates/agent-core/     library: config, events, llm (client/sse/fake/types), session, context, conversations, error
crates/agent-web/      axum binary: main.rs (CLI), routes.rs, bridge.rs, error.rs, assets.rs, assets/ (embedded UI)
data/                  runtime state, CWD-relative, git-ignored: config.toml (0600) + conversations/{id}.json
scripts/               dev tooling (mock provider server)
Bort/                  owner's blog — design reference ONLY (see below)
```

## Hard invariants (do not violate)

- **C2 — separation:** no frontend concept (HTTP, HTML, Discord, axum) may
  enter `agent-core`; frontends only translate `CoreEvent`s. `agent-core`
  must compile and pass `cargo test -p agent-core` with no frontend present.
- **C15 — boring stack:** new dependencies require an ADR in the arc42 doc.
  The current set is small and deliberate (axum, tokio, reqwest, serde, toml,
  rust-embed, tokio-stream, tracing).
- **ADR-005 — protocol isolation:** all OpenAI-compatible wire knowledge lives
  in `crates/agent-core/src/llm/`. Nothing else may depend on provider shapes.
- **C17 — Bort is reference-only:** `Bort/` is a nested git repo (the owner's
  Astro/Tailwind blog), git-ignored via `Bort/*`. It is never imported, never
  built, and deleting it must change nothing. Port design *by hand*: tokens
  into `crates/agent-web/assets/app.css`, fonts copied as static `.otf` files.
  Themes follow Bort's enum order in `app.js` (the `THEMES` array skips
  `trans`, which CSS still ships); persisted under localStorage key
  `kaeru-theme`, auth token under `kaeru-auth-token`.
- **C7 — localhost only:** the server always binds `127.0.0.1`. Browser gating
  is `X-Auth-Token` on `/api/*` (enforced only when `auth_token` is set;
  static assets stay public). CORS is intentionally absent (same-origin).
- **C13 — single binary:** UI assets are embedded via rust-embed. Editing
  anything under `crates/agent-web/assets/` requires a rebuild; the running
  binary keeps the previously compiled assets.

## Milestone discipline

The roadmap is staged; variants and stubs exist on purpose. Do not implement
later-milestone features opportunistically, and don't "clean up" intentional
stubs. Examples:

- `CoreEvent` is the full enum from day one, but M1 uses only
  `Delta`/`TurnDone`/`Error`; `ToolCall`/`ToolResult`/`ApprovalRequest` arrive
  with the M3 agent loop, `Artifact` with M5 files.
- `ChatSession::approve` is a documented stub returning an error until M3.
- `EventStream` is a live `broadcast` tap; bounded replay for reconnects is
  M3. M1 semantics: a dropped subscriber aborts the turn.
- There are no tools, memory, sandbox, or multi-conversation features yet
  (M2 persistence of a single `"default"` conversation is as far as it goes).
- Don't add protocol fields "now that we're here" — stage them.

When you complete milestone work, update the matching `docs/milestones/Mx.md`
notes (the repo treats those as the decision log); significant design changes
belong in `docs/arc42-architecture.md` as a new ADR/version row.

## Non-obvious patterns & gotchas

- **Wire formats differ by layer.** `CoreEvent` uses a snake_case `type` tag
  (`{"type":"delta","text":"…"}`); SSE adds an `event:` name from
  `CoreEvent::event_name()`. Persisted conversation JSON uses **camelCase**
  (`createdAt`, `toolCallId`) with `"schema": 1`. Config TOML uses
  `#[serde(deny_unknown_fields)]`, so unknown keys are hard errors.
- **Abort is `Error { kind: Aborted }`,** not a dedicated variant — the enum is
  doc-fixed. The frontend renders it as "stopped", not an error card.
- **Turn/abort race:** `abort()` flushes partial text under the session mutex
  and the turn finalizer is id-guarded (`turn_id` must still be current), so
  history flushes exactly once. A rare late delta after the aborted event is
  known and accepted until M3's serialized emitter.
- **Retry semantics:** on provider failure with zero assistant output the
  trailing user message is popped so retry doesn't duplicate it. Failure
  *after* partial output keeps everything (retry then duplicates the user
  message — accepted edge).
- **Context budget** (`context.rs`, ADR-018) is deterministic (~4 chars/token)
  and runs *before* the provider call: drop-oldest window + rolling summary.
  The last message is always kept even when it alone exceeds the budget, and
  the window is aligned to a `user` boundary. Summarization never fails: it
  degrades to a deterministic excerpt.
- **SSE parser** (`llm/sse.rs`) is **byte-level**, not line-level — chunks can
  split a line mid-UTF-8. It has no OpenAI knowledge; `[DONE]` handling lives
  in `client.rs`.
- **Compat risk:** `stream_options: {"include_usage": true}` is sent and the
  mock accepts it; if an exotic server 400s, drop the field (usage is optional
  end-to-end). OpenRouter `HTTP-Referer`/`X-Title` are deliberately not sent.
- **Fake/record:** `--fake` replays `data/cassette.json` by exact
  `(model, messages)` match, else a built-in canned answer (keyless boot). In
  tests the fallback is **off**, so unmatched requests are loud errors.
  `RecordingClient` decorates `dyn LlmClient`, which is why record→replay is
  unit-testable offline. `--fake` and `--record` are mutually exclusive.
- **Error → HTTP mapping** (`agent-web/src/error.rs`): `busy`→409, `config`→400
  (server-side config → 500), `internal`→500, all provider kinds→502 (gateway).
- **Auth comparison** is length-check + XOR fold (not constant-time crypto, but
  non-trivial timing); `auth_token` is normalized (empty ⇒ `None`) at parse time.
- **CLI is hand-rolled** (no clap) in `agent-web/src/main.rs`; options are
  `--config`, `--cassette`, `--fake`, `--record`, `-h/--help`. `Paths` holds
  the defaults and is overridable.
- **Bort design recipe** (Appendix E): box cards use `background:
  base-background-color`, `ring-4` box color, `rounded-sm`, and a hard `8px
  8px` offset shadow; user messages locally re-skin `--box-color-standard` to
  the `hot-pink` accent so ring/shadow/title bar recolor from one variable.

## Testing

- Everything is **offline**: the fake provider replays cassettes/canned
  responses; never add tests that hit the network. `scripts/mock-openai-server.js`
  is for *manual* live-path verification only, not the test suite.
- Prefer unit tests next to the code (`#[cfg(test)] mod tests`); `agent-web`
  route tests drive the axum router via `tower::ServiceExt::oneshot` with an
  in-process `AppState` (dev-deps: `http-body-util`, `tower`).
- Timing-based session tests use generous margins (20–100 ms). If they flake,
  replace the sleep-racing with a manually-stepped fake provider rather than
  loosening timings.
- The `--fake`/test cassette format is versioned (`CASSETTE_VERSION`); fixtures
  are dated and schema-versioned, re-record on adapter changes.

## Runtime data

`data/` holds everything mutable and is the complete backup: `config.toml`
(written 0600, holds the provider key and auth token — never sent to the
browser) and `conversations/{id}.json` (atomic tmp+rename writes; unreadable or
unknown-schema files are **quarantined** aside rather than crashing a turn).
The deployment guide (`docs/deployment.md`) covers systemd + cloudflared +
Cloudflare Access; `WorkingDirectory=` is what places `data/` correctly there.
