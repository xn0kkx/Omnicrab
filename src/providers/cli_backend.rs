//! Official CLIs (`claude`, `codex`, `gemini`) in headless mode. This is how
//! subscription accounts are used without touching their OAuth tokens.

use std::process::Stdio;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::{ChatRequest, ProviderError, ProviderEvent, TokenUsage, which};
use crate::model::{Access, Account, LimitWindow, ProviderKind, RateLimits, Role};

/// CLIs are single-prompt, so history is flattened into a transcript.
fn flatten(req: &ChatRequest) -> String {
    if req.turns.len() == 1 {
        return req.turns[0].content.clone();
    }
    let mut s = String::from("Conversation so far:\n\n");
    for t in &req.turns[..req.turns.len().saturating_sub(1)] {
        let who = match t.role {
            Role::User => "User",
            Role::Assistant => "Assistant",
            Role::System => "System",
        };
        s.push_str(&format!("{who}: {}\n\n", t.content));
    }
    if let Some(last) = req.turns.last() {
        s.push_str(&format!("Reply to the latest message:\n{}", last.content));
    }
    s
}

fn binary(a: &Account) -> Result<String, ProviderError> {
    if let Some(p) = a.cli_path.as_ref().filter(|p| !p.is_empty()) {
        return Ok(p.clone());
    }
    let name = a.kind.cli_binary().unwrap_or_default();
    which(name)
        .map(|p| p.to_string_lossy().into_owned())
        .ok_or_else(|| ProviderError::new(format!("`{name}` not found in PATH"), true))
}

/// Expands `~` and creates the profile directory (private) if needed.
fn profile_path(dir: &str) -> std::path::PathBuf {
    let p = match dir.strip_prefix("~/") {
        Some(rest) => directories::BaseDirs::new()
            .map(|b| b.home_dir().join(rest))
            .unwrap_or_else(|| dir.into()),
        None => dir.into(),
    };
    let _ = std::fs::create_dir_all(&p);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o700));
    }
    p
}

/// Env for a CLI subprocess: points it at the account's own login directory
/// and drops variables that would override or confuse subscription auth.
/// omnicrab never reads what the CLI stores there.
fn apply_env(c: &mut Command, a: &Account) {
    if let (Some(var), Some(dir)) = (
        a.kind.profile_env(),
        a.profile_dir.as_deref().filter(|d| !d.is_empty()),
    ) {
        c.env(var, profile_path(dir));
    }
    if a.kind == ProviderKind::ClaudeCli {
        // An API key in the environment wins over the subscription in `-p` mode.
        for v in ["ANTHROPIC_API_KEY", "CLAUDECODE", "CLAUDE_CODE_ENTRYPOINT"] {
            c.env_remove(v);
        }
    }
}

/// Models a CLI can be asked for. Only `agy` can list them; the others get a static list.
pub async fn list_models(a: &Account) -> Result<Vec<String>, ProviderError> {
    match a.kind {
        ProviderKind::AgyCli => {
            let mut c = Command::new(binary(a)?);
            c.arg("models")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .current_dir(std::env::temp_dir());
            apply_env(&mut c, a);
            let out = tokio::time::timeout(std::time::Duration::from_secs(15), c.output())
                .await
                .map_err(|_| ProviderError::new("`agy models` timed out", true))?
                .map_err(|e| ProviderError::new(format!("cannot start `agy`: {e}"), true))?;
            Ok(parse_agy_models(&String::from_utf8_lossy(&out.stdout)))
        }
        ProviderKind::ClaudeCli => Ok([
            "fable",
            "opus",
            "sonnet",
            "haiku",
            "claude-fable-5-1",
            "claude-opus-5-5",
            "claude-sonnet-5-5",
            "claude-haiku-5-5",
        ]
        .map(String::from)
        .to_vec()),
        ProviderKind::CodexCli => Ok(["gpt-5", "gpt-5-codex", "o3"].map(String::from).to_vec()),
        _ => Ok(["gemini-2.5-pro", "gemini-2.5-flash"]
            .map(String::from)
            .to_vec()),
    }
}

/// `agy models` prints `id<TAB>Label` lines after a "Fetching..." banner.
fn parse_agy_models(out: &str) -> Vec<String> {
    out.lines()
        .filter_map(|l| l.split_once('\t'))
        .map(|(id, _)| id.trim().to_string())
        .filter(|id| !id.is_empty())
        .collect()
}

/// Interactive sign-in command for the account (inherits the terminal).
pub fn login_command(a: &Account) -> Result<std::process::Command, ProviderError> {
    if a.kind.login_args().is_empty() {
        return Err(ProviderError::new(
            "this provider has no separate sign-in flow",
            false,
        ));
    }
    let mut c = Command::new(binary(a)?);
    c.args(a.kind.login_args());
    apply_env(&mut c, a);
    Ok(c.into_std())
}

fn build_command(a: &Account, req: &ChatRequest, bin: &str) -> Command {
    let prompt = flatten(req);
    let access = req.workspace.as_ref().map(|w| w.access);
    let model = req
        .model
        .clone()
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| a.model.clone());
    let mut c = Command::new(bin);
    match a.kind {
        ProviderKind::ClaudeCli => {
            c.args([
                "-p",
                &prompt,
                "--output-format",
                "stream-json",
                "--verbose",
                "--include-partial-messages",
                "--no-session-persistence",
                "--strict-mcp-config",
                "--tools",
                claude_tools(access),
                "--allowedTools",
                claude_allowed(access),
            ]);
            if let Some(schema) = &req.json_schema {
                c.args(["--json-schema", schema]);
            }
            if let Some(ws) = &req.workspace {
                c.arg("--add-dir").arg(&ws.root);
                if ws.access == Access::Write {
                    c.args(["--permission-mode", "acceptEdits"]);
                }
                // The encrypted project state is not for agents to touch.
                c.args(["--disallowedTools", "Edit(.omni/**) Write(.omni/**)"]);
            }
            if let Some(s) = &req.system {
                c.args(["--append-system-prompt", s]);
            }
            if !model.is_empty() {
                c.args(["--model", &model]);
            }
            if let Some(e) = a.effort.as_deref().filter(|e| !e.is_empty()) {
                c.args(["--effort", e]);
            }
        }
        ProviderKind::CodexCli => {
            let prompt = match &req.system {
                Some(s) => format!("{s}\n\n{prompt}"),
                None => prompt,
            };
            let sandbox = match access {
                Some(Access::Write) => "workspace-write",
                _ => "read-only",
            };
            c.args([
                "exec",
                "--json",
                "--skip-git-repo-check",
                "--sandbox",
                sandbox,
            ]);
            if let Some(ws) = &req.workspace {
                c.arg("-C").arg(&ws.root);
            }
            if !model.is_empty() {
                c.args(["-m", &model]);
            }
            c.arg(prompt);
        }
        ProviderKind::AgyCli => {
            let prompt = match &req.system {
                Some(s) => format!("{s}\n\n{prompt}"),
                None => prompt,
            };
            // `--mode plan` keeps the agent read-only; only Write access lets it edit files.
            let mode = match access {
                Some(Access::Write) => "accept-edits",
                _ => "plan",
            };
            c.args([
                "--output-format",
                "stream-json",
                "--mode",
                mode,
                "--disable-slash-commands",
            ]);
            if let Some(ws) = &req.workspace {
                c.arg("--add-dir").arg(&ws.root);
            }
            if let Some(schema) = &req.json_schema {
                c.args(["--json-schema", schema]);
            }
            if !model.is_empty() {
                c.args(["--model", &model]);
            }
            c.args(["-p", &prompt]);
        }
        _ => {
            let prompt = match &req.system {
                Some(s) => format!("{s}\n\n{prompt}"),
                None => prompt,
            };
            c.args(["--output-format", "stream-json"]);
            if let Some(ws) = &req.workspace {
                c.arg("--include-directories").arg(&ws.root);
                if ws.access == Access::Write {
                    c.args(["--approval-mode", "auto_edit"]);
                }
            }
            if !model.is_empty() {
                c.args(["-m", &model]);
            }
            c.args(["-p", &prompt]);
        }
    }
    c.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    apply_env(&mut c, a);
    // Without a workspace run in a neutral directory so the CLI doesn't pick up project context.
    match &req.workspace {
        Some(ws) => c.current_dir(&ws.root),
        None => c.current_dir(std::env::temp_dir()),
    };
    c
}

fn claude_tools(access: Option<Access>) -> &'static str {
    match access {
        None => "WebSearch,WebFetch",
        Some(Access::Read) => "Read,Grep,Glob,WebSearch,WebFetch",
        Some(Access::Write) => "Read,Grep,Glob,Edit,Write,Bash,WebSearch,WebFetch",
    }
}

fn claude_allowed(access: Option<Access>) -> &'static str {
    match access {
        None => "WebSearch WebFetch",
        Some(Access::Read) => "Read Grep Glob WebSearch WebFetch",
        Some(Access::Write) => "Read Grep Glob Edit Write Bash WebSearch WebFetch",
    }
}

/// In `--mode plan`, agy starts its answer with a `[MAHI68]`-style marker line.
fn strip_agy_tag(s: &str) -> &str {
    let t = s.trim_start();
    match t.strip_prefix('[').and_then(|r| r.split_once(']')) {
        Some((tag, rest)) if !tag.is_empty() && tag.chars().all(|c| c.is_ascii_alphanumeric()) => {
            rest.trim_start_matches(['\n', ' '])
        }
        _ => s,
    }
}

/// agy restarts its answer after an internal retry, marking it with a new `[TAG]`
/// (upper-case letters and digits). Returns the text after the last such marker.
fn agy_restart(s: &str) -> Option<&str> {
    let mut found = None;
    for (i, _) in s.match_indices('[') {
        let rest = &s[i + 1..];
        let Some(end) = rest.find(']') else { continue };
        let tag = &rest[..end];
        if (4..=12).contains(&tag.len())
            && tag
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
            && tag.chars().any(|c| c.is_ascii_digit())
        {
            found = Some(rest[end + 1..].trim_start_matches(['\n', ' ']));
        }
    }
    found
}

#[derive(Default)]
struct Parsed {
    delta: Option<String>,
    /// Complete text (used when no deltas were streamed).
    full: Option<String>,
    usage: Option<TokenUsage>,
    error: Option<String>,
    model: Option<String>,
    /// Tool activity, e.g. "web search: query".
    notes: Vec<String>,
    /// Subscription plan limits (Claude only).
    limits: Option<RateLimits>,
}

fn u(v: &Value, k: &str) -> u64 {
    v.get(k).and_then(Value::as_u64).unwrap_or(0)
}

fn limit_window(v: &Value, fallback_reset: Option<i64>) -> Option<LimitWindow> {
    let utilization = v.get("utilization")?.as_f64()?;
    let secs = v
        .get("resetsAt")
        .and_then(Value::as_i64)
        .or(fallback_reset)?;
    Some(LimitWindow {
        utilization,
        resets_at: chrono::DateTime::from_timestamp(secs, 0)?,
    })
}

/// Reads the `rate_limit_info` of a Claude `rate_limit_event`. Prefers the
/// per-window map; falls back to the single window named by `rateLimitType`.
fn parse_rate_limits(info: &Value) -> Option<RateLimits> {
    let mut r = RateLimits {
        status: info["status"].as_str().unwrap_or_default().to_string(),
        ..Default::default()
    };
    let w = &info["unifiedWindows"];
    r.five_hour = limit_window(&w["five_hour"], None);
    r.seven_day = limit_window(&w["seven_day"], None);
    if r.is_empty() {
        let top = limit_window(info, info["resetsAt"].as_i64());
        match info["rateLimitType"].as_str() {
            Some("five_hour") => r.five_hour = top,
            Some("seven_day") => r.seven_day = top,
            _ => {}
        }
    }
    (!r.is_empty()).then_some(r)
}

/// One-line, length-capped version of a path or command for a note.
fn short(s: &str) -> String {
    let s = s.lines().next().unwrap_or("").trim();
    if s.chars().count() > 80 {
        format!("{}…", s.chars().take(79).collect::<String>())
    } else {
        s.to_string()
    }
}

fn tool_note(b: &Value) -> String {
    let i = &b["input"];
    let s = |k: &str| short(i[k].as_str().unwrap_or(""));
    match b["name"].as_str().unwrap_or("tool") {
        "WebSearch" => format!("web search: {}", s("query")),
        "WebFetch" => format!("fetch: {}", s("url")),
        "Read" => format!("read: {}", s("file_path")),
        "Edit" | "MultiEdit" => format!("edit: {}", s("file_path")),
        "Write" => format!("write: {}", s("file_path")),
        "Bash" => format!("$ {}", s("command")),
        "Grep" => format!("grep: {}", s("pattern")),
        "Glob" => format!("glob: {}", s("pattern")),
        n => n.to_string(),
    }
}

/// Tool activity of an agy `step_update` (anything that is not the answer itself).
/// Tools arrive as `{"step_type":"tool","tool_name":"run_command","tool_info":{"parameters":{..}}}`.
fn agy_step_note(st: &Value) -> Option<String> {
    let ty = st["step_type"].as_str()?;
    if st["state"] != "ACTIVE" || matches!(ty, "agent_response" | "user_input" | "finish") {
        return None;
    }
    let name = st["tool_name"].as_str().unwrap_or(ty);
    let target = st["tool_info"]["parameters"]
        .as_object()
        .and_then(|m| m.values().find_map(Value::as_str))
        .map(short)
        .unwrap_or_default();
    Some(if target.is_empty() {
        name.to_string()
    } else {
        format!("{name}: {target}")
    })
}

