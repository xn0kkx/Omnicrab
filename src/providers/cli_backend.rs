//! Official CLIs (`claude`, `codex`, `gemini`) in headless mode. This is how
//! subscription accounts are used without touching their OAuth tokens.

use std::process::Stdio;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::{ChatRequest, ProviderError, ProviderEvent, TokenUsage, which};
use crate::model::{Account, ProviderKind, Role};

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

fn build_command(a: &Account, req: &ChatRequest, bin: &str) -> Command {
    let prompt = flatten(req);
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
                "",
            ]);
            if let Some(s) = &req.system {
                c.args(["--append-system-prompt", s]);
            }
            if !model.is_empty() {
                c.args(["--model", &model]);
            }
        }
        ProviderKind::CodexCli => {
            let prompt = match &req.system {
                Some(s) => format!("{s}\n\n{prompt}"),
                None => prompt,
            };
            c.args([
                "exec",
                "--json",
                "--skip-git-repo-check",
                "--sandbox",
                "read-only",
            ]);
            if !model.is_empty() {
                c.args(["-m", &model]);
            }
            c.arg(prompt);
        }
        _ => {
            let prompt = match &req.system {
                Some(s) => format!("{s}\n\n{prompt}"),
                None => prompt,
            };
            c.args(["--output-format", "stream-json"]);
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
    // Run in a neutral directory so the CLI doesn't pick up project context/tools.
    c.current_dir(std::env::temp_dir());
    c
}

#[derive(Default)]
struct Parsed {
    delta: Option<String>,
    /// Complete text (used when no deltas were streamed).
    full: Option<String>,
    usage: Option<TokenUsage>,
    error: Option<String>,
    model: Option<String>,
}

fn u(v: &Value, k: &str) -> u64 {
    v.get(k).and_then(Value::as_u64).unwrap_or(0)
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
                let text: String = v["message"]["content"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|b| b["text"].as_str()).collect())
                    .unwrap_or_default();
                if !text.is_empty() {
                    p.full = Some(text);
                }
                p.model = v["message"]["model"].as_str().map(str::to_string);
            }
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
                    });
                }
            }
            _ => {}
        },
        ProviderKind::CodexCli => match ty {
            "item.completed" if v["item"]["type"] == "agent_message" => {
                p.full = v["item"]["text"].as_str().map(str::to_string);
            }
            "turn.completed" => {
                let us = &v["usage"];
                p.usage = Some(TokenUsage {
                    input: u(us, "input_tokens"),
                    output: u(us, "output_tokens"),
                    reported_cost_usd: None,
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
    let (mut usage, mut model, mut error) = (None::<TokenUsage>, None::<String>, None::<String>);
    loop {
        let line = tokio::select! {
            _ = cancel.cancelled() => { let _ = child.kill().await; return; }
            l = lines.next_line() => l,
        };
        let Ok(Some(line)) = line else { break };
        let p = parse_line(account.kind, &line);
        if let Some(d) = p.delta {
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
        usage = p.usage.or(usage);
        model = p.model.or(model);
        error = p.error.or(error);
    }
    let status = child.wait().await;
    let stderr_text = err_task.await.unwrap_or_default();

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
        let _ = tx.send(ProviderEvent::Delta(t)).await;
    }
    let model = model.unwrap_or_else(|| {
        if account.model.is_empty() {
            account.kind.short().to_string()
        } else {
            account.model.clone()
        }
    });
    let _ = tx
        .send(ProviderEvent::Done {
            usage: usage.unwrap_or_default(),
            model,
        })
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn garbage_is_ignored() {
        assert!(
            parse_line(ProviderKind::GeminiCli, "not json")
                .delta
                .is_none()
        );
    }
}
