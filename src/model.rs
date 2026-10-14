//! Plaintext models. These are only ever persisted inside encrypted blobs.

use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    Anthropic,
    OpenAi,
    Gemini,
    OpenRouter,
    Groq,
    DeepSeek,
    Xai,
    Mistral,
    Ollama,
    /// Any OpenAI-compatible server (llama.cpp, LM Studio, vLLM...).
    OpenAiCompat,
    ClaudeCli,
    CodexCli,
    GeminiCli,
    AgyCli,
}

impl ProviderKind {
    pub const ALL: &'static [ProviderKind] = &[
        Self::Anthropic,
        Self::OpenAi,
        Self::Gemini,
        Self::OpenRouter,
        Self::Groq,
        Self::DeepSeek,
        Self::Xai,
        Self::Mistral,
        Self::Ollama,
        Self::OpenAiCompat,
        Self::ClaudeCli,
        Self::CodexCli,
        Self::GeminiCli,
        Self::AgyCli,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Anthropic => "Anthropic (API key)",
            Self::OpenAi => "OpenAI (API key)",
            Self::Gemini => "Google Gemini (API key)",
            Self::OpenRouter => "OpenRouter (API key)",
            Self::Groq => "Groq (API key)",
            Self::DeepSeek => "DeepSeek (API key)",
            Self::Xai => "xAI (API key)",
            Self::Mistral => "Mistral (API key)",
            Self::Ollama => "Ollama (local)",
            Self::OpenAiCompat => "OpenAI-compatible (local/custom URL)",
            Self::ClaudeCli => "Claude Code CLI (subscription)",
            Self::CodexCli => "Codex CLI (subscription)",
            Self::GeminiCli => "Gemini CLI (subscription)",
            Self::AgyCli => "Antigravity CLI - agy (subscription)",
        }
    }

    pub fn short(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAi => "openai",
            Self::Gemini => "gemini",
            Self::OpenRouter => "openrouter",
            Self::Groq => "groq",
            Self::DeepSeek => "deepseek",
            Self::Xai => "xai",
            Self::Mistral => "mistral",
            Self::Ollama => "ollama",
            Self::OpenAiCompat => "openai-compat",
            Self::ClaudeCli => "claude-cli",
            Self::CodexCli => "codex-cli",
            Self::GeminiCli => "gemini-cli",
            Self::AgyCli => "agy-cli",
        }
    }

    pub fn needs_api_key(self) -> bool {
        !matches!(
            self,
            Self::Ollama
                | Self::OpenAiCompat
                | Self::ClaudeCli
                | Self::CodexCli
                | Self::GeminiCli
                | Self::AgyCli
        )
    }

    pub fn is_cli(self) -> bool {
        matches!(
            self,
            Self::ClaudeCli | Self::CodexCli | Self::GeminiCli | Self::AgyCli
        )
    }

    pub fn is_local(self) -> bool {
        matches!(self, Self::Ollama | Self::OpenAiCompat)
    }

    pub fn default_model(self) -> &'static str {
        match self {
            Self::Anthropic => "claude-sonnet-5-5",
            Self::OpenAi => "gpt-5",
            Self::Gemini => "gemini-2.5-pro",
            Self::OpenRouter => "anthropic/claude-sonnet-5-5",
            Self::Groq => "llama-3.3-70b-versatile",
            Self::DeepSeek => "deepseek-chat",
            Self::Xai => "grok-4",
            Self::Mistral => "mistral-large-latest",
            Self::Ollama => "qwen3-coder:30b",
            Self::OpenAiCompat => "local-model",
            Self::ClaudeCli | Self::CodexCli | Self::GeminiCli | Self::AgyCli => "",
        }
    }

    pub fn default_base_url(self) -> Option<&'static str> {
        match self {
            Self::Ollama => Some("http://localhost:11434"),
            Self::OpenAiCompat => Some("http://localhost:8080/v1"),
            _ => None,
        }
    }

    /// Env var that points the CLI at a per-account credential directory.
    /// Only set for CLIs where this is documented and keeps logins separate.
    pub fn profile_env(self) -> Option<&'static str> {
        match self {
            Self::ClaudeCli => Some("CLAUDE_CONFIG_DIR"),
            _ => None,
        }
    }

    /// Arguments of the CLI's own interactive sign-in flow.
    pub fn login_args(self) -> &'static [&'static str] {
        match self {
            Self::ClaudeCli => &["auth", "login"],
            _ => &[],
        }
    }

    pub fn cli_binary(self) -> Option<&'static str> {
        match self {
            Self::ClaudeCli => Some("claude"),
            Self::CodexCli => Some("codex"),
            Self::GeminiCli => Some("gemini"),
            Self::AgyCli => Some("agy"),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Account {
    #[serde(skip, default = "Uuid::nil")]
    pub id: Uuid,
    pub name: String,
    pub kind: ProviderKind,
    #[serde(default, with = "opt_secret")]
    pub api_key: Option<SecretString>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub cli_path: Option<String>,
    /// Credential directory for a separate subscription login (see `ProviderKind::profile_env`).
    #[serde(default)]
    pub profile_dir: Option<String>,
    #[serde(default)]
    pub budget_usd_month: Option<f64>,
    #[serde(default = "yes")]
    pub enabled: bool,
}

fn yes() -> bool {
    true
}

mod opt_secret {
    use secrecy::{ExposeSecret, SecretString};
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &Option<SecretString>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(k) => s.serialize_some(k.expose_secret()),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<SecretString>, D::Error> {
        Ok(Option::<String>::deserialize(d)?.map(SecretString::from))
    }
}

/// `sk-…abcd` style masking for display.
pub fn mask_secret(s: &str) -> String {
    let n = s.chars().count();
    if n <= 8 {
        return "••••".into();
    }
    let head: String = s.chars().take(3).collect();
    let tail: String = s.chars().skip(n - 4).collect();
    format!("{head}…{tail}")
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Agent {
    #[serde(skip, default = "Uuid::nil")]
    pub id: Uuid,
    pub name: String,
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub system_prompt: String,
    pub account_id: Uuid,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub fallback_account_ids: Vec<Uuid>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionMeta {
    #[serde(skip, default = "Uuid::nil")]
    pub id: Uuid,
    pub title: String,
    #[serde(skip, default = "chrono::Utc::now")]
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
    System,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
    #[serde(default)]
    pub agent_name: Option<String>,
    #[serde(default)]
    pub account_name: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageStatus {
    Ok,
    Error,
    Cancelled,
    /// Request was skipped/redirected to a fallback account.
    FailedOver,
}

/// One subscription rate-limit window (e.g. the 5-hour or 7-day one).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct LimitWindow {
    /// Fraction of the window used (0.0..=1.0, may exceed 1.0 with overage).
    pub utilization: f64,
    pub resets_at: chrono::DateTime<chrono::Utc>,
}

/// Plan limits as reported by a provider CLI alongside a response.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RateLimits {
    #[serde(default)]
    pub five_hour: Option<LimitWindow>,
    #[serde(default)]
    pub seven_day: Option<LimitWindow>,
    /// "allowed", "allowed_warning" or "rejected".
    #[serde(default)]
    pub status: String,
}

impl RateLimits {
    pub fn is_empty(&self) -> bool {
        self.five_hour.is_none() && self.seven_day.is_none()
    }

    /// Fills in windows that `newer` knows about, keeping the rest.
    pub fn merge(&mut self, newer: RateLimits) {
        if newer.five_hour.is_some() {
            self.five_hour = newer.five_hour;
        }
        if newer.seven_day.is_some() {
            self.seven_day = newer.seven_day;
        }
        if !newer.status.is_empty() {
            self.status = newer.status;
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UsageEvent {
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
    pub latency_ms: u64,
    pub status: UsageStatus,
    /// Plan limits seen during this request (Claude CLI only).
    #[serde(default)]
    pub rate_limits: Option<RateLimits>,
    #[serde(skip, default = "chrono::Utc::now")]
    pub at: chrono::DateTime<chrono::Utc>,
    #[serde(skip, default = "Uuid::nil")]
    pub account_id: Uuid,
}

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::ExposeSecret;

    #[test]
    fn account_json_roundtrip_keeps_key() {
        let a = Account {
            id: Uuid::nil(),
            name: "Work".into(),
            kind: ProviderKind::Anthropic,
            api_key: Some(SecretString::from("sk-test-1234567890")),
            base_url: None,
            model: "m".into(),
            cli_path: None,
            profile_dir: None,
            budget_usd_month: Some(10.0),
            enabled: true,
        };
        let j = serde_json::to_vec(&a).unwrap();
        let b: Account = serde_json::from_slice(&j).unwrap();
        assert_eq!(b.api_key.unwrap().expose_secret(), "sk-test-1234567890");
    }

    #[test]
    fn masking() {
        assert_eq!(mask_secret("sk-ant-abcdefgh1234"), "sk-…1234");
        assert_eq!(mask_secret("short"), "••••");
    }
}
