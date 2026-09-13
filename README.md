# Kaeru

A personal, single-user agent: a platform-agnostic **Rust core** (`agent-core`)
with thin frontends on top. The first frontend is **agent-web**, a localhost
web UI that streams from any OpenAI-compatible provider (OpenRouter, Ollama,
LM Studio, vLLM, llama.cpp).

```sh
cargo run -p agent-web -- --fake   # keyless demo UI on http://127.0.0.1:8080
cargo run -p agent-web             # live, using data/config.toml
```

Everything is configured in `data/config.toml`, created on first run:

```toml
auth_token = ""                              # set before exposing via a tunnel

[provider]
base_url = "https://openrouter.ai/api/v1"    # any OpenAI-compatible endpoint
api_key  = "sk-or-v1-..."                    # empty for local servers
model    = "openai/gpt-4o-mini"
```

The server binds `127.0.0.1` only. The Python tool additionally needs Linux
with `bwrap` and `uv` on `PATH`; without them the server refuses to start.

## License

MIT — see [LICENSE](LICENSE).