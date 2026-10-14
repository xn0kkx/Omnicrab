# 🦀 omnicrab

A terminal UI written in Rust that connects **many AI accounts and local models** and **orchestrates agents across them**. Think Claude Code / Hermes, but provider-agnostic, with per-account usage tracking and every byte of stored data encrypted at the application layer before it reaches PostgreSQL.

## Features

- **Many providers, many accounts** – Anthropic, OpenAI, Gemini, OpenRouter, Groq, DeepSeek, xAI, Mistral via API key; Ollama and any OpenAI-compatible server (llama.cpp, LM Studio, vLLM) locally.
- **Subscriptions through official CLIs** – `claude`, `codex` and `gemini` run in headless mode, so subscription logins are never extracted or reused.
- **Named accounts** – give each account its own name ("Claude Work", "OpenAI Personal") and rename it any time.
- **Usage dashboard** – requests, tokens in/out and cost per account for today / 7d / 30d / all time, a 30-day token sparkline, per-account monthly budget gauge and a cost bar chart.
- **Agent orchestration** – named agents with roles, system prompts, a primary account and ordered fallbacks; `@agent` routing; `/fanout` to compare accounts side by side; `/orchestrate` for plan → parallel workers → synthesis.
- **Failover and budgets** – rate limits, auth errors, 5xx and exhausted monthly budgets move the request to the next account.
- **Application-layer encryption** – Argon2id → key-encryption key → data key → XChaCha20-Poly1305 per record, with AAD bound to table, column and row. See [docs/security.md](docs/security.md).
- **Persistent sessions** stored (encrypted) in PostgreSQL.

## Requirements

- Linux, Rust 1.85+ (edition 2024; developed on 1.95)
- PostgreSQL (any recent version)
- Optional: [Ollama](https://ollama.com), and the `claude` / `codex` / `gemini` CLIs

## Quick start

```bash
# 1. Create a database (once)
createdb omnicrab

# 2. Point omnicrab at it. Password via ~/.pgpass or PGPASSWORD.
export OMNICRAB_DATABASE_URL=postgres://USER@localhost/omnicrab

# 3. Build and check your environment
cargo build --release
./target/release/omnicrab doctor

# 4. Run
./target/release/omnicrab
```

On first launch you choose a **master password**. It is never stored and **cannot be recovered**: losing it means losing the data.

Then press `Tab` for the Accounts tab and `a` to add an account, or `D` to auto-detect Ollama and installed CLIs.

You can also set `database_url` in `~/.config/omnicrab/config.toml`; the environment variable takes precedence.

## Keys and commands

| Key | Action |
| --- | --- |
| `F1`–`F5` / `Tab` | Switch tabs (Chat, Accounts, Agents, Usage, Sessions) |
| `Enter` / `Alt+Enter` | Send / newline in the message box |
| `Esc` / `Ctrl+C` | Cancel running requests (`Ctrl+C` quits when idle) |
| `Ctrl+N` / `Ctrl+L` | New session / lock the vault |
| `PgUp` / `PgDn` | Scroll chat |

Slash commands: `/help /new /sessions /usage /accounts /agents /use /model /fanout /orchestrate /rename /passwd /lock /clear /quit`. Full reference in [docs/usage.md](docs/usage.md).

## Documentation

- [Usage guide](docs/usage.md) – TUI tour, accounts, agents, orchestration
- [Providers](docs/providers.md) – supported providers, CLI integration, local models, pricing
- [Architecture](docs/architecture.md) – modules, data flow, database schema
- [Security](docs/security.md) – threat model and encryption design
- [Development](docs/development.md) – building, testing, contributing

## Status

Early-stage (v0.1). Unit tests cover encryption, usage aggregation, CLI output parsing and plan parsing. The PostgreSQL store, the TUI and the CLI/HTTP provider adapters have not yet been verified end-to-end; expect rough edges and please report issues.

## License

MIT
