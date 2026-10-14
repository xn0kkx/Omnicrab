# Usage guide

## First run

1. Start `omnicrab`. If no vault exists you are asked for a master password (min. 8 characters) and a confirmation.
2. Argon2id derives a key from the password (about 0.2 s) and the vault is created in PostgreSQL.
3. On later runs you only enter the password. A wrong password is rejected without any data being decrypted.

`Ctrl+L` or `/lock` locks the vault: the key is dropped from memory and the unlock screen returns.

## Tabs

| Tab | Purpose |
| --- | --- |
| **Chat** | Conversation with the active account or agent, with live streaming. |
| **Accounts** | Add, rename, edit, test, enable/disable and delete accounts. |
| **Agents** | Define named agents with role, system prompt, account and fallbacks. |
| **Usage** | Per-account consumption and costs. |
| **Sessions** | Resume or delete saved conversations. |

Switching tabs (works from anywhere, including the chat box; `Alt` is deliberately not used because terminals use it for their own tabs):

| Key | Action |
| --- | --- |
| `Ctrl+P` | Quick switcher: press `c` Chat, `a` Accounts, `g` Agents, `u` Usage, `s` Sessions (or `1`–`5`, or `↑/↓` + `Enter`). `Esc` closes it. |
| `Ctrl+←` / `Ctrl+→` | Previous / next tab |
| Mouse click | Click a tab title in the top bar |
| `F1`–`F5` | Jump to a tab |
| `Tab` / `Shift+Tab` | Next / previous tab in list tabs (in Chat, `Tab` completes `/` and `@` first) |
| `1`–`5` | Jump to a tab from list tabs |

Lists share navigation keys: `↑/↓` or `j/k` to move, `Esc` to return to Chat, `?` for help.

### Accounts tab

| Key | Action |
| --- | --- |
| `a` | Add account |
| `e` / `r` | Edit or rename the selected account |
| `t` | Test connection (sends a tiny prompt) |
| `Space` | Enable/disable |
| `Enter` / `u` | Use as the chat target (★) |
| `d` | Delete (with confirmation) |
| `L` | Sign in the selected CLI account (runs the CLI's own login for its **Profile dir**) |
| `D` | Detect installed CLIs (`claude`, `agy`, `codex`, `gemini`) and add them |

The add/edit form only shows the fields relevant to the chosen provider. `Ctrl+T` tests the connection from inside the form. When editing, leave **API key** empty to keep the stored one; the current key is shown masked (`sk-…abcd`).

Account names are unique (case-insensitive). The optional **Budget USD/mo** makes the orchestrator skip the account once the month's spend reaches the limit.

### Agents tab

An agent has:

- **Name** – used for `@name` routing (unique).
- **Role** and **System prompt** – combined into the system message.
- **Account** and optional **Model** override.
- **Fallbacks** – account names, comma separated, tried in order on failover.

### Usage tab

- **Plan limits** panel: for each Claude Code CLI account, how much of the **5-hour** and **weekly** subscription limits is used (bar, percentage, time until the window resets) and how old that reading is. Accounts of other kinds show `n/a`. See [how the numbers are obtained](providers.md#plan-limits-5-hour-and-weekly). `p` sends a tiny prompt to the selected Claude account to refresh its reading.
- Table: requests, errors, input/output tokens and cost for the selected window. `t` cycles *today → 7d → 30d → all*; `r` refreshes.
- Sparkline: tokens per day for the last 30 days for the selected account.
- Gauge: month spend against the account's budget (green < 70 %, yellow < 90 %, red above).
- Bar chart: cost per account for the window.

Cost is computed from the [price table](providers.md#pricing); CLI accounts use the cost the CLI reports; local models cost 0.

## Chat

- `Enter` sends, `Alt+Enter` (or `Ctrl+J`) inserts a newline, `Esc` clears the box or cancels running requests. `Ctrl+H` works as Backspace for terminals that send `^H`. The mouse wheel and `PageUp`/`PageDown` scroll the chat; hold `Shift` while dragging to select text (mouse capture is on).
- Typing `/` or `@` opens a completion popup; `Tab` completes.
- A small crab sits above the message box. While a request is running it scuttles from side to side and snaps its claws; when idle it rests (and blinks). It is hidden on very short terminals (< 18 rows).
- The status bar shows the active target, model override, session tokens/cost and a spinner while requests run.

### Routing

- Default target: the account/agent chosen with `/use` or `Enter` in the Accounts/Agents tab.
- `@name message` sends a single message to that agent or account without changing the default.

### Commands

| Command | Description |
| --- | --- |
| `/help` | Show keys and commands |
| `/new` | Start a new session |
| `/sessions`, `/usage`, `/accounts`, `/agents` | Jump to a tab |
| `/use <name>` | Choose the active agent or account |
| `/model <name>` | Override the model for subsequent requests; empty resets |
| `/fanout a,b,c <prompt>` | Send one prompt to several accounts in parallel (names comma separated, no spaces) |
| `/orchestrate <task>` | Plan → parallel agents → synthesis (see below) |
| `/rename <title>` | Rename the current session |
| `/passwd` | Change the master password |
| `/lock` | Lock the vault |
| `/clear` | Clear the screen (session is kept) |
| `/quit` | Exit |

### Orchestration

`/orchestrate <task>` uses the active target as **planner and synthesizer**:

1. The planner receives the agent roster and returns a JSON list `[{"agent": "...", "task": "..."}]`.
2. Each step runs in parallel on the named agent's account chain (unknown names run on the planner's account).
3. The planner receives all results and writes the final answer.

Each stage appears as its own message labelled `[planner]`, `[@agent]` or `[synthesis]`. If the plan cannot be parsed, the task runs as a single step. Create agents first (Agents tab) to get meaningful delegation.

### Failover

If an account fails with an auth, rate-limit, quota, 5xx or network error **before any text was streamed**, the next account in the chain is tried and a `failover:` note is shown. Errors after streaming has started are reported, not retried. Every attempt is recorded in the usage history.

## Configuration

`~/.config/omnicrab/config.toml` (non-sensitive only):

```toml
database_url = "postgres://user@localhost/omnicrab"

# USD per 1M tokens, matched by model-name prefix
[prices."my-model"]
input = 1.0
output = 3.0
```

Logs are written to `~/.local/state/omnicrab/omnicrab.log.*` (never to the terminal). Set `RUST_LOG=omnicrab=debug` for more detail. Logs never contain keys or message content.

## CLI

```
omnicrab            # start the TUI
omnicrab doctor     # check database, Ollama and installed CLIs
omnicrab init-db    # run migrations only
```
