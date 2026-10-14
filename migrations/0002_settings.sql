-- Encrypted application settings (routing roles and balance policy).
CREATE TABLE settings (
    key        text        PRIMARY KEY,
    id         uuid        NOT NULL UNIQUE,
    blob       bytea       NOT NULL,
    updated_at timestamptz NOT NULL DEFAULT now()
);
