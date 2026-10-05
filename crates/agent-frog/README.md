# agent-frog 🐸

The Kaeru Wayland desktop frog (M6, ADR-031) — a small, always-on-top widget for
quickly asking the daemon something. It is a **thin client**: it speaks the same
`/api/*` wire API as the web UI (`POST /api/chat` SSE + `/api/approval` +
`/api/abort` + `/api/threads`), so it needs no core and no `data/` of its own.

```
collapsed:  🐸            (a little square you can drag)
expanded:   ┌ Kaeru  ↻ 🎨 ➖ ┐
            │ messages…      │
            │ [ask…] [Ponder]│
            └─────── 🐸 ─────┘
```

## Requirements

- Linux with a Wayland compositor that supports **wlr-layer-shell** (niri,
  Sway, Hyprland, river, …). On a compositor without it the frog falls back to
  a normal undecorated window (`--no-layer-shell` forces that).
- GTK 4 and `gtk4-layer-shell` on the host (Arch: `gtk4 gtk4-layer-shell`;
  Debian/Ubuntu ≥ 25.10: `libgtk-4-dev libgtk4-layer-shell-dev`).
- A running `agent-daemon` to talk to.

## Build & run

```sh
# from the repo root; the GUI crate is a workspace member but not a default
# member, so build it explicitly
cargo build -p agent-frog

# localhost daemon (the default)
cargo run -p agent-frog

# keyless demo against a fake daemon
cargo run -p agent-daemon -- --fake   # in one terminal
cargo run -p agent-frog               # in another
```

The frog is a *client*: it connects to whatever the daemon exposes. Run the
daemon with `--fake` to try the UI without a provider key.

## Configuration

`~/.config/kaeru/frog.toml` (written `0600`; it holds the auth token):

```toml
url    = "http://127.0.0.1:8080"   # or http://100.x.y.z:8080 on the tailnet
token  = ""                        # X-Auth-Token; required past localhost
theme  = "latenightbath"
corner = "bottom-right"            # bottom-right|bottom-left|top-right|top-left
thread = ""                        # empty = newest thread, created on demand
avatar = "🐸"

[sprite]                           # drop real art in later:
frames = []                        # e.g. ["idle.png", "blink.png"]
fps    = 4

[position]                         # persisted by dragging
h = 18
v = 18
```

CLI flags override the file: `--url`, `--token`, `--thread`, `--theme`,
`--corner`, `--config`, `--expanded`, `--no-layer-shell`, `-h`.

Unknown keys are hard errors (same `deny_unknown_fields` discipline as the
daemon config). A `401` shows an in-widget token prompt; saving it writes the
config.

## Tailnet use (M6, ADR-030)

On the daemon host, bind a tailnet address (and set `auth_token`):

```toml
# data/config.toml
auth_token = "<long random>"
[daemon]
bind = "100.x.y.z:8080"   # `tailscale ip -4`
```

Then point the frog at it from any tailnet device:

```sh
agent-frog --url http://100.x.y.z:8080 --token "<long random>"
```

The daemon refuses anything but loopback or a tailnet address, and refuses a
non-loopback bind without a token — so this is private transport plus the same
owner-only guard the web API uses. See `Kaeru-docs/deployment.md` §7a.

## Interaction

- **Click** the frog to open/close the panel.
- **Hold and drag** the frog to move it; the position is saved to `[position]`.
- **Ponder** (or Enter) sends; **stop** aborts the turn (`/api/abort`).
- Consent cards (memory writes, package installs, network, TODO writes) render
  inline with **allow** / **deny**.
- **🎨** cycles the Bort themes; **➖** collapses; **↻** reconnects.

The assistant's streamed text is shown as plain text (the web UI re-renders
Markdown on reload via the server; the frog deliberately stays minimal).

## Design notes

- The eight Bort themes are copied by hand from `agent-server/assets/app.css`
  (C17, ADR-022) into `src/theme.rs`, which generates a GTK4 stylesheet per
  theme. GTK has no CSS custom properties, hence the regeneration.
- The two Bort fonts are embedded from the web assets and registered with
  fontconfig at startup (GTK4 CSS has no `@font-face`).
- `CoreEvent`s are mirrored as a small serde enum in `src/client.rs` (the web
  client mirrors them in JS) — the frog never depends on `agent-core` (C2).
- New dependencies (GTK4, gtk4-layer-shell, async-channel, fontconfig via FFI)
  are recorded in **ADR-031**; the crate is a workspace member but excluded
  from `default-members`, so `cargo test`/`cargo build` stay headless.
