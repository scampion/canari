-- All timestamps are unix epoch seconds (INTEGER), always UTC.
-- Storing them as integers keeps `alert_after <= ?` comparisons exact and
-- indexable, which the alert loop depends on.

CREATE TABLE checks (
    id               INTEGER PRIMARY KEY,
    uuid             TEXT    NOT NULL UNIQUE,
    name             TEXT    NOT NULL,
    description      TEXT    NOT NULL DEFAULT '',
    tags             TEXT    NOT NULL DEFAULT '',
    -- 'simple': expect a ping every period_s seconds.
    -- 'cron':   expect a ping after every occurrence of cron_expr in tz.
    kind             TEXT    NOT NULL DEFAULT 'simple' CHECK (kind IN ('simple', 'cron')),
    period_s         INTEGER NOT NULL DEFAULT 86400,
    grace_s          INTEGER NOT NULL DEFAULT 3600,
    cron_expr        TEXT,
    tz               TEXT    NOT NULL DEFAULT 'UTC',
    -- 'paused' is a status, not a flag: a paused check is simply never late.
    status           TEXT    NOT NULL DEFAULT 'new'
                             CHECK (status IN ('new', 'up', 'grace', 'down', 'paused')),
    last_ping_at     INTEGER,
    -- Set by a /start ping, cleared by the matching success/fail ping.
    last_start_at    INTEGER,
    last_duration_ms INTEGER,
    -- The instant this check becomes late. The alert loop scans on this column
    -- alone rather than recomputing schedules for every check on every tick.
    alert_after      INTEGER,
    n_pings          INTEGER NOT NULL DEFAULT 0,
    created_at       INTEGER NOT NULL,
    updated_at       INTEGER NOT NULL
);

-- Partial index: only checks that can go late are ever scanned.
CREATE INDEX checks_alert_after ON checks (alert_after)
    WHERE status IN ('up', 'grace');

CREATE TABLE pings (
    id          INTEGER PRIMARY KEY,
    check_id    INTEGER NOT NULL REFERENCES checks (id) ON DELETE CASCADE,
    -- Per-check sequence number, as shown in the UI ("ping #42").
    n           INTEGER NOT NULL,
    ts          INTEGER NOT NULL,
    kind        TEXT    NOT NULL CHECK (kind IN ('success', 'start', 'fail', 'log')),
    exit_code   INTEGER,
    duration_ms INTEGER,
    remote_addr TEXT,
    user_agent  TEXT,
    method      TEXT,
    body        TEXT
);

CREATE INDEX pings_check_recent ON pings (check_id, id DESC);

CREATE TABLE channels (
    id         INTEGER PRIMARY KEY,
    kind       TEXT    NOT NULL CHECK (kind IN ('webhook', 'ntfy')),
    name       TEXT    NOT NULL,
    -- Kind-specific settings as JSON (url, headers, topic, priority...).
    config     TEXT    NOT NULL DEFAULT '{}',
    enabled    INTEGER NOT NULL DEFAULT 1,
    created_at INTEGER NOT NULL
);

CREATE TABLE check_channels (
    check_id   INTEGER NOT NULL REFERENCES checks (id) ON DELETE CASCADE,
    channel_id INTEGER NOT NULL REFERENCES channels (id) ON DELETE CASCADE,
    PRIMARY KEY (check_id, channel_id)
) WITHOUT ROWID;

-- One row per delivery attempt. Doubles as the guard that keeps the alert loop
-- from re-notifying on every tick: a transition is notified once.
CREATE TABLE notifications (
    id         INTEGER PRIMARY KEY,
    check_id   INTEGER NOT NULL REFERENCES checks (id) ON DELETE CASCADE,
    channel_id INTEGER NOT NULL REFERENCES channels (id) ON DELETE CASCADE,
    ts         INTEGER NOT NULL,
    -- The check status that triggered this notification ('down' or 'up').
    reason     TEXT    NOT NULL,
    status     TEXT    NOT NULL CHECK (status IN ('sent', 'failed')),
    error      TEXT
);

CREATE INDEX notifications_check_recent ON notifications (check_id, id DESC);

CREATE TABLE api_keys (
    id           INTEGER PRIMARY KEY,
    name         TEXT    NOT NULL,
    hash         TEXT    NOT NULL UNIQUE,
    read_only    INTEGER NOT NULL DEFAULT 0,
    created_at   INTEGER NOT NULL,
    last_used_at INTEGER
);
