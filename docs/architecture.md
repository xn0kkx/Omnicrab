# Architecture

omnicrab is a single Rust binary. The UI never calls providers directly: it talks to a core through channels, which keeps the door open for a future daemon/RPC split (the approach Hermes takes).

```
 ┌────────────── tui ──────────────┐
 │ app.rs  state + key handling    │   Msg / (LaneId, RunEvent)
 │ ui.rs   pure render(&App)       │◄────────────────────────────┐
 └──────────────┬──────────────────┘                             │
                │ spawn                                          │
        ┌───────▼────────┐  run()   ┌────────────────────┐       │
        │ orchestrator   ├─────────►│ providers          │       │
        │ failover,budget│ stream   │  genai_backend (HTTP)      │
        │ fan-out, plan  │◄─────────┤  cli_backend (subprocess)  │
        └───────┬────────┘ events   │  ollama_discovery  │       │
                │ usage/messages    └────────────────────┘       │
        ┌───────▼────────┐ seal/open ┌─────────┐                  │
        │ store (sqlx)   ├──────────►│ crypto  │                  │
        └───────┬────────┘           └─────────┘                  │
                ▼                                                 │
           PostgreSQL (ciphertext only) ──────────────────────────┘
```

## Modules

| Path | Responsibility |
| --- | --- |
| `src/main.rs` | CLI (`clap`), logging to file, `doctor`, `init-db`, starts the TUI |
| `src/config.rs` | Non-sensitive config, database URL resolution |
| `src/model.rs` | Plaintext models (`Account`, `Agent`, `Message`, `UsageEvent`, `ProviderKind`) |
| `src/crypto/cipher.rs` | XChaCha20-Poly1305 seal/open with AAD and format version |
| `src/crypto/vault.rs` | Argon2id → KEK, wrapped DEK, JSON sealing, blind index |
| `src/store/mod.rs` | PostgreSQL access; every payload column goes through the vault |
| `src/providers/` | `stream_chat`, `complete`, `test_connection`; HTTP and CLI backends |
| `src/orchestrator/mod.rs` | `Core::run`: failover chain, budgets, usage recording |
| `src/orchestrator/modes.rs` | Fan-out and plan → workers → synthesis |
| `src/usage.rs` | Price table, time windows, aggregation, latest plan-limit snapshot per account |
| `src/tui/` | `app` (state/events), `ui` (rendering), `form` (modal forms), `crab` (chat mascot animation) |

## Event flow for one chat message

1. `App::submit` resolves the target (default or `@name`) into an ordered account chain and builds a `ChatRequest` from the visible history.
2. `spawn_run` creates the session if needed, stores the user message, then runs the work in a tokio task with a `CancellationToken`.
3. `Core::run` iterates the chain. For each account it checks enabled/budget, streams from the provider, forwards `RunEvent`s on the UI channel and records a `UsageEvent`.
4. On a failover-class error with no text yet shown, it moves to the next account; otherwise it finishes or reports the failure.
5. The finished answer is stored as an encrypted message; `Msg::RunDone` triggers a reload of accounts, agents, sessions and usage.

A **lane** is one logical request. Chat uses one lane; `/fanout` one per account; `/orchestrate` a block of lanes (planner, workers, synthesis). The UI creates one chat entry per lane on its first event.

## Concurrency

- The UI loop is a single `tokio::select!` over terminal events, a 150 ms tick, `Msg` and lane events.
- Providers stream through bounded `mpsc` channels; cancellation drops the stream or kills the CLI child (`kill_on_drop`).
- Key derivation runs in `spawn_blocking` so the UI stays responsive.

## Database schema

`migrations/0001_init.sql`. Everything in `blob` columns is ciphertext.

| Table | Plaintext columns | Encrypted `blob` content |
| --- | --- | --- |
| `vault_meta` | `kdf_salt`, `kdf_params`, `wrapped_dek` | – |
| `accounts` | `id`, `name_idx` (HMAC), `created_at` | name, provider, API key, URL, model, CLI path, budget, enabled |
| `agents` | `id`, `created_at` | name, role, prompt, account ids, model |
| `sessions` | `id`, `created_at` | title |
| `messages` | `id`, `session_id`, `seq`, `created_at` | role, content, agent/account names |
| `usage_events` | `id`, `account_id`, `created_at` | model, tokens, cost, latency, status |

Usage is aggregated in memory after decryption, which is fine at personal scale.

`usage_events` payloads may also carry `rate_limits` (5-hour / weekly utilization and reset times). It lives inside the encrypted blob, so it needs no migration; older events simply lack it. The value travels from the CLI parser through `TokenUsage.rate_limits` to `Core::record_model`.