fn parse_line(kind: ProviderKind, line: &str) -> Parsed {
    let mut p = Parsed::default();
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return p;
    };
    let ty = v.get("type").and_then(Value::as_str).unwrap_or("");
    match kind {
        ProviderKind::ClaudeCli => match ty {
            "stream_event" => {
                let d = &v["event"]["delta"];
                if d["type"] == "text_delta" {
                    p.delta = d["text"].as_str().map(str::to_string);
                }
            }
            "assistant" => {
                let blocks = v["message"]["content"].as_array();
                let text: String = blocks
                    .map(|a| a.iter().filter_map(|b| b["text"].as_str()).collect())
                    .unwrap_or_default();
                for b in blocks.into_iter().flatten() {
                    if b["type"] != "tool_use" {
                        continue;
                    }
                    // With `--json-schema` the answer arrives as the input of this pseudo tool.
                    if b["name"] == "StructuredOutput" {
                        p.full = Some(b["input"].to_string());
                    } else {
                        p.notes.push(tool_note(b));
                    }
                }
                if !text.is_empty() {
                    p.full = Some(text);
                }
                p.model = v["message"]["model"].as_str().map(str::to_string);
            }
            "rate_limit_event" => p.limits = parse_rate_limits(&v["rate_limit_info"]),
            "result" => {
                if v["is_error"].as_bool().unwrap_or(false) {
                    p.error = Some(
                        v["result"]
                            .as_str()
                            .unwrap_or("claude CLI error")
                            .to_string(),
                    );
                } else {
                    let us = &v["usage"];
                    p.usage = Some(TokenUsage {
                        input: u(us, "input_tokens")
                            + u(us, "cache_read_input_tokens")
                            + u(us, "cache_creation_input_tokens"),
                        output: u(us, "output_tokens"),
                        reported_cost_usd: v["total_cost_usd"].as_f64(),
                        ..Default::default()
                    });
                }
            }
            _ => {}
        },
        ProviderKind::CodexCli => match ty {
            "item.completed" if v["item"]["type"] == "agent_message" => {
                p.full = v["item"]["text"].as_str().map(str::to_string);
            }
            "item.started" => {
                let it = &v["item"];
                p.notes.extend(match it["type"].as_str() {
                    Some("command_execution") => {
                        Some(format!("$ {}", short(it["command"].as_str().unwrap_or(""))))
                    }
                    Some("file_change") => Some("editing files".to_string()),
                    Some("web_search") => Some(format!(
                        "web search: {}",
                        short(it["query"].as_str().unwrap_or(""))
                    )),
                    _ => None,
                });
            }
            "turn.completed" => {
                let us = &v["usage"];
                p.usage = Some(TokenUsage {
                    input: u(us, "input_tokens"),
                    output: u(us, "output_tokens"),
                    reported_cost_usd: None,
                    ..Default::default()
                });
            }
            "turn.failed" | "error" => {
                p.error = Some(
                    v["error"]["message"]
                        .as_str()
                        .or(v["message"].as_str())
                        .unwrap_or("codex CLI error")
                        .to_string(),
                );
            }
            _ => {}
        },
        ProviderKind::AgyCli => match v["event"].as_str().unwrap_or("") {
            "step_update" => {
                let st = &v["step_update"];
                if st["step_type"] == "agent_response" && st["state"] == "ACTIVE" {
                    p.delta = st["text_delta"].as_str().map(str::to_string);
                } else {
                    p.notes.extend(agy_step_note(st));
                }
            }
            "result" => {
                let r = &v["result"];
                if r["status"] == "SUCCESS" {
                    // A schema-constrained answer comes ready parsed (the text has extra keys).
                    p.full = match &r["structured_output"] {
                        Value::Null => r["response"].as_str().map(str::to_string),
                        so => Some(so.to_string()),
                    };
                    p.usage = Some(TokenUsage {
                        input: u(&r["usage"], "input_tokens"),
                        output: u(&r["usage"], "output_tokens"),
                        reported_cost_usd: None,
                        ..Default::default()
                    });
                } else {
                    let msg = r["error"]
                        .as_str()
                        .or(r["response"].as_str())
                        .unwrap_or("agy CLI error");
                    p.error = Some(format!(
                        "{}: {msg}",
                        r["status"].as_str().unwrap_or("FAILED")
                    ));
                }
            }
            _ => {}
        },
        _ => match ty {
            "message" if v["role"] == "assistant" => {
                p.delta = v["content"].as_str().map(str::to_string);
            }
            "result" => {
                if v["status"] == "error" {
                    p.error = Some(
                        v["error"]["message"]
                            .as_str()
                            .unwrap_or("gemini CLI error")
                            .to_string(),
                    );
                } else {
                    let st = &v["stats"];
                    p.usage = Some(TokenUsage {
                        input: u(st, "input_tokens").max(u(st, "input")),
                        output: u(st, "output_tokens"),
                        reported_cost_usd: None,
                        ..Default::default()
                    });
                }
            }
            _ => {}
        },
    }
    p
}

