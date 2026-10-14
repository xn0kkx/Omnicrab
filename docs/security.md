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

omnicrab never touches OAuth tokens of vendor subscriptions. CLI-backed accounts run the vendor binary, which uses its own credential store.

CLI subprocesses run in the **project folder** omnicrab was opened in (not in a temp directory). Executors get edit and shell access there, the planner and chat are read-only, and `--read-only` removes write access altogether. Treat an executor like any coding agent working in that checkout: it can change files and run commands, so use version control. `/` and the home directory are refused as workspaces unless passed explicitly with `--workspace`.

## The `.omni` folder

`<project>/.omni/` holds instructions, memory, settings, project agents/commands, approved plans and session transcripts. Each file is sealed like a database record (XChaCha20-Poly1305, vault key) with AAD `omni:<project id>:<kind>:<file id>`: a file copied to another kind, file id or project fails to decrypt, and so does one made with another vault. Files are written atomically with mode 0600 in a 0700 directory. Plaintext: `project.toml` (random project id and format version), file names, sizes and timestamps. `/omni edit` decrypts one document into a 0700 temp directory for the editor and deletes it afterwards; a rejected edit is kept in `/tmp/omnicrab-<doc>.rejected` so you do not lose it. The CLI agents cannot read the ciphertext, and Claude Code is launched with `Edit`/`Write` on `.omni/**` denied; other CLIs are not restricted, but any tampering is detected on the next read.

## Operational advice

- **Back up the database and remember the password.** There is no recovery path.
- Use a dedicated PostgreSQL role/database and TLS if the DB is not local.
- Use a strong passphrase; Argon2id parameters are stored per vault and can be raised in code for new vaults.
- Report security issues privately to the repository owner.
