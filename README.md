# 🦀 omnicrab

omnicrab is a terminal UI that connects many AI accounts and local models and lets you chat with them, compare them, and have agents on different accounts work together. It shows how much each account is using, and everything it stores is encrypted before it reaches PostgreSQL.

## What you can do

- Connect several accounts per provider (Anthropic, OpenAI, Gemini, OpenRouter, Groq, DeepSeek, xAI, Mistral) and give each one its own name.
- Use local models through Ollama or any OpenAI-compatible server.
- Use Claude Code, Antigravity (`agy`), Codex and Gemini subscriptions through their official CLIs.
- Watch requests, tokens and cost per account, with optional monthly budgets, and see how much of each Claude subscription's 5-hour and weekly limits you have used.
- Define named agents and route messages with `@agent`.
- Send one prompt to several accounts with `/fanout`, or let agents split a task with `/orchestrate`.
- Fall back to another account automatically on rate limits, auth errors or exhausted budgets.
- Pick models from lists each account actually offers, and assign roles in the **Routing** tab: which account plans, which execute, which merge and which summarize.
- Balance work by free capacity (plan limits, budgets, local models) and turn on **plan mode** to review a plan before it runs.
- Save tokens: strong planner and cheap executors, summaries of old history, no synthesis for single-step plans.

## Install

You need Linux, Rust, and PostgreSQL.

```bash
git clone git@github.com:xn0kkx/Omnicrab.git
cd Omnicrab
cargo install --path . --root ~/.local --locked   # installs `omnicrab` into ~/.local/bin
```

Make sure `~/.local/bin` is in your `PATH`.

## Update

```bash
cd Omnicrab
git pull
cargo install --path . --root ~/.local --locked
```

## Set up

```bash
createdb omnicrab
export OMNICRAB_DATABASE_URL=postgres://USER@localhost/omnicrab   # password via ~/.pgpass or PGPASSWORD
omnicrab doctor    # checks the database, Ollama and installed CLIs
omnicrab           # starts the TUI
```

On first launch you choose a master password. It is never stored and cannot be recovered.

## Use it

1. Press `Tab` to open **Accounts**, then `a` to add an account (or `D` to detect installed CLIs). Press `t` to test it.
2. Press `Enter` on an account to make it the chat target, then type a message and press `Enter`.
3. Use `@name message` to talk to a specific account or agent, and `/help` to see all commands.
4. Open **Usage** (`F4`) to see consumption and plan limits per account.

| Key | Action |
| --- | --- |
| `Ctrl+P` | Quick tab switcher (`c` `a` `g` `u` `s` `r` or `1`–`6`) |
| `Ctrl+←` / `Ctrl+→`, click, `F1`–`F6` | Previous/next tab, click a tab, jump to a tab |
| `Shift+Tab` | Cycle mode: chat → plan (review before running) → auto |
| `Ctrl+O` / `/plan` | Toggle plan mode |
| `/model` | Open the model picker |
| `Enter` / `Alt+Enter` | Send / new line |
| `Esc` | Cancel running requests |
| `Ctrl+N` / `Ctrl+L` | New session / lock |

## Documentation

- [Usage guide](docs/usage.md)
- [Providers](docs/providers.md)
- [Architecture](docs/architecture.md)
- [Security](docs/security.md)
- [Development and project status](docs/development.md)
