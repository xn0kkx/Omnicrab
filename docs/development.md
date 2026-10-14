# Development

## Status

Early-stage (v0.1), released under the MIT license (see `LICENSE`).

Verified end-to-end on Linux with PostgreSQL 17 and the real Claude Code CLI, Antigravity (`agy`) CLI and Ollama:

- vault creation, wrong-password rejection, unlock after a wrong attempt, lock/unlock
- account detection, add/edit (form), connection test, usage per account
- streaming chat with Claude Code CLI, agy and Ollama, cancellation with `Esc`
- `@name` routing and `/fanout` with account names containing spaces
- agents, failover to a fallback account, `/orchestrate` (plan → worker → synthesis)
- session persistence and resume after restart
- database inspection: no account name, key, prompt, answer, model or token count appears in plaintext

Plan mode, routing and balancing (unit-tested for ranking, plan parsing, history cut points and model-list parsing) were also exercised against the real Claude Code and `agy` CLIs on a scratch database: role slots persist across restarts and are encrypted in `settings`, the model picker lists `agy models` and the Claude aliases, a plan with two steps was reviewed, edited by removing a step and executed on the pool, and a single-step plan skipped synthesis.

The lead loop (scouts, JSON turns, question picker, plan review, workers editing files, `git` change report, `.omni`) was driven end to end in tmux against a throwaway PostgreSQL cluster and a git repo: first with stub `claude` binaries (valid turns, prose replies that need the repair retry, a hung CLI stopped by the watchdog), then with the real Claude CLI on `haiku` (two worker steps wrote `hello.txt` and `notes.md`; about $0.01). Not yet run end to end: `agy` as lead or worker with `--json-schema`, `/omni edit` in a real editor, and read-only (`--read-only`) runs.

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
| Setting stored in the database | `Store::save_setting` / `load_setting` with a new key (no migration needed) |
| Balancing rule | `orchestrator/balance.rs` (`headroom`, `rank_with_load`) with a unit test |

## Continuous integration

`.github/workflows/ci.yml` runs fmt, clippy (warnings are errors) and tests on every push and pull request.

## Roadmap ideas

- Side-by-side columns for `/fanout`
- Tool use / MCP for agents
- Cursor movement and history in the message box
- Daemon mode with a JSON-RPC socket
- Optional OS keyring caching of the unlocked key
- Persist the history summary with the session
- Per-slot daily token caps in the balancer
