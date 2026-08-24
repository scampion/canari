-- Single-operator authentication: one password, stored hashed, plus
-- server-side sessions so logging out actually revokes access.

CREATE TABLE settings (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE sessions (
    -- SHA-256 of the cookie value, never the value itself: a stolen database
    -- must not hand out live sessions.
    token_hash TEXT PRIMARY KEY,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL
);

CREATE INDEX sessions_expiry ON sessions (expires_at);
