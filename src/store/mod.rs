//! PostgreSQL persistence. Every payload column is sealed by the [`Vault`].

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use sqlx::{PgPool, Row, postgres::PgPoolOptions};
use uuid::Uuid;

use crate::crypto::{KdfParams, Vault, VaultHeader};
use crate::model::*;

#[derive(Clone)]
pub struct Store {
    pool: PgPool,
}

impl Store {
    pub async fn connect(url: &str) -> Result<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect(url)
            .await
            .context("cannot connect to PostgreSQL (check OMNICRAB_DATABASE_URL)")?;
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .context("running migrations")?;
        Ok(Self { pool })
    }

    pub async fn vault_header(&self) -> Result<Option<VaultHeader>> {
        let row =
            sqlx::query("SELECT kdf_salt, kdf_params, wrapped_dek FROM vault_meta WHERE id = 1")
                .fetch_optional(&self.pool)
                .await?;
        let Some(r) = row else { return Ok(None) };
        let params: serde_json::Value = r.get("kdf_params");
        Ok(Some(VaultHeader {
            kdf_salt: r.get("kdf_salt"),
            kdf_params: serde_json::from_value::<KdfParams>(params)?,
            wrapped_dek: r.get("wrapped_dek"),
        }))
    }

    pub async fn init_vault(&self, password: &str) -> Result<Arc<Vault>> {
        if self.vault_header().await?.is_some() {
            bail!("vault already initialized");
        }
        let (vault, header) = Vault::create(password)?;
        // DO NOTHING: a concurrent init must never overwrite an existing wrapped DEK.
        let done = sqlx::query(
            "INSERT INTO vault_meta (id, kdf_salt, kdf_params, wrapped_dek) VALUES (1, $1, $2, $3)
             ON CONFLICT (id) DO NOTHING",
        )
        .bind(&header.kdf_salt)
        .bind(serde_json::to_value(header.kdf_params)?)
        .bind(&header.wrapped_dek)
        .execute(&self.pool)
        .await?;
        if done.rows_affected() == 0 {
            bail!("vault already initialized");
        }
        Ok(Arc::new(vault))
    }

    pub async fn save_header(&self, h: &VaultHeader) -> Result<()> {
        sqlx::query(
            "INSERT INTO vault_meta (id, kdf_salt, kdf_params, wrapped_dek) VALUES (1, $1, $2, $3)
             ON CONFLICT (id) DO UPDATE SET kdf_salt = $1, kdf_params = $2, wrapped_dek = $3",
        )
        .bind(&h.kdf_salt)
        .bind(serde_json::to_value(h.kdf_params)?)
        .bind(&h.wrapped_dek)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn upsert_account(&self, v: &Vault, a: &Account) -> Result<Uuid> {
        let id = if a.id.is_nil() { Uuid::new_v4() } else { a.id };
        let blob = v.seal_json("accounts", "blob", &id, a)?;
        let idx = v.blind_index("account_name", &a.name);
        sqlx::query(
            "INSERT INTO accounts (id, name_idx, blob) VALUES ($1, $2, $3)
             ON CONFLICT (id) DO UPDATE SET name_idx = $2, blob = $3",
        )
        .bind(id)
        .bind(idx)
        .bind(blob)
        .execute(&self.pool)
        .await
        .map_err(
            |e| match e.as_database_error().and_then(|d| d.code()).as_deref() {
                Some("23505") => anyhow::anyhow!("an account named '{}' already exists", a.name),
                _ => e.into(),
            },
        )?;
        Ok(id)
    }

    pub async fn list_accounts(&self, v: &Vault) -> Result<Vec<Account>> {
        let rows = sqlx::query("SELECT id, blob FROM accounts ORDER BY created_at")
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter()
            .map(|r| {
                let id: Uuid = r.get("id");
                let mut a: Account = v.open_json(
                    "accounts",
                    "blob",
                    &id,
                    r.get::<Vec<u8>, _>("blob").as_slice(),
                )?;
                a.id = id;
                Ok(a)
            })
            .collect()
    }

    pub async fn delete_account(&self, id: Uuid) -> Result<()> {
        sqlx::query("DELETE FROM accounts WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn upsert_agent(&self, v: &Vault, a: &Agent) -> Result<Uuid> {
        let id = if a.id.is_nil() { Uuid::new_v4() } else { a.id };
        let blob = v.seal_json("agents", "blob", &id, a)?;
        sqlx::query("INSERT INTO agents (id, blob) VALUES ($1, $2) ON CONFLICT (id) DO UPDATE SET blob = $2")
            .bind(id)
            .bind(blob)
            .execute(&self.pool)
            .await?;
        Ok(id)
    }

    pub async fn list_agents(&self, v: &Vault) -> Result<Vec<Agent>> {
        let rows = sqlx::query("SELECT id, blob FROM agents ORDER BY created_at")
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter()
            .map(|r| {
                let id: Uuid = r.get("id");
                let mut a: Agent = v.open_json(
                    "agents",
                    "blob",
                    &id,
                    r.get::<Vec<u8>, _>("blob").as_slice(),
                )?;
                a.id = id;
                Ok(a)
            })
            .collect()
    }

    pub async fn delete_agent(&self, id: Uuid) -> Result<()> {
        sqlx::query("DELETE FROM agents WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn create_session(&self, v: &Vault, id: Uuid, title: &str) -> Result<Uuid> {
        let blob = v.seal_json(
            "sessions",
            "blob",
            &id,
            &SessionMeta {
                id,
                title: title.into(),
                created_at: Utc::now(),
            },
        )?;
        sqlx::query("INSERT INTO sessions (id, blob) VALUES ($1, $2)")
            .bind(id)
            .bind(blob)
            .execute(&self.pool)
            .await?;
        Ok(id)
    }

    pub async fn rename_session(&self, v: &Vault, id: Uuid, title: &str) -> Result<()> {
        let blob = v.seal_json(
            "sessions",
            "blob",
            &id,
            &SessionMeta {
                id,
                title: title.into(),
                created_at: Utc::now(),
            },
        )?;
        sqlx::query("UPDATE sessions SET blob = $2 WHERE id = $1")
            .bind(id)
            .bind(blob)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn list_sessions(&self, v: &Vault) -> Result<Vec<SessionMeta>> {
        let rows =
            sqlx::query("SELECT id, blob, created_at FROM sessions ORDER BY created_at DESC")
                .fetch_all(&self.pool)
                .await?;
        rows.into_iter()
            .map(|r| {
                let id: Uuid = r.get("id");
                let mut s: SessionMeta = v.open_json(
                    "sessions",
                    "blob",
                    &id,
                    r.get::<Vec<u8>, _>("blob").as_slice(),
                )?;
                s.id = id;
                s.created_at = r.get::<DateTime<Utc>, _>("created_at");
                Ok(s)
            })
            .collect()
    }

    pub async fn delete_session(&self, id: Uuid) -> Result<()> {
        sqlx::query("DELETE FROM sessions WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn append_message(&self, v: &Vault, session: Uuid, m: &Message) -> Result<()> {
        let id = Uuid::new_v4();
        let blob = v.seal_json("messages", "blob", &id, m)?;
        sqlx::query(
            "INSERT INTO messages (id, session_id, seq, blob)
             SELECT $1, $2, COALESCE(MAX(seq), 0) + 1, $3 FROM messages WHERE session_id = $2",
        )
        .bind(id)
        .bind(session)
        .bind(blob)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn load_messages(&self, v: &Vault, session: Uuid) -> Result<Vec<Message>> {
        let rows = sqlx::query("SELECT id, blob FROM messages WHERE session_id = $1 ORDER BY seq")
            .bind(session)
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter()
            .map(|r| {
                let id: Uuid = r.get("id");
                Ok(v.open_json(
                    "messages",
                    "blob",
                    &id,
                    r.get::<Vec<u8>, _>("blob").as_slice(),
                )?)
            })
            .collect()
    }

    /// Stable row id per setting key, so the AAD stays unique without a second column.
    fn setting_id(key: &str) -> Uuid {
        use sha2::{Digest, Sha256};
        let h = Sha256::digest(key.as_bytes());
        let mut b = [0u8; 16];
        b.copy_from_slice(&h[..16]);
        Uuid::from_bytes(b)
    }

    pub async fn load_setting<T: serde::de::DeserializeOwned>(
        &self,
        v: &Vault,
        key: &str,
    ) -> Result<Option<T>> {
        let id = Self::setting_id(key);
        let row = sqlx::query("SELECT blob FROM settings WHERE key = $1")
            .bind(key)
            .fetch_optional(&self.pool)
            .await?;
        row.map(|r| {
            Ok(v.open_json(
                "settings",
                "blob",
                &id,
                r.get::<Vec<u8>, _>("blob").as_slice(),
            )?)
        })
        .transpose()
    }

    pub async fn save_setting<T: serde::Serialize>(
        &self,
        v: &Vault,
        key: &str,
        value: &T,
    ) -> Result<()> {
        let id = Self::setting_id(key);
        let blob = v.seal_json("settings", "blob", &id, value)?;
        sqlx::query(
            "INSERT INTO settings (key, id, blob) VALUES ($1, $2, $3)
             ON CONFLICT (key) DO UPDATE SET id = $2, blob = $3, updated_at = now()",
        )
        .bind(key)
        .bind(id)
        .bind(blob)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn record_usage(&self, v: &Vault, account: Uuid, e: &UsageEvent) -> Result<()> {
        let id = Uuid::new_v4();
        let blob = v.seal_json("usage_events", "blob", &id, e)?;
        sqlx::query(
            "INSERT INTO usage_events (id, account_id, blob, created_at) VALUES ($1, $2, $3, $4)",
        )
        .bind(id)
        .bind(account)
        .bind(blob)
        .bind(e.at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Events since `since` (or all). Aggregation happens client-side after decryption.
    pub async fn load_usage(
        &self,
        v: &Vault,
        since: Option<DateTime<Utc>>,
    ) -> Result<Vec<UsageEvent>> {
        let rows = sqlx::query(
            "SELECT id, account_id, blob, created_at FROM usage_events
             WHERE ($1::timestamptz IS NULL OR created_at >= $1) ORDER BY created_at",
        )
        .bind(since)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|r| {
                let id: Uuid = r.get("id");
                let mut e: UsageEvent = v.open_json(
                    "usage_events",
                    "blob",
                    &id,
                    r.get::<Vec<u8>, _>("blob").as_slice(),
                )?;
                e.account_id = r.get("account_id");
                e.at = r.get("created_at");
                Ok(e)
            })
            .collect()
    }
}
