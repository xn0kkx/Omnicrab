//! Provider backends. All of them stream [`ProviderEvent`]s over an mpsc channel.

pub mod cli_backend;
pub mod genai_backend;
pub mod ollama_discovery;

use secrecy::SecretString;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::model::{Account, Role};

#[derive(Clone, Debug)]
pub struct ChatTurn {
    pub role: Role,
    pub content: String,
}

#[derive(Clone, Debug, Default)]
pub struct ChatRequest {
    pub system: Option<String>,
    pub turns: Vec<ChatTurn>,
    /// Overrides the account's default model.
    pub model: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct TokenUsage {
    pub input: u64,
    pub output: u64,
    /// Cost reported by the provider itself (CLIs). `None` => compute from price table.
    pub reported_cost_usd: Option<f64>,
    /// Plan limits reported by the CLI during the request.
    pub rate_limits: Option<crate::model::RateLimits>,
}

#[derive(Clone, Debug)]
pub enum ProviderEvent {
    Delta(String),
    /// Tool activity (e.g. a web search); shown as a note, not as answer text.
    Note(String),
    Done {
        usage: TokenUsage,
        model: String,
    },
    Error(ProviderError),
}

#[derive(Clone, Debug, thiserror::Error)]
#[error("{message}")]
pub struct ProviderError {
    pub message: String,
    /// Worth retrying on another account (auth/rate-limit/5xx/network).
    pub failover: bool,
}

impl ProviderError {
    pub fn new(message: impl Into<String>, failover: bool) -> Self {
        Self {
            message: message.into(),
            failover,
        }
    }

    /// Heuristic classification of error text.
    pub fn classify(message: impl Into<String>) -> Self {
        let message = message.into();
        let m = message.to_lowercase();
        let failover = [
            "401",
            "403",
            "429",
            "500",
            "502",
            "503",
            "504",
            "overloaded",
            "rate",
            "quota",
            "unauthorized",
            "timed out",
            "timeout",
            "connect",
            "credit",
            "billing",
            "not found in path",
            "error sending request",
            "web stream error",
            "refused",
            "unreachable",
            "dns",
            "network",
            "cannot start",
        ]
        .iter()
        .any(|k| m.contains(k));
        Self { message, failover }
    }
}

/// Stream a chat for `account`. Never panics; failures arrive as `ProviderEvent::Error`.
pub async fn stream_chat(
    account: &Account,
    req: ChatRequest,
    tx: mpsc::Sender<ProviderEvent>,
    cancel: CancellationToken,
) {
    if account.kind.is_cli() {
        cli_backend::stream(account, req, tx, cancel).await
    } else {
        genai_backend::stream(account, req, tx, cancel).await
    }
}

/// Collect a whole response (used by the orchestrator for non-interactive steps).
pub async fn complete(
    account: &Account,
    req: ChatRequest,
    cancel: CancellationToken,
) -> Result<(String, TokenUsage, String), ProviderError> {
    let (tx, mut rx) = mpsc::channel(64);
    let acct = account.clone();
    let c = cancel.clone();
    let h = tokio::spawn(async move { stream_chat(&acct, req, tx, c).await });
    let mut text = String::new();
    let mut out = None;
    while let Some(ev) = rx.recv().await {
        match ev {
            ProviderEvent::Delta(d) => text.push_str(&d),
            ProviderEvent::Done { usage, model } => out = Some(Ok((usage, model))),
            ProviderEvent::Error(e) => out = Some(Err(e)),
            ProviderEvent::Note(_) => {}
        }
    }
    let _ = h.await;
    match out {
        Some(Ok((u, m))) => Ok((text, u, m)),
        Some(Err(e)) => Err(e),
        None => Err(ProviderError::new("cancelled", false)),
    }
}

/// Connectivity check: 1-token style ping. Returns the model name that answered.
pub async fn test_connection(account: &Account) -> Result<String, ProviderError> {
    let req = ChatRequest {
        system: None,
        turns: vec![ChatTurn {
            role: Role::User,
            content: "Reply with the single word: ok".into(),
        }],
        model: None,
    };
    let fut = complete(account, req, CancellationToken::new());
    match tokio::time::timeout(std::time::Duration::from_secs(60), fut).await {
        Ok(Ok((_, _, model))) => Ok(model),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(ProviderError::new("timed out", true)),
    }
}

pub fn which(bin: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(bin))
        .find(|p| p.is_file())
}

pub fn secret_str(s: &Option<SecretString>) -> Option<&str> {
    use secrecy::ExposeSecret;
    s.as_ref().map(|k| k.expose_secret())
}

#[cfg(test)]
mod tests {
    use super::ProviderError;

    #[test]
    fn classifies_transport_and_api_errors_as_failover() {
        assert!(ProviderError::classify("Web stream error for model 'x'.\nCause: error sending request for url (http://localhost:8080/v1)").failover);
        assert!(ProviderError::classify("HTTP 429 Too Many Requests").failover);
        assert!(!ProviderError::classify("invalid request: messages must not be empty").failover);
    }
}
