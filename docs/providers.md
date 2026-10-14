# Providers

## API-key providers

Handled by the [`genai`](https://crates.io/crates/genai) crate. A fresh client is built per request with a service-target resolver that injects the account's adapter, endpoint and API key, so any number of accounts of the same provider coexist.

| Provider | Notes |
| --- | --- |
| Anthropic | Native protocol |
| OpenAI | Chat Completions |
| Google Gemini | Native protocol |
| OpenRouter, Groq, DeepSeek, xAI | Native adapters |
| Mistral | OpenAI-compatible endpoint (`https://api.mistral.ai/v1`) |

Default model names are suggestions and can be edited in the account form.

## Local models

| Kind | Default URL | Notes |
| --- | --- | --- |
| Ollama | `http://localhost:11434` | `D` in the Accounts tab detects it and uses the first installed model. |
| OpenAI-compatible | `http://localhost:8080/v1` | llama.cpp server, LM Studio, vLLM, LocalAI… Set the model name your server expects. |

Local accounts have no API key and cost `$0`.

## Subscription accounts via official CLIs

Anthropic's terms do not allow using Free/Pro/Max OAuth tokens in third-party applications, and other vendors have similar restrictions. omnicrab therefore **never reads or reuses subscription credentials**. Instead it runs the vendor's own CLI as a subprocess, which authenticates itself:

| Kind | Command (simplified) |
| --- | --- |
| Claude Code | `claude -p <prompt> --output-format stream-json --verbose --include-partial-messages --no-session-persistence --strict-mcp-config --tools "WebSearch,WebFetch" --allowedTools "WebSearch WebFetch"` |
| Codex | `codex exec --json --skip-git-repo-check --sandbox read-only <prompt>` |
| Antigravity (`agy`) | `agy --output-format stream-json --mode plan --disable-slash-commands -p <prompt>` |
| Gemini CLI | `gemini --output-format stream-json -p <prompt>` |

Details:

- Log in once with the CLI itself (`claude`, `codex login`, `gemini`).
- The subprocess runs in the system temp directory with tools disabled or read-only, so it cannot touch your projects.
- Claude accounts get only the read-only web tools (`WebSearch`, `WebFetch`), so "search the web" requests work. With `--tools ""` the model would print a fake `<tool_use>` block and no answer. Tool calls show up as a dim note (`web search: ...`) above the answer.
- Multi-turn history is flattened into one prompt because these CLIs are single-shot.
- `--strict-mcp-config` stops the CLI from loading the MCP servers of your global config, which otherwise adds ~20k input tokens (about 5× the cost) to every request.
- Token counts and (for Claude) `total_cost_usd` come from the CLI's final JSON event.
- A custom binary path can be set per account.
- **Antigravity (`agy`)**: runs with `--mode plan` so the agent stays read-only and nothing is auto-approved. Plan mode prefixes answers with a marker like `[MAHI68]`, which omnicrab strips. `agy` reports tokens but no cost, and its model name is not part of the output, so cost shows `$0` unless you set the account's **Model** (for example `claude-sonnet-5-5-medium`, see `agy models`) and the price table has a matching prefix. Its built-in context is large (~17k input tokens per request).
- Output formats change between CLI versions. Parsers ignore unknown events; if a CLI starts failing, check the log and `omnicrab doctor`.

Check the current terms of each vendor before relying on this for automation.

### Multiple subscription accounts

Each CLI-backed account can have its own **Profile dir**. omnicrab starts the CLI with an environment variable pointing at that directory, so every account keeps a separate login:

| Kind | Variable | Status |
| --- | --- | --- |
| Claude Code | `CLAUDE_CONFIG_DIR` | Supported |
| Antigravity (`agy`) | none documented | Not supported yet. `agy` keeps one login per machine (system keyring). A per-account `HOME` is reported to work by community tools but is unverified here. |

Flow for a second Claude account: add an account of kind *Claude Code CLI*, set **Profile dir** (for example `~/.omnicrab/claude-work`), save, select it in **Accounts** and press `L`. omnicrab suspends the TUI and runs `claude auth login` for that profile, then tests the connection. Sign-in goes through Anthropic's own flow; omnicrab never reads or stores the credentials in the directory.

For Claude accounts omnicrab also removes `ANTHROPIC_API_KEY` from the subprocess environment, because in `-p` mode an API key would otherwise take precedence over the subscription.

## Pricing

Cost for API accounts = `tokens_in × input + tokens_out × output` per 1M tokens, looked up by the **longest model-name prefix** in:

1. entries from `[prices]` in `config.toml` (win ties), then
2. a built-in table of approximate list prices (Claude, GPT, Gemini, DeepSeek, Grok, Mistral, Llama).

Unknown models cost 0 and are still counted in requests and tokens. Prices change; override them in the config.

## Adding a provider

Implement a new `ProviderKind` in `src/model.rs` and map it in `src/providers/genai_backend.rs` (`adapter_for`, `endpoint_for`), or add a new backend module that sends `ProviderEvent`s on the channel given to `stream_chat`.
