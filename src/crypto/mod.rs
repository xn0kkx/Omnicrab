pub mod cipher;
pub mod vault;

pub use vault::{KdfParams, Vault, VaultHeader};

#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    #[error("encryption failed")]
    Encrypt,
    #[error("decryption failed (corrupt data or wrong key)")]
    Decrypt,
    #[error("malformed ciphertext")]
    Malformed,
    #[error("unsupported ciphertext version {0}")]
    UnsupportedVersion(u8),
    #[error("key derivation failed")]
    Kdf,
    #[error("wrong password")]
    WrongPassword,
    #[error("serialization error")]
    Encode,
}
