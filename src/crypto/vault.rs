//! Key hierarchy: password --Argon2id--> KEK --wraps--> DEK --seals--> records.
//!
//! A wrong password is detected when the wrapped DEK fails AEAD verification.

use argon2::{Algorithm, Argon2, Params, Version};
use hmac::{Hmac, Mac, digest::KeyInit};
use sha2::Sha256;
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

use super::{
    CryptoError,
    cipher::{self, KEY_LEN},
};

const WRAP_AAD: &[u8] = b"vault_meta:wrapped_dek";

#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub struct KdfParams {
    pub m_kib: u32,
    pub t: u32,
    pub p: u32,
}

impl Default for KdfParams {
    fn default() -> Self {
        Self {
            m_kib: 64 * 1024,
            t: 3,
            p: 1,
        }
    }
}

/// Persisted (non-secret) vault header.
#[derive(Clone)]
pub struct VaultHeader {
    pub kdf_salt: Vec<u8>,
    pub kdf_params: KdfParams,
    pub wrapped_dek: Vec<u8>,
}

/// Unlocked vault: holds the DEK in memory only.
#[derive(ZeroizeOnDrop)]
pub struct Vault {
    dek: [u8; KEY_LEN],
}

fn derive_kek(
    password: &str,
    salt: &[u8],
    p: KdfParams,
) -> Result<Zeroizing<[u8; KEY_LEN]>, CryptoError> {
    let params = Params::new(p.m_kib, p.t, p.p, Some(KEY_LEN)).map_err(|_| CryptoError::Kdf)?;
    let mut out = Zeroizing::new([0u8; KEY_LEN]);
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into(password.as_bytes(), salt, &mut *out)
        .map_err(|_| CryptoError::Kdf)?;
    Ok(out)
}

impl Vault {
    /// Create a brand-new vault with a random DEK.
    pub fn create(password: &str) -> Result<(Self, VaultHeader), CryptoError> {
        Self::create_with(password, KdfParams::default())
    }

    pub fn create_with(
        password: &str,
        params: KdfParams,
    ) -> Result<(Self, VaultHeader), CryptoError> {
        let dek = cipher::random_bytes::<KEY_LEN>();
        let vault = Self { dek };
        let header = vault.wrap(password, params)?;
        Ok((vault, header))
    }

    pub fn unlock(password: &str, header: &VaultHeader) -> Result<Self, CryptoError> {
        let kek = derive_kek(password, &header.kdf_salt, header.kdf_params)?;
        let dek = cipher::open(&kek, WRAP_AAD, &header.wrapped_dek)
            .map_err(|_| CryptoError::WrongPassword)?;
        let dek: [u8; KEY_LEN] = dek
            .as_slice()
            .try_into()
            .map_err(|_| CryptoError::Malformed)?;
        Ok(Self { dek })
    }

    /// Re-wrap the DEK under a new password. Stored records stay valid.
    pub fn wrap(&self, password: &str, params: KdfParams) -> Result<VaultHeader, CryptoError> {
        let salt = cipher::random_bytes::<16>();
        let kek = derive_kek(password, &salt, params)?;
        let wrapped_dek = cipher::seal(&kek, WRAP_AAD, &self.dek)?;
        Ok(VaultHeader {
            kdf_salt: salt.to_vec(),
            kdf_params: params,
            wrapped_dek,
        })
    }

    /// Seal a record. AAD binds ciphertext to `table:column:row_id`.
    pub fn seal(
        &self,
        table: &str,
        column: &str,
        row: &uuid::Uuid,
        pt: &[u8],
    ) -> Result<Vec<u8>, CryptoError> {
        cipher::seal(&self.dek, &aad(table, column, row), pt)
    }

    pub fn open(
        &self,
        table: &str,
        column: &str,
        row: &uuid::Uuid,
        blob: &[u8],
    ) -> Result<Zeroizing<Vec<u8>>, CryptoError> {
        cipher::open(&self.dek, &aad(table, column, row), blob)
    }