pub async fn stream(
    account: &Account,
    req: ChatRequest,
    tx: mpsc::Sender<ProviderEvent>,
    cancel: CancellationToken,
) {
    let fail = |e: ProviderError| {
        let tx = tx.clone();
        async move {
            let _ = tx.send(ProviderEvent::Error(e)).await;
        }
    };
    let bin = match binary(account) {
        Ok(b) => b,
        Err(e) => return fail(e).await,
    };
    let mut child = match build_command(account, &req, &bin).spawn() {
        Ok(c) => c,
        Err(e) => {
            return fail(ProviderError::new(
                format!("cannot start `{bin}`: {e}"),
                true,
            ))
            .await;
        }
    };
    let stdout = child.stdout.take().expect("piped");
    let mut stderr = child.stderr.take().expect("piped");
    let err_task = tokio::spawn(async move {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s).await;
        s
    });

    let mut lines = BufReader::new(stdout).lines();
    let (mut streamed, mut last_full) = (false, None::<String>);
    let mut after_tool = false;
    let mut last_note = None::<String>;
    let (mut usage, mut model, mut error) = (None::<TokenUsage>, None::<String>, None::<String>);
    let mut limits = RateLimits::default();
    loop {
        let line = tokio::select! {
            _ = cancel.cancelled() => { let _ = child.kill().await; return; }
            l = lines.next_line() => l,
        };
        let Ok(Some(line)) = line else { break };
        let p = parse_line(account.kind, &line);
        for n in p.notes {
            if last_note.as_deref() == Some(n.as_str()) {
                continue;
            }
            last_note = Some(n.clone());
            let _ = tx.send(ProviderEvent::Note(n)).await;
            if streamed {
                after_tool = true;
            }
        }
        if let Some(mut d) = p.delta {
            if account.kind == ProviderKind::AgyCli {
                if !streamed {
                    d = strip_agy_tag(&d).to_string();
                } else if req.json_schema.is_none()
                    && let Some(rest) = agy_restart(&d)
                {
                    let rest = rest.to_string();
                    let _ = tx.send(ProviderEvent::Reset).await;
                    d = rest;
                }
            }
            if d.is_empty() {
                continue;
            }
            if after_tool {
                after_tool = false;
                let _ = tx.send(ProviderEvent::Delta("\n\n".into())).await;
            }
            streamed = true;
            if tx.send(ProviderEvent::Delta(d)).await.is_err() {
                let _ = child.kill().await;
                return;
            }
        }
        if p.full.is_some() {
            // Codex emits whole messages; Claude's `assistant` event repeats the deltas.
            if !streamed && account.kind == ProviderKind::CodexCli {
                let _ = tx.send(ProviderEvent::Delta(p.full.clone().unwrap())).await;
            }
            last_full = p.full;
        }
        if let Some(l) = p.limits {
            limits.merge(l);
        }
        usage = p.usage.or(usage);
        model = p.model.or(model);
        error = p.error.or(error);
    }
    let status = child.wait().await;
    let stderr_text = err_task.await.unwrap_or_default();
    tracing::debug!(
        account = %account.name,
        status = ?status,
        stderr = %stderr_text.trim().chars().take(2000).collect::<String>(),
        "cli finished"
    );

    if let Some(e) = error {
        return fail(ProviderError::classify(e)).await;
    }
    if !matches!(&status, Ok(s) if s.success()) {
        let msg = stderr_text.trim();
        let msg = if msg.is_empty() {
            format!("`{bin}` exited with {status:?}")
        } else {
            msg.to_string()
        };
        return fail(ProviderError::classify(msg)).await;
    }
    // Fallback for CLIs that don't stream deltas (claude without partials, gemini non-delta).
    if !streamed
        && account.kind != ProviderKind::CodexCli
        && let Some(t) = last_full
    {
        let t = if account.kind == ProviderKind::AgyCli {
            strip_agy_tag(&t).trim_end().to_string()
        } else {
            t
        };
        let _ = tx.send(ProviderEvent::Delta(t)).await;
    }
    let model = model.unwrap_or_else(|| {
        if account.model.is_empty() {
            account.kind.short().to_string()
        } else {
            account.model.clone()
        }
    });
    let mut usage = usage.unwrap_or_default();
    usage.rate_limits = (!limits.is_empty()).then_some(limits);
    let _ = tx.send(ProviderEvent::Done { usage, model }).await;
}

