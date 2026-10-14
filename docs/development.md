# Development

## Status

Early-stage (v0.1), released under the MIT license (see `LICENSE`).

Verified end-to-end on Linux with PostgreSQL 17 and the real Claude Code CLI and Ollama:

- vault creation, wrong-password rejection, unlock after a wrong attempt, lock/unlock
- account detection, add/edit (form), connection test, usage per account
- streaming chat with Claude Code CLI and Ollama, cancellation with `Esc`
- `@name` routing and `/fanout` with account names containing spaces
- agents, failover to a fallback account, `/orchestrate` (plan → worker → synthesis)
- session persistence and resume after restart
- database inspection: no account name, key, prompt, answer, model or token count appears in plaintext

Not yet verified: the Codex and Gemini CLI adapters (they follow the documented JSONL formats but were never run against the real CLIs), API-key providers against their real endpoints, and non-Linux platforms.

## Requirements

Linux, Rust 1.85+ (edition 2024; developed on 1.95) and PostgreSQL. Optional: Ollama and the `claude` / `codex` / `gemini` CLIs.

## Build and test

```bash
cargo build
cargo test                              # unit tests (crypto, usage, parsers, planner)
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

All unit tests are database-free. The store is exercised by running the app against a scratch database:

```bash
createdb omnicrab_dev
export OMNICRAB_DATABASE_URL=postgres://USER@localhost/omnicrab_dev
cargo run -- doctor
cargo run
```

### Verifying nothing sensitive is stored in plaintext

Add an account whose key is `sk-TESTPLAINTEXT`, then:

```bash
psql "$OMNICRAB_DATABASE_URL" -Atc "select encode(blob,'escape') from accounts" | grep -a TESTPLAINTEXT   # must print nothing
psql "$OMNICRAB_DATABASE_URL" -Atc "select encode(blob,'escape') from messages" | grep -a "<words you typed>"  # must print nothing
```

## Conventions

- All code, comments, docs and UI strings are in English.
- No `unwrap()` on fallible runtime paths (tests excepted); errors reach the user as notices.
- Anything persisted goes through `Vault::seal_json` with a unique `(table, column, row)` AAD.
- Rendering (`tui/ui.rs`) must stay a pure function of `&App`.
- Never write secrets or message content to logs.

## Adding things

| To add | Touch |
| --- | --- |
| Provider | `model.rs::ProviderKind`, `providers/genai_backend.rs` (or a new backend) |
| CLI adapter | `providers/cli_backend.rs` (`build_command`, `parse_line` + a unit test with a sample line) |
| Slash command | `COMMANDS` and `App::command` in `tui/app.rs` |
| Table column | New migration file in `migrations/`, never edit `0001_init.sql` after release |
| Orchestration mode | `orchestrator/modes.rs`; label lanes with `RunEvent::Label` |

## Continuous integration

`.github/workflows/ci.yml` runs fmt, clippy (warnings are errors) and tests on every push and pull request.

## Roadmap ideas

- Side-by-side columns for `/fanout`
- Tool use / MCP for agents
- Cursor movement and history in the message box
- Daemon mode with a JSON-RPC socket
- Optional OS keyring caching of the unlocked key