    pub fn seal_json<T: serde::Serialize>(
        &self,
        table: &str,
        column: &str,
        row: &uuid::Uuid,
        v: &T,
    ) -> Result<Vec<u8>, CryptoError> {
        let mut pt = serde_json::to_vec(v).map_err(|_| CryptoError::Encode)?;
        let out = self.seal(table, column, row, &pt);
        pt.zeroize();
        out
    }

    pub fn open_json<T: serde::de::DeserializeOwned>(
        &self,
        table: &str,
        column: &str,
        row: &uuid::Uuid,
        blob: &[u8],
    ) -> Result<T, CryptoError> {
        let pt = self.open(table, column, row, blob)?;
        serde_json::from_slice(&pt).map_err(|_| CryptoError::Encode)
    }

    /// Deterministic keyed hash for equality lookups (blind index).
    pub fn blind_index(&self, label: &str, value: &str) -> Vec<u8> {
        // Domain-separated subkey: HMAC(DEK, "blind-index-key").
        let mut kdf = <Hmac<Sha256> as KeyInit>::new_from_slice(&self.dek).expect("hmac key");
        kdf.update(b"omnicrab/blind-index-key");
        let mut sub = Zeroizing::new([0u8; 32]);
        sub.copy_from_slice(&kdf.finalize().into_bytes());
        let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(&sub[..]).expect("hmac key");
        mac.update(label.as_bytes());
        mac.update(&[0]);
        mac.update(value.trim().to_lowercase().as_bytes());
        mac.finalize().into_bytes().to_vec()
    }
}

fn aad(table: &str, column: &str, row: &uuid::Uuid) -> Vec<u8> {
    format!("{table}:{column}:{row}").into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Cheap params so tests are fast.
    const P: KdfParams = KdfParams {
        m_kib: 8,
        t: 1,
        p: 1,
    };

    #[test]
    fn unlock_roundtrip() {
        let (v, h) = Vault::create_with("pw", P).unwrap();
        let id = uuid::Uuid::new_v4();
        let blob = v.seal("accounts", "blob", &id, b"secret").unwrap();
        let v2 = Vault::unlock("pw", &h).unwrap();
        assert_eq!(
            &**v2.open("accounts", "blob", &id, &blob).unwrap(),
            b"secret"
        );
    }

    #[test]
    fn wrong_password_rejected() {
        let (_, h) = Vault::create_with("pw", P).unwrap();
        assert!(matches!(
            Vault::unlock("nope", &h),
            Err(CryptoError::WrongPassword)
        ));
    }

    #[test]
    fn row_binding() {
        let (v, _) = Vault::create_with("pw", P).unwrap();
        let (a, b) = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
        let blob = v.seal("accounts", "blob", &a, b"x").unwrap();
        assert!(v.open("accounts", "blob", &b, &blob).is_err());
        assert!(v.open("agents", "blob", &a, &blob).is_err());
    }

    #[test]
    fn password_change_preserves_data() {
        let (v, _) = Vault::create_with("old", P).unwrap();
        let id = uuid::Uuid::new_v4();
        let blob = v.seal("t", "c", &id, b"data").unwrap();
        let h2 = v.wrap("new", P).unwrap();
        assert!(Vault::unlock("old", &h2).is_err());
        let v2 = Vault::unlock("new", &h2).unwrap();
        assert_eq!(&**v2.open("t", "c", &id, &blob).unwrap(), b"data");
    }

    #[test]
    fn blind_index_stable_and_normalized() {
        let (v, _) = Vault::create_with("pw", P).unwrap();
        assert_eq!(
            v.blind_index("acct", "Work "),
            v.blind_index("acct", "work")
        );
        assert_ne!(v.blind_index("acct", "a"), v.blind_index("acct", "b"));
    }
}
