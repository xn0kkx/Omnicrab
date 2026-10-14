//! HTTP providers through the `genai` crate. One client per request, with an
//! account-scoped service-target resolver (adapter + endpoint + API key).

use futures::StreamExt;
use genai::adapter::AdapterKind;
use genai::chat::{ChatMessage, ChatOptions, ChatRequest as GReq, ChatStreamEvent};
use genai::resolver::{AuthData, Endpoint, ServiceTargetResolver};
use genai::{Client, ModelIden, ServiceTarget};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::{ChatRequest, ProviderError, ProviderEvent, TokenUsage, secret_str};
use crate::model::{Account, ProviderKind, Role};

pub fn adapter_of(kind: ProviderKind) -> AdapterKind {
    adapter_for(kind)
}

fn adapter_for(kind: ProviderKind) -> AdapterKind {
    match kind {
        ProviderKind::Anthropic => AdapterKind::Anthropic,
        ProviderKind::OpenAi => AdapterKind::OpenAI,
        ProviderKind::Gemini => AdapterKind::Gemini,
        ProviderKind::OpenRouter => AdapterKind::OpenRouter,
        ProviderKind::Groq => AdapterKind::Groq,
        ProviderKind::DeepSeek => AdapterKind::DeepSeek,
        ProviderKind::Xai => AdapterKind::Xai,
        ProviderKind::Ollama => AdapterKind::Ollama,
        // Mistral and generic servers speak the OpenAI chat protocol.
        ProviderKind::Mistral | ProviderKind::OpenAiCompat => AdapterKind::OpenAI,
        _ => AdapterKind::OpenAI,
    }
}

/// Custom endpoint for kinds that don't use genai's built-in default URL.
fn endpoint_for(a: &Account) -> Option<String> {
    let base = a.base_url.clone().or_else(|| match a.kind {
        ProviderKind::Mistral => Some("https://api.mistral.ai/v1".to_string()),
        k => k.default_base_url().map(str::to_string),
    })?;
    // genai joins relative paths onto the endpoint, so it needs a trailing slash.
    Some(if base.ends_with('/') {
        base
    } else {
        format!("{base}/")
    })
}

pub fn build_client(a: &Account) -> Client {
    let adapter = adapter_for(a.kind);
    let endpoint = endpoint_for(a);
    let key = secret_str(&a.api_key).map(str::to_string);
    let resolver = ServiceTargetResolver::from_resolver_fn(
        move |t: ServiceTarget| -> Result<ServiceTarget, genai::resolver::Error> {
            let ServiceTarget {
                endpoint: default_ep,
                model,
                ..
            } = t;
            let endpoint = match &endpoint {
                Some(e) => Endpoint::from_owned(e.clone()),
                None => default_ep,
            };
            let auth = match &key {
                Some(k) => AuthData::from_single(k.clone()),
                // Local servers ignore the key, but some adapters require a non-empty one.
                None => AuthData::from_single("none"),
            };
            Ok(ServiceTarget {
                endpoint,
                auth,
                model: ModelIden::new(adapter, model.model_name),
            })
        },
    );
    Client::builder()
        .with_service_target_resolver(resolver)
        .build()
}

fn to_genai(req: &ChatRequest) -> GReq {
    let mut r = GReq::default();
    if let Some(s) = &req.system {
        r = r.with_system(s.clone());
    }
    for t in &req.turns {
        r = r.append_message(match t.role {
            Role::User => ChatMessage::user(t.content.clone()),
            Role::Assistant => ChatMessage::assistant(t.content.clone()),
            Role::System => ChatMessage::system(t.content.clone()),
        });
    }
    r
}

pub async fn stream(
    account: &Account,
    req: ChatRequest,
    tx: mpsc::Sender<ProviderEvent>,
    cancel: CancellationToken,
) {
    let model = req
        .model
        .clone()
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| account.model.clone());
    let client = build_client(account);
    let opts = ChatOptions::default().with_capture_usage(true);

    let resp = tokio::select! {
        _ = cancel.cancelled() => return,
        r = client.exec_chat_stream(model.as_str(), to_genai(&req), Some(&opts)) => r,
    };
    let mut stream = match resp {
        Ok(r) => r.stream,
        Err(e) => {
            let _ = tx
                .send(ProviderEvent::Error(ProviderError::classify(e.to_string())))
                .await;
            return;
        }
    };

    let mut usage = TokenUsage::default();
    loop {
        let next = tokio::select! {
            _ = cancel.cancelled() => return,
            n = stream.next() => n,
        };
        match next {
            None => break,
            Some(Err(e)) => {
                let _ = tx
                    .send(ProviderEvent::Error(ProviderError::classify(e.to_string())))
                    .await;
                return;
            }
            Some(Ok(ChatStreamEvent::Chunk(c))) => {
                if tx.send(ProviderEvent::Delta(c.content)).await.is_err() {
                    return;
                }
            }
            Some(Ok(ChatStreamEvent::End(end))) => {
                if let Some(u) = end.captured_usage {
                    usage.input = u.prompt_tokens.unwrap_or(0).max(0) as u64;
                    usage.output = u.completion_tokens.unwrap_or(0).max(0) as u64;
                }
            }
            Some(Ok(_)) => {}
        }
    }
    let _ = tx.send(ProviderEvent::Done { usage, model }).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acct(kind: ProviderKind, base: Option<&str>) -> Account {
        Account {
            id: uuid::Uuid::nil(),
            name: "t".into(),
            kind,
            api_key: None,
            base_url: base.map(Into::into),
            model: "m".into(),
            cli_path: None,
            profile_dir: None,
            budget_usd_month: None,
            enabled: true,
            effort: None,
        }
    }

    #[test]
    fn endpoint_gets_trailing_slash() {
        assert_eq!(
            endpoint_for(&acct(ProviderKind::OpenAiCompat, Some("http://x:1/v1"))).unwrap(),
            "http://x:1/v1/"
        );
        assert_eq!(
            endpoint_for(&acct(ProviderKind::Ollama, None)).unwrap(),
            "http://localhost:11434/"
        );
        assert!(endpoint_for(&acct(ProviderKind::Anthropic, None)).is_none());
        assert_eq!(
            endpoint_for(&acct(
                ProviderKind::Anthropic,
                Some("https://gw.example/claude/v1")
            ))
            .unwrap(),
            "https://gw.example/claude/v1/"
        );
    }
}
