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
        │ balance, plan  │◄─────────┤  cli_backend (subprocess)  │
        │ compact, fanout│          │  catalog (model lists)     │
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
| `src/orchestrator/modes.rs` | Fan-out; `plan`, `execute`, `synthesize` (and `orchestrate` = all three) |
| `src/orchestrator/balance.rs` | Pure ranking of role slots by free capacity; spreads parallel steps |
| `src/orchestrator/compact.rs` | History-size estimate, cut point, summary prompt (pure) |
| `src/providers/catalog.rs` | Lists the models of an account for the pickers |
| `src/usage.rs` | Price table, time windows, aggregation, latest plan-limit snapshot per account |
| `src/orchestrator/questions.rs` | Questions a model can ask (planner JSON, chat `omni-questions` block): parsing and limits |
| `src/project/` | Workspace root choice and `.omni/` (sealed documents, plans, sessions, project agents/commands) |
| `src/tui/` | `app` (state/events, plan review, routing keys; `app/omni.rs` for `.omni`), `ui` (rendering), `form` (modal forms), `questions` (question picker state), `crab` (chat mascot animation) |

## Event flow for one chat message

1. `App::submit` resolves the target (default or `@name`) into an ordered account chain and builds a `ChatRequest` from the visible history.
2. `spawn_run` creates the session if needed, stores the user message, then runs the work in a tokio task with a `CancellationToken`.
3. `Core::run` iterates the chain. For each account it checks enabled/budget, streams from the provider, forwards `RunEvent`s on the UI channel and records a `UsageEvent`.
4. On a failover-class error with no text yet shown, it moves to the next account; otherwise it finishes or reports the failure.
5. The finished answer is stored as an encrypted message; `Msg::RunDone` triggers a reload of accounts, agents, sessions and usage.

### Plan and auto mode: the lead loop

`App::start_lead` builds a `modes::Team` (`lead` chain: the `/model` override on the active target, else the Planner slots; `scouts` = Auxiliary slots; `pool` = executors; project context; workspace) and runs `lead::run` in one task. The task talks to the UI through `Msg::Lead` and oneshot channels: `LeadMsg::Ask` opens `Modal::Questions`, `LeadMsg::Review` opens the plan review, and the loop waits for the answer (or for cancellation).

`lead::run`: `scout_phase` (overview digest cached per `HEAD` in `.omni/digest.enc`, plus a task-specific scout) → up to 8 turns of `ask_lead` (`ChatRequest.json_schema`, `parse_turn`, one repair retry) → for `plan`/`delegate`, `modes::execute` runs the steps (sequential when any step writes, otherwise parallel) and `project::git_changes` reports what changed; the reports go into the log the next turn sees (older entries are trimmed first). `finish` emits `RunEvent::Final`. `/orchestrate` still uses `modes::orchestrate` (plan → execute → synthesize).

Each lane is announced with `RunEvent::Queued`, then `Started`, tool `Note`s, and `Finished`/`Failed`. `RunEvent::Reset` clears a lane's text when a provider restarts or fails after partial output (requests with `restart_on_partial`). `Core::run` has a watchdog (`Limits`: warn, then stop and fail over when a run stays silent) and logs each run with `tracing`. Structured (`json_schema`) answers are not streamed to the chat. Requests carry an optional `Workspace { root, access }`; `cli_backend::build_command` maps it to each CLI's flags and working directory. The Claude CLI returns schema-constrained answers as a `StructuredOutput` tool call and agy in `structured_output`; the parser turns both into the answer text.

### History compaction

Before a chat request, if the estimated history exceeds `BalancePolicy::history_budget_tokens` and an Auxiliary slot exists, `compact_history` (in `tui/app.rs`) asks the auxiliary chain to update a running summary of turns `1..=cut`. The request then carries turn 0, the summary and the turns after `cut`. The summary lives in `App::summary` and is reused until 4 more turns pile up.

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
| `settings` (`0002`) | `key` (`routing`), `id`, `updated_at` | the `Routing` document: role slots (account ids, models, effort) and balance policy |

Usage is aggregated in memory after decryption, which is fine at personal scale.

`usage_events` payloads may also carry `rate_limits` (5-hour / weekly utilization and reset times). It lives inside the encrypted blob, so it needs no migration; older events simply lack it. The value travels from the CLI parser through `TokenUsage.rate_limits` to `Core::record_model`.
