//! AEAD primitives: XChaCha20-Poly1305 with per-record AAD.
//!
//! Wire format: `version(1B) || nonce(24B) || ciphertext+tag`.

use chacha20poly1305::{
    XChaCha20Poly1305, XNonce,
    aead::{Aead, KeyInit, Payload},
};
use rand::RngExt;
use zeroize::Zeroizing;

use super::CryptoError;

pub const FORMAT_VERSION: u8 = 1;
pub const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 24;

pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut buf = [0u8; N];
    rand::rng().fill(&mut buf[..]);
    buf
}

pub fn seal(key: &[u8; KEY_LEN], aad: &[u8], plaintext: &[u8]) -> Result<Vec<u8>, CryptoError> {
    let cipher = XChaCha20Poly1305::new(key.into());
    let nonce_bytes = random_bytes::<NONCE_LEN>();
    let ct = cipher
        .encrypt(
            &XNonce::try_from(&nonce_bytes[..]).map_err(|_| CryptoError::Encrypt)?,
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .map_err(|_| CryptoError::Encrypt)?;
    let mut out = Vec::with_capacity(1 + NONCE_LEN + ct.len());
    out.push(FORMAT_VERSION);
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ct);
    Ok(out)
}

pub fn open(
    key: &[u8; KEY_LEN],
    aad: &[u8],
    blob: &[u8],
) -> Result<Zeroizing<Vec<u8>>, CryptoError> {
    if blob.len() < 1 + NONCE_LEN + 16 {
        return Err(CryptoError::Malformed);
    }
    if blob[0] != FORMAT_VERSION {
        return Err(CryptoError::UnsupportedVersion(blob[0]));
    }
    let (nonce, ct) = blob[1..].split_at(NONCE_LEN);
    let cipher = XChaCha20Poly1305::new(key.into());
    cipher
        .decrypt(
            &XNonce::try_from(nonce).map_err(|_| CryptoError::Malformed)?,
            Payload { msg: ct, aad },
        )
        .map(Zeroizing::new)
        .map_err(|_| CryptoError::Decrypt)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let k = random_bytes::<32>();
        let blob = seal(&k, b"t:c:1", b"hello").unwrap();
        assert_eq!(&**open(&k, b"t:c:1", &blob).unwrap(), b"hello");
    }

    #[test]
    fn wrong_aad_fails() {
        let k = random_bytes::<32>();
        let blob = seal(&k, b"t:c:1", b"hello").unwrap();
        assert!(open(&k, b"t:c:2", &blob).is_err());
    }

    #[test]
    fn wrong_key_fails() {
        let blob = seal(&random_bytes::<32>(), b"a", b"hello").unwrap();
        assert!(open(&random_bytes::<32>(), b"a", &blob).is_err());
    }

    #[test]
    fn tamper_fails() {
        let k = random_bytes::<32>();
        let mut blob = seal(&k, b"a", b"hello").unwrap();
        *blob.last_mut().unwrap() ^= 1;
        assert!(open(&k, b"a", &blob).is_err());
    }

    #[test]
    fn nonces_differ() {
        let k = random_bytes::<32>();
        assert_ne!(seal(&k, b"a", b"x").unwrap(), seal(&k, b"a", b"x").unwrap());
    }
}
