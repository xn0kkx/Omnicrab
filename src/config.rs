//! Non-sensitive settings from `~/.config/omnicrab/config.toml`.
//! Secrets never live here; they are only stored encrypted in PostgreSQL.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Config {
    /// e.g. `postgres://user@localhost/omnicrab` (password via ~/.pgpass or PGPASSWORD).
    /// `OMNICRAB_DATABASE_URL` takes precedence.
    pub database_url: Option<String>,
    /// Extra / overriding prices: key is a model-name prefix.
    #[serde(default)]
    pub prices: BTreeMap<String, Price>,
}

/// USD per 1M tokens.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct Price {
    pub input: f64,
    pub output: f64,
}

pub fn config_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "omnicrab").map(|d| d.config_dir().join("config.toml"))
}

pub fn log_dir() -> PathBuf {
    directories::ProjectDirs::from("", "", "omnicrab")
        .and_then(|d| d.state_dir().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| std::env::temp_dir().join("omnicrab"))
}

impl Config {
    pub fn load() -> Result<Self> {
        let Some(path) = config_path() else {
            return Ok(Self::default());
        };
        match std::fs::read_to_string(&path) {
            Ok(s) => toml::from_str(&s).with_context(|| format!("parsing {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn database_url(&self) -> Result<String> {
        std::env::var("OMNICRAB_DATABASE_URL")
            .ok()
            .or_else(|| self.database_url.clone())
            .context(
                "no database configured: set OMNICRAB_DATABASE_URL or `database_url` in ~/.config/omnicrab/config.toml\n\
                 example: postgres://USER@localhost/omnicrab   (password via ~/.pgpass or PGPASSWORD)",
            )
    }
}
