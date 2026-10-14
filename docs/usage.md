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
| **Routing** | Which accounts plan, execute, merge and summarize, and how load is balanced. |

Switching tabs (works from anywhere, including the chat box; `Alt` is deliberately not used because terminals use it for their own tabs):

| Key | Action |
| --- | --- |
| `Ctrl+P` | Quick switcher: press `c` Chat, `a` Accounts, `g` Agents, `u` Usage, `s` Sessions, `r` Routing (or `1`–`6`, or `↑/↓` + `Enter`). `Esc` closes it. |
| `Ctrl+←` / `Ctrl+→` | Previous / next tab |
| Mouse click | Click a tab title in the top bar |
| `F1`–`F6` | Jump to a tab |
| `Tab` / `Shift+Tab` | Next / previous tab in list tabs. In Chat, `Tab` completes `/` and `@` first, and `Shift+Tab` cycles the mode |
| `1`–`6` | Jump to a tab from list tabs |

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
| `m` | Refresh the list of models this account offers (see [model picker](#choosing-models)) |

The add/edit form only shows the fields relevant to the chosen provider. `Ctrl+T` tests the connection from inside the form. When editing, leave **API key** empty to keep the stored one; the current key is shown masked (`sk-…abcd`).

Account names are unique (case-insensitive). The optional **Budget USD/mo** makes the orchestrator skip the account once the month's spend reaches the limit.

### Agents tab

An agent has:

- **Name** – used for `@name` routing (unique).
- **Role** and **System prompt** – combined into the system message.
- **Account** and optional **Model** override.
- **Fallbacks** – account names, comma separated, tried in order on failover.

### Choosing models

omnicrab asks every enabled account which models it offers when you unlock the vault (and again with `m` in the Accounts tab):

| Account | Source |
| --- | --- |
| Ollama / OpenAI-compatible | the server's own model list |
| API providers | the provider's model-list endpoint |
| Antigravity (`agy`) | `agy models` |
| Claude Code, Codex, Gemini CLI | a built-in list (the CLIs cannot list models); you can always type any name |

The lists feed one filterable **picker**: type to filter, `↑/↓`, `Enter` to choose, `Esc` to cancel. If nothing matches, `Enter` uses what you typed.

- `/model` opens the picker (`account › model`) for the chat; `/model <name>` sets it directly; `/model reset` clears it.
- In the account and agent forms, `Ctrl+O` on the **Model** field opens the picker.
- In the Routing tab, `m` opens it for the selected slot.

### Routing tab

Roles are stored encrypted in the database like everything else.

| Section | Purpose |
| --- | --- |
| **Planner** | The lead of plan and auto mode. Use a strong model. Without slots here the active target leads; a model chosen with `/model` always wins. |
| **Executors** | Pool for steps that do not name an agent. |
| **Synthesizer** | Merges the results of 2+ steps. Empty means the planner does it. |
| **Auxiliary** | Cheap helpers: scouts that read the project and docs, and summaries of old messages. A local or low-cost model is ideal. |
| **Policy** | Balancing and history limits (below). |

Each slot is an account plus an optional **model** and, for Claude Code accounts, an **effort** (`--effort low|medium|high|xhigh|max`). The bar shows how much of the account is still free.

| Key | Action |
| --- | --- |
| `a` | Add an account to the selected section |
| `m` | Choose the slot's model |
| `f` | Cycle effort (Claude Code accounts) |
| `Space` | Enable/disable the slot (on a policy row: adjust it) |
| `J` / `K` | Move the slot down / up |
| `d` | Remove the slot |
| `+` / `-` | Change the selected policy value |

**Balancing.** Slots of a role are ordered by free capacity before every run:

- local models count as fully free (and go first with *prefer local models*);
- Claude Code accounts use `1 − max(5-hour use, weekly use)` from the plan limits shown in the Usage tab;
- API accounts with a monthly budget use the budget left;
- others have unknown capacity and rank in the middle, fewest requests today first;
- accounts above *skip accounts above* (default 85 %) go to the end of the chain but stay available as failover; accounts over their monthly budget are dropped;
- parallel steps are spread so they do not all start on the same account;
- the planner stays on the same account while it has room, which keeps the provider's prompt cache warm.

Failover works as before on the ordered chain.

### Modes

`Shift+Tab` in the Chat tab cycles the mode, as in Claude Code: **chat → plan → auto**. The status bar shows `PLAN` or `AUTO` (nothing in chat mode). `/plan` or `Ctrl+O` toggles plan mode directly.

| Mode | A plain message… |
| --- | --- |
| chat | goes straight to the active target |
| plan | is planned, shown for your review, then executed |
| auto | is planned and executed right away, without review (same as `/orchestrate`) |

`@name message` always bypasses the mode.

### Plan mode

Plan mode (and auto mode, without the review) runs a **lead loop**, in the style of Hermes Agent: cheap models gather context, the model you picked plans and orchestrates, cheap agents do the work.

1. **Scouts** (cheap, read-only, in parallel) read the project and its docs first: one project overview (cached in `.omni` until the git commit changes) and one look at what matters for your task. They run on the **Auxiliary** slots, or the executor pool when there are none. Without a project folder this step is skipped.
2. The **lead** is the model you picked with `/use` + `/model` (or the Planner slots when you picked none). It has no tools and answers with one JSON object per turn (enforced with `--json-schema` for Claude and agy; others get one automatic repair retry). It can **ask** you (the question picker), **plan**, **delegate**, **scout** more, **remember** a fact in `.omni`, or **finish**.
3. A **review** window opens for the first plan: `Enter` approves, `e` edits a step, `d` removes one, `r` asks the lead to revise, `Esc` discards. Each step says whether it may write or only read.
4. Approved steps run on the balanced executors: steps that write run one after another, read-only ones in parallel. When they end the lead gets their reports **and what really changed on disk** (`git status` + `git diff --stat`, shown as a `changes` entry), then decides: delegate again, fix something, or finish. It has at most 8 turns.
5. The lead's `finish` message is the answer.

Typical setup: put a strong model in **Planner** (or `/model` it), cheap models in **Executors** and **Auxiliary**.

`@name message` still goes straight to that agent or account. `/orchestrate <task>` keeps the older fixed pipeline (plan → parallel steps → synthesis) without review.

**If it seems stuck.** A lane with no output for 90 s shows `no output for 90s (still running)`; after 10 minutes the process is stopped and the next account (or an error) takes over. Change both with `OMNICRAB_IDLE_WARN_SECS` and `OMNICRAB_IDLE_KILL_SECS`. `RUST_LOG=omnicrab=debug omnicrab` logs every run (account, model, duration, result, CLI stderr) to `~/.local/state/omnicrab/omnicrab.log.*`.

**Watching a run.** Every lane (planner, each step, synthesis) has its own entry with a state (`queued`, `running 12s`, `done 31s`, `failed`), the account that is answering, and its tool activity on separate `↳` lines (files read or edited, shell commands, web searches; the last six are shown). A lane that has not produced text yet shows `thinking…`. The status bar counts running and queued lanes and the elapsed time. If a provider fails after it already streamed part of a planner, step or synthesis answer, the partial text is dropped (`restart:` note) and the next account in the chain tries again. If every synthesizer fails, you get the raw step results instead of nothing.

**Questions.** When the planner (or, in chat, the model) needs a decision it cannot make itself, a **Questions** window opens, like Claude Code's:

| Key | Action |
| --- | --- |
| `↑`/`↓`, `j`/`k` | Move the highlight (`1`–`9` mark that option) |
| `Enter` | Confirm the question (the highlighted row counts if nothing is marked) and go to the next one; submits after the last |
| `Space` | Mark the highlighted option (toggle in multiple choice); never closes the window |
| `←`/`→`, `Tab` | Switch question |
| last row **Other** | Type your own answer, `Enter` to confirm |
| `Esc` | Skip (the planner then plans without answers) |

Your answers are shown in the chat and sent back: the planner plans again with them; in chat they are your next message.

### Project folder and access

omnicrab starts CLI agents (Claude Code, Codex, Gemini, `agy`) **inside the folder it was opened in**, so they see your code. Executors may read and edit files and run commands there; the planner and plain chat can only read. The status bar shows `rw <folder>` or `ro <folder>`.

- `omnicrab --workspace <dir>` uses another folder; `--read-only` (or `"read_only": true` in the project settings) stops all edits.
- `/` and your home directory are refused unless you pass them with `--workspace`.
- Steps that may edit files run **one after another** (each gets a short summary of the earlier results) so they do not overwrite each other. Read-only runs stay parallel.
- API-key and Ollama accounts have no file access; they only get the text of their task.

### The `.omni` folder

On unlock omnicrab creates `<project>/.omni/`, its equivalent of `.claude/` / `.gemini/`. Everything in it is **encrypted with the vault key** (only `project.toml`, a random project id, is plain), so it is safe in a shared checkout but unreadable without your master password. `.omni/.gitignore` ignores it.

| Item | Purpose |
| --- | --- |
| `instructions` | Project instructions (like `CLAUDE.md`), added to every prompt |
| `memory` | Notes that persist between runs, added to every prompt |
| `settings` | `{"read_only": true}` |
| `agents` | JSON list of agents available only in this project (accounts by name) |
| `commands` | JSON list of custom commands: `{"name","description","prompt"}`; `$ARGUMENTS` is replaced by what follows `/name` |
| `plans/`, `sessions/` | Every approved plan with its result, and the transcript of each session (written automatically) |

| Command | Action |
| --- | --- |
| `/omni` | Status: what exists, counts |
| `/omni init` | Import `CLAUDE.md`, `GEMINI.md`, `AGENTS.md` into the instructions |
| `/omni edit [doc]` | Decrypt one document to a private temp file, open `$VISUAL`/`$EDITOR`, re-encrypt on save and delete the copy (JSON documents are validated; a rejected text is kept in `/tmp/omnicrab-<doc>.rejected`) |
| `/omni remember <note>` | Append a line to the memory |

The CLI agents cannot read the encrypted files; omnicrab decrypts instructions and memory and passes them in the prompt. Claude Code is also told not to edit `.omni/`.

### Saving tokens

- **Plan with a strong model, execute with cheap ones.** Put e.g. Claude `opus` in Planner and `sonnet`/`haiku` with low effort, flash models or a local model in Executors.
- **History summaries.** When the conversation grows past *summarize history above* (default ~24 000 tokens, estimated as characters ÷ 4), the Auxiliary account(s) fold older messages into a running summary. The first message and the last *keep last turns* messages stay verbatim. The summary is updated incrementally and kept in memory for the session (it is not stored). This matters most for the CLI accounts, which are single-shot and would otherwise receive the whole history on every message. Without an Auxiliary slot nothing is summarized.
- **`/compact [focus]`** does the same on demand, like Claude Code: everything between the first message and the last *keep last turns* messages is folded into a summary, which replaces those messages in every later request. The conversation stays on screen and a `compact` entry shows the summary. The optional text tells the summarizer what to keep (`/compact keep the database schema decisions`). It uses the Auxiliary account(s), or the active target when none is configured, and can be repeated: each run updates the previous summary.
- **No synthesis for one step**, and each step's result is cut at 8 000 characters before synthesis.
- Executors only see their own task, not the conversation.

### Usage tab

- **Plan limits** panel: for each Claude Code CLI account, how much of the **5-hour** and **weekly** subscription limits is used (bar, percentage, time until the window resets) and how old that reading is. Accounts of other kinds show `n/a`. See [how the numbers are obtained](providers.md#plan-limits-5-hour-and-weekly). `p` sends a tiny prompt to the selected Claude account to refresh its reading.
- Table: requests, errors, input/output tokens and cost for the selected window. `t` cycles *today → 7d → 30d → all*; `r` refreshes.
- Sparkline: tokens per day for the last 30 days for the selected account.
- Gauge: month spend against the account's budget (green < 70 %, yellow < 90 %, red above).
- Bar chart: cost per account for the window.

Cost is computed from the [price table](providers.md#pricing); CLI accounts use the cost the CLI reports; local models cost 0.

## Chat

- `Enter` sends, `Alt+Enter` (or `Ctrl+J`) inserts a newline, `Esc` clears the box or cancels running requests. `Ctrl+H` works as Backspace for terminals that send `^H`.
- The message box edits like a readline prompt (and like Claude Code):

  | Key | Action |
  | --- | --- |
  | `←` / `→` | Move the cursor |
  | `↑` / `↓` | Move between lines of a multi-line message; from the first/last line, browse previously sent messages (your unsent draft comes back at the end) |
  | `Home` / `End`, `Ctrl+A` / `Ctrl+E` | Start / end of the line |
  | `Alt+←` / `Alt+→`, `Alt+B` / `Alt+F` | Move by word (`Ctrl+←/→` switch tabs) |
  | `Backspace`, `Delete` / `Ctrl+D` | Delete before / under the cursor |
  | `Ctrl+W`, `Alt+Backspace` | Delete the previous word |
  | `Ctrl+U` / `Ctrl+K` | Delete to the start / end of the line |

  Message history lives in memory only and is gone when omnicrab exits, because prompts are never stored unencrypted. The mouse wheel and `PageUp`/`PageDown` scroll the chat; hold `Shift` while dragging to select text (mouse capture is on).
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
| `/model [name\|reset]` | Open the model picker, set the override, or clear it |
| `/plan` | Toggle plan mode (`Shift+Tab` cycles chat / plan / auto) |
| `/fanout a,b,c <prompt>` | Send one prompt to several accounts in parallel (names comma separated, no spaces) |
| `/orchestrate <task>` | Plan → parallel agents → synthesis (see below) |
| `/omni [init\|edit <doc>\|remember <note>]` | Encrypted project state in `.omni/` (see above) |
| `/<name> [args]` | A custom command from `.omni` |
| `/compact [focus]` | Summarize older messages now (see below) |
| `/rename <title>` | Rename the current session |
| `/passwd` | Change the master password |
| `/lock` | Lock the vault |
| `/clear` | Clear the screen (session is kept) |
| `/quit` | Exit |

### Orchestration

`/orchestrate <task>` uses the **Planner** of the Routing tab (or the active target when none is set):

1. The planner receives the agent roster and returns a JSON list `[{"agent": "...", "task": "..."}]`.
2. Each step runs in parallel on the named agent's account chain; steps without an agent run on the balanced executor pool (or on the planner's account when the pool is empty).
3. With 2+ steps the synthesizer writes the final answer; with one step that answer is final.

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
omnicrab                       # start the TUI in the current folder
omnicrab --workspace ~/proj    # work in another folder
omnicrab --read-only           # agents may not change the project
omnicrab doctor                # check database, Ollama and installed CLIs
omnicrab init-db               # run migrations only
```
