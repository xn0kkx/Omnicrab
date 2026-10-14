//! Detect a local Ollama server and the models it serves.

use serde::Deserialize;

#[derive(Deserialize)]
struct Tags {
    models: Vec<Tag>,
}
#[derive(Deserialize)]
struct Tag {
    name: String,
}

pub async fn list_models(base_url: &str) -> anyhow::Result<Vec<String>> {
    let url = format!("{}/api/tags", base_url.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()?;
    let tags: Tags = client
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(tags.models.into_iter().map(|m| m.name).collect())
}
