-- All sensitive fields live inside application-layer encrypted `blob` columns.
CREATE TABLE vault_meta (
    id          smallint PRIMARY KEY CHECK (id = 1),
    kdf_salt    bytea       NOT NULL,
    kdf_params  jsonb       NOT NULL,
    wrapped_dek bytea       NOT NULL,
    created_at  timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE accounts (
    id         uuid PRIMARY KEY,
    name_idx   bytea       NOT NULL UNIQUE,
    blob       bytea       NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE agents (
    id         uuid PRIMARY KEY,
    blob       bytea       NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE sessions (
    id         uuid PRIMARY KEY,
    blob       bytea       NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE messages (
    id         uuid PRIMARY KEY,
    session_id uuid        NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    seq        integer     NOT NULL,
    blob       bytea       NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (session_id, seq)
);

CREATE TABLE usage_events (
    id         uuid PRIMARY KEY,
    account_id uuid        NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    blob       bytea       NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX usage_events_account_time ON usage_events (account_id, created_at);