#[cfg(test)]
mod tests {
    #[test]
    fn agy_models_listing() {
        let out = "Fetching available models...\ngemini-3.8-flash-low\tGemini 3.8 Flash (Low)\nclaude-opus-5-5-high\tClaude Opus 5.5 (High)\n";
        assert_eq!(
            super::parse_agy_models(out),
            ["gemini-3.8-flash-low", "claude-opus-5-5-high"]
        );
    }

    use super::*;

    #[test]
    fn claude_profile_env() {
        let dir = std::env::temp_dir().join("omnicrab-test-profile");
        let a = Account {
            id: uuid::Uuid::nil(),
            name: "t".into(),
            kind: ProviderKind::ClaudeCli,
            api_key: None,
            base_url: None,
            model: String::new(),
            cli_path: None,
            profile_dir: Some(dir.to_string_lossy().into_owned()),
            budget_usd_month: None,
            enabled: true,
            effort: None,
        };
        let mut c = Command::new("claude");
        apply_env(&mut c, &a);
        let envs: Vec<_> = c.as_std().get_envs().collect();
        let get = |k: &str| envs.iter().find(|(n, _)| *n == k).map(|(_, v)| *v);
        assert_eq!(get("CLAUDE_CONFIG_DIR"), Some(Some(dir.as_os_str())));
        assert_eq!(get("ANTHROPIC_API_KEY"), Some(None));
        assert!(dir.is_dir());
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn claude_rate_limit_event() {
        let line = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","resetsAt":1792001400,"rateLimitType":"five_hour","unifiedWindows":{"five_hour":{"utilization":0.54,"resetsAt":1792001400},"seven_day":{"utilization":0.64,"resetsAt":1792126800}}}}"#;
        let l = parse_line(ProviderKind::ClaudeCli, line).limits.unwrap();
        assert_eq!(l.status, "allowed");
        assert_eq!(l.five_hour.unwrap().utilization, 0.54);
        assert_eq!(l.seven_day.unwrap().resets_at.timestamp(), 1792126800);

        // Only the top-level window: falls back to `rateLimitType`.
        let line = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","resetsAt":1792126800,"rateLimitType":"seven_day","utilization":0.9}}"#;
        let l = parse_line(ProviderKind::ClaudeCli, line).limits.unwrap();
        assert!(l.five_hour.is_none());
        assert_eq!(l.seven_day.unwrap().utilization, 0.9);

        // Nothing usable.
        let line = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed"}}"#;
        assert!(parse_line(ProviderKind::ClaudeCli, line).limits.is_none());
    }

    #[test]
    fn claude_delta_and_result() {
        let d = parse_line(
            ProviderKind::ClaudeCli,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"hi"}}}"#,
        );
        assert_eq!(d.delta.as_deref(), Some("hi"));
        let r = parse_line(
            ProviderKind::ClaudeCli,
            r#"{"type":"result","is_error":false,"total_cost_usd":0.0123,"usage":{"input_tokens":10,"output_tokens":5}}"#,
        );
        let u = r.usage.unwrap();
        assert_eq!(
            (u.input, u.output, u.reported_cost_usd),
            (10, 5, Some(0.0123))
        );
    }

    #[test]
    fn claude_tool_use_becomes_note() {
        let p = parse_line(
            ProviderKind::ClaudeCli,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"WebSearch","input":{"query":"dalai lama"}}]}}"#,
        );
        assert_eq!(p.notes, ["web search: dalai lama"]);
        assert!(p.full.is_none());
    }

    #[test]
    fn tool_notes_cover_files_and_shell() {
        let note = |name: &str, input: &str| {
            let line = format!(
                r#"{{"type":"assistant","message":{{"content":[{{"type":"tool_use","name":"{name}","input":{input}}}]}}}}"#
            );
            parse_line(ProviderKind::ClaudeCli, &line).notes.remove(0)
        };
        assert_eq!(
            note("Read", r#"{"file_path":"src/a.rs"}"#),
            "read: src/a.rs"
        );
        assert_eq!(
            note("Bash", r#"{"command":"cargo test\nmore"}"#),
            "$ cargo test"
        );
        assert_eq!(note("Grep", r#"{"pattern":"fn main"}"#), "grep: fn main");
        let c = parse_line(
            ProviderKind::CodexCli,
            r#"{"type":"item.started","item":{"type":"command_execution","command":"ls"}}"#,
        );
        assert_eq!(c.notes, ["$ ls"]);
    }

    #[test]
    fn claude_structured_output_is_the_answer() {
        let p = parse_line(
            ProviderKind::ClaudeCli,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"StructuredOutput","input":{"actions":[{"type":"finish","answer":"pong"}]}}]}}"#,
        );
        assert!(p.notes.is_empty(), "not shown as tool activity");
        assert_eq!(
            p.full.as_deref(),
            Some(r#"{"actions":[{"answer":"pong","type":"finish"}]}"#)
        );
    }

    #[test]
    fn claude_notes_every_tool_use() {
        let p = parse_line(
            ProviderKind::ClaudeCli,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Read","input":{"file_path":"a.rs"}},{"type":"tool_use","name":"Grep","input":{"pattern":"x"}}]}}"#,
        );
        assert_eq!(p.notes, ["read: a.rs", "grep: x"]);
    }

    #[test]
    fn agy_tools_and_structured_result() {
        let t = parse_line(
            ProviderKind::AgyCli,
            r#"{"event":"step_update","step_update":{"step_type":"tool","state":"ACTIVE","tool_name":"run_command","tool_info":{"name":"run_command","parameters":{"CommandLine":"echo hi"}}}}"#,
        );
        assert_eq!(t.notes, ["run_command: echo hi"]);
        let done = parse_line(
            ProviderKind::AgyCli,
            r#"{"event":"step_update","step_update":{"step_type":"tool","state":"DONE","tool_name":"run_command"}}"#,
        );
        assert!(done.notes.is_empty());
        let r = parse_line(
            ProviderKind::AgyCli,
            r#"{"event":"result","result":{"status":"SUCCESS","response":"{\"x\":1,\"toolAction\":\"y\"}","structured_output":{"x":1},"usage":{}}}"#,
        );
        assert_eq!(r.full.as_deref(), Some(r#"{"x":1}"#));
    }

    #[test]
    fn json_schema_reaches_claude_and_agy() {
        let a = |kind| {
            let acct = Account {
                id: uuid::Uuid::nil(),
                name: "t".into(),
                kind,
                api_key: None,
                base_url: None,
                model: String::new(),
                cli_path: None,
                profile_dir: None,
                budget_usd_month: None,
                enabled: true,
                effort: None,
            };
            let req = ChatRequest {
                turns: vec![super::super::ChatTurn {
                    role: Role::User,
                    content: "hi".into(),
                }],
                json_schema: Some("{}".into()),
                ..Default::default()
            };
            build_command(&acct, &req, "bin")
                .as_std()
                .get_args()
                .map(|a| a.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        };
        assert!(has(&a(ProviderKind::ClaudeCli), "--json-schema", "{}"));
        assert!(has(&a(ProviderKind::AgyCli), "--json-schema", "{}"));
    }

    #[test]
    fn codex_message_and_usage() {
        let m = parse_line(
            ProviderKind::CodexCli,
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"yo"}}"#,
        );
        assert_eq!(m.full.as_deref(), Some("yo"));
        let t = parse_line(
            ProviderKind::CodexCli,
            r#"{"type":"turn.completed","usage":{"input_tokens":3,"output_tokens":4}}"#,
        );
        assert_eq!(t.usage.unwrap().output, 4);
    }

    #[test]
    fn agy_delta_result_and_error() {
        let d = parse_line(
            ProviderKind::AgyCli,
            r#"{"event":"step_update","step_update":{"step_type":"agent_response","state":"ACTIVE","text_delta":"pong"}}"#,
        );
        assert_eq!(d.delta.as_deref(), Some("pong"));
        let done = parse_line(
            ProviderKind::AgyCli,
            r#"{"event":"step_update","step_update":{"step_type":"agent_response","state":"DONE","text_delta":"\n"}}"#,
        );
        assert!(done.delta.is_none());
        let r = parse_line(
            ProviderKind::AgyCli,
            r#"{"event":"result","result":{"status":"SUCCESS","response":"pong\n","usage":{"input_tokens":7,"output_tokens":3}}}"#,
        );
        assert_eq!(r.usage.unwrap().input, 7);
        let e = parse_line(
            ProviderKind::AgyCli,
            r#"{"event":"result","result":{"status":"FAILED","error":"quota exceeded"}}"#,
        );
        assert!(e.error.unwrap().contains("quota"));
    }

    #[test]
    fn agy_tag_is_stripped() {
        assert_eq!(strip_agy_tag("[MAHI68]\nTokio"), "Tokio");
        assert_eq!(strip_agy_tag("[MAHI68] Tokio"), "Tokio");
        assert_eq!(strip_agy_tag("Tokio"), "Tokio");
        assert_eq!(strip_agy_tag("[note: x] keep"), "[note: x] keep");
    }

    #[test]
    fn agy_restart_marker_mid_stream() {
        assert_eq!(
            agy_restart("aplicado[MAHI68] Nenhum repo"),
            Some("Nenhum repo")
        );
        assert_eq!(agy_restart("see [docs] and [OK]"), None);
        assert_eq!(agy_restart("plain"), None);
    }

    fn cmd_args(
        kind: ProviderKind,
        access: Option<Access>,
    ) -> (Vec<String>, Option<std::path::PathBuf>) {
        let a = Account {
            id: uuid::Uuid::nil(),
            name: "t".into(),
            kind,
            api_key: None,
            base_url: None,
            model: String::new(),
            cli_path: None,
            profile_dir: None,
            budget_usd_month: None,
            enabled: true,
            effort: None,
        };
        let req = ChatRequest {
            turns: vec![super::super::ChatTurn {
                role: Role::User,
                content: "hi".into(),
            }],
            workspace: access.map(|access| crate::model::Workspace {
                root: "/proj".into(),
                access,
            }),
            ..Default::default()
        };
        let c = build_command(&a, &req, "bin");
        let c = c.as_std();
        let args = c
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        (args, c.get_current_dir().map(|d| d.to_path_buf()))
    }

    fn has(args: &[String], a: &str, b: &str) -> bool {
        args.windows(2).any(|w| w[0] == a && w[1] == b)
    }

    #[test]
    fn workspace_access_maps_to_cli_flags() {
        // No workspace: neutral dir, web tools only.
        let (args, dir) = cmd_args(ProviderKind::ClaudeCli, None);
        assert_eq!(dir, Some(std::env::temp_dir()));
        assert!(has(&args, "--tools", "WebSearch,WebFetch"));
        assert!(!args.iter().any(|a| a == "--add-dir"));

        let (args, dir) = cmd_args(ProviderKind::ClaudeCli, Some(Access::Read));
        assert_eq!(dir, Some("/proj".into()));
        assert!(has(&args, "--tools", "Read,Grep,Glob,WebSearch,WebFetch"));
        assert!(has(&args, "--add-dir", "/proj"));
        assert!(!args.iter().any(|a| a == "--permission-mode"));

        let (args, _) = cmd_args(ProviderKind::ClaudeCli, Some(Access::Write));
        assert!(has(&args, "--permission-mode", "acceptEdits"));
        assert!(args.iter().any(|a| a.contains("Bash")));
        assert!(has(
            &args,
            "--disallowedTools",
            "Edit(.omni/**) Write(.omni/**)"
        ));

        let (args, _) = cmd_args(ProviderKind::CodexCli, Some(Access::Write));
        assert!(has(&args, "--sandbox", "workspace-write") && has(&args, "-C", "/proj"));
        let (args, _) = cmd_args(ProviderKind::CodexCli, Some(Access::Read));
        assert!(has(&args, "--sandbox", "read-only"));

        let (args, _) = cmd_args(ProviderKind::AgyCli, Some(Access::Write));
        assert!(has(&args, "--mode", "accept-edits") && has(&args, "--add-dir", "/proj"));
        let (args, _) = cmd_args(ProviderKind::AgyCli, None);
        assert!(has(&args, "--mode", "plan"));

        let (args, _) = cmd_args(ProviderKind::GeminiCli, Some(Access::Write));
        assert!(has(&args, "--approval-mode", "auto_edit"));
    }

    #[test]
    fn garbage_is_ignored() {
        assert!(
            parse_line(ProviderKind::GeminiCli, "not json")
                .delta
                .is_none()
        );
    }
}
