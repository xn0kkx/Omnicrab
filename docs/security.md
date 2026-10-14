# Security

## Goals and threat model

Protect stored data against anyone who obtains **the database, a backup or a SQL dump** (including a compromised DB role). The master password and the running process memory are trusted.

Out of scope: an attacker with code execution as your user while the vault is unlocked, keyloggers, a malicious provider, and traffic analysis of the plaintext columns listed below.

## Key hierarchy

```
master password
   │  Argon2id (m=64 MiB, t=3, p=1, random 16-byte salt)
   ▼
KEK (32 B, memory only) ──wraps──► DEK (32 B random) ──seals──► every record
                                      │
                                      └─ HMAC subkey ──► blind index for account names
```

- **KEK** exists only while deriving or re-wrapping and is zeroized immediately.
- **DEK** is generated once from the OS CSPRNG and stored only in wrapped form (`vault_meta.wrapped_dek`). It is held in memory while unlocked and zeroized on drop/lock.
- **Changing the password** (`/passwd`) re-wraps the DEK under a new KEK. No record is re-encrypted.
- **Wrong password** detection: unwrapping the DEK fails AEAD verification. There is no separate password hash to attack.

## Record encryption

Algorithm: **XChaCha20-Poly1305** (192-bit random nonce, so random nonces are safe at any volume).

```
version (1 B) || nonce (24 B) || ciphertext || tag (16 B)
```

- The version byte allows future algorithm migration.
- **AAD** = `"<table>:<column>:<row uuid>"`. A ciphertext copied to another row, column or table fails to decrypt, which prevents swapping attacks within the database.
- Payloads are JSON serialized, sealed, and the plaintext buffer is zeroized.

## Blind index

Account names must be unique but are encrypted. `name_idx = HMAC-SHA256(subkey, "account_name" ‖ 0 ‖ lowercase(trim(name)))` gives deterministic equality checks (a `UNIQUE` constraint) without revealing the name. It leaks only whether two names are equal.

## What is *not* encrypted

Row ids, foreign keys, `created_at`, the message `seq` and the HMAC blind index. An attacker with the database can therefore see how many accounts, sessions and messages exist and when requests happened, but not content, names, keys, models, token counts or costs.

## In-memory handling

- API keys are held as `secrecy::SecretString` and zeroized on drop; they are masked in the UI (`sk-…abcd`).
- Keys are exposed only to build a provider request or, when editing, to re-serialize the account.
- Logs never include keys or message bodies.
- Locking drops the vault; the unlock screen needs the password again.

## Subscription credentials

omnicrab never touches OAuth tokens of vendor subscriptions. CLI-backed accounts run the vendor binary, which uses its own credential store. CLI subprocesses run in a temp directory with tools disabled/read-only.

## Operational advice

- **Back up the database and remember the password.** There is no recovery path.
- Use a dedicated PostgreSQL role/database and TLS if the DB is not local.
- Use a strong passphrase; Argon2id parameters are stored per vault and can be raised in code for new vaults.
- Report security issues privately to the repository owner.
