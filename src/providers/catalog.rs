//! Which models an account can serve, for the model picker and the routing tab.

use std::time::Duration;

use serde::Deserialize;

use super::{ProviderError, cli_backend, genai_backend, ollama_discovery, secret_str};
use crate::model::{Account, ProviderKind};

#[derive(Deserialize)]
struct OpenAiList {
    data: Vec<OpenAiModel>,
}
#[derive(Deserialize)]
struct OpenAiModel {
    id: String,
}

/// Lists the models of one account. Never blocks for long; callers show the
/// error and fall back to free-text entry.
pub async fn list_models(a: &Account) -> Result<Vec<String>, ProviderError> {
    let fut = async {
        match a.kind {
            ProviderKind::Ollama => {
                let base = a
                    .base_url
                    .as_deref()
                    .or(a.kind.default_base_url())
                    .unwrap_or_default();
                ollama_discovery::list_models(base)
                    .await
                    .map_err(|e| ProviderError::classify(format!("{e:#}")))
            }
            ProviderKind::OpenAiCompat => openai_compat(a).await,
            k if k.is_cli() => cli_backend::list_models(a).await,
            // A gateway lists its own models; genai only knows the official ones.
            _ if a.base_url.is_some() => openai_compat(a).await,
            _ => {
                let client = genai_backend::build_client(a);
                let auth = genai::resolver::AuthData::from_single(
                    secret_str(&a.api_key).unwrap_or("none").to_string(),
                );
                client
                    .all_model_names(genai_backend::adapter_of(a.kind), auth)
                    .await
                    .map_err(|e| ProviderError::classify(e.to_string()))
            }
        }
    };
    let mut list = tokio::time::timeout(Duration::from_secs(10), fut)
        .await
        .map_err(|_| ProviderError::new("timed out listing models", true))??;
    list.sort();
    list.dedup();
    Ok(list)
}

async fn openai_compat(a: &Account) -> Result<Vec<String>, ProviderError> {
    let base = a
        .base_url
        .as_deref()
        .or(a.kind.default_base_url())
        .unwrap_or_default();
    let client = reqwest::Client::new();
    let mut req = client.get(format!("{}/models", base.trim_end_matches('/')));
    if let Some(k) = secret_str(&a.api_key) {
        // Bearer for OpenAI-style servers, x-api-key for Anthropic-style gateways.
        req = req
            .bearer_auth(k)
            .header("x-api-key", k)
            .header("anthropic-version", "2023-06-01");
    }
    let list: OpenAiList = req
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| ProviderError::classify(e.to_string()))?
        .json()
        .await
        .map_err(|e| ProviderError::classify(e.to_string()))?;
    Ok(list.data.into_iter().map(|m| m.id).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_list_parses() {
        let l: OpenAiList =
            serde_json::from_str(r#"{"object":"list","data":[{"id":"a"},{"id":"b"}]}"#).unwrap();
        assert_eq!(l.data.len(), 2);
    }
}
