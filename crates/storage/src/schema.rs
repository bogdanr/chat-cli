pub const V1: &str = r#"
CREATE TABLE IF NOT EXISTS accounts (
    id           TEXT PRIMARY KEY,
    platform     TEXT NOT NULL,
    display_name TEXT NOT NULL,
    avatar_path  TEXT,
    config_json  TEXT NOT NULL,
    created_at   INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS chats (
    id           TEXT NOT NULL,
    account_id   TEXT NOT NULL REFERENCES accounts(id),
    platform     TEXT NOT NULL,
    name         TEXT NOT NULL,
    avatar_path  TEXT,
    is_group     INTEGER NOT NULL DEFAULT 0,
    kind         TEXT NOT NULL DEFAULT 'direct',
    membership   TEXT NOT NULL DEFAULT 'joined',
    is_shared    INTEGER NOT NULL DEFAULT 0,
    unread_count INTEGER NOT NULL DEFAULT 0,
    muted        INTEGER NOT NULL DEFAULT 0,
    pinned       INTEGER NOT NULL DEFAULT 0,
    last_msg_at  INTEGER,
    last_preview TEXT,
    thread_id    TEXT,
    PRIMARY KEY (id, account_id)
);

CREATE TABLE IF NOT EXISTS messages (
    id              TEXT NOT NULL,
    chat_id         TEXT NOT NULL,
    account_id      TEXT NOT NULL,
    sender_id       TEXT NOT NULL,
    sender_name     TEXT NOT NULL,
    sender_avatar   TEXT,
    timestamp       INTEGER NOT NULL,
    edited_at       INTEGER,
    content_type    TEXT NOT NULL,
    content_text    TEXT,
    content_caption TEXT,
    media_id        TEXT,
    media_filename  TEXT,
    media_mime      TEXT,
    media_size      INTEGER,
    media_local     TEXT,
    media_thumbnail TEXT,
    reply_to_id     TEXT,
    thread_id       TEXT,
    is_from_me      INTEGER NOT NULL DEFAULT 0,
    mentions_me     INTEGER NOT NULL DEFAULT 0,
    platform_json   TEXT,
    PRIMARY KEY (id, account_id)
);
CREATE INDEX IF NOT EXISTS idx_messages_chat ON messages (chat_id, account_id, timestamp DESC);
CREATE INDEX IF NOT EXISTS idx_messages_thread ON messages (thread_id, account_id, timestamp);
CREATE INDEX IF NOT EXISTS idx_messages_text ON messages (content_text);

CREATE TABLE IF NOT EXISTS reactions (
    message_id  TEXT NOT NULL,
    account_id  TEXT NOT NULL,
    emoji       TEXT NOT NULL,
    sender_id   TEXT NOT NULL,
    PRIMARY KEY (message_id, account_id, emoji, sender_id)
);

CREATE TABLE IF NOT EXISTS receipts (
    message_id  TEXT NOT NULL,
    account_id  TEXT NOT NULL,
    sender_id   TEXT NOT NULL,
    kind        TEXT NOT NULL,
    at          INTEGER,
    PRIMARY KEY (message_id, account_id, sender_id, kind)
);

CREATE TABLE IF NOT EXISTS persons (
    id           TEXT PRIMARY KEY,
    display_name TEXT NOT NULL,
    avatar_path  TEXT
);

CREATE TABLE IF NOT EXISTS handles (
    person_id    TEXT NOT NULL REFERENCES persons(id),
    platform     TEXT NOT NULL,
    account_id   TEXT NOT NULL,
    platform_id  TEXT NOT NULL,
    display_name TEXT NOT NULL,
    PRIMARY KEY (account_id, platform_id)
);

CREATE TABLE IF NOT EXISTS settings (
    key        TEXT PRIMARY KEY,
    value_json TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS avatar_thumbnail_cache (
    cache_key     TEXT PRIMARY KEY,
    source_kind   TEXT NOT NULL,
    source_path   TEXT NOT NULL,
    source_mtime  INTEGER,
    source_size   INTEGER,
    image_format  TEXT NOT NULL,
    image_blob    BLOB NOT NULL,
    blob_bytes    INTEGER NOT NULL,
    cache_version INTEGER NOT NULL,
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL,
    last_accessed_at INTEGER NOT NULL,
    error_json    TEXT
);
CREATE INDEX IF NOT EXISTS idx_avatar_thumbnail_cache_accessed
    ON avatar_thumbnail_cache (last_accessed_at);

-- Per-thread read state. Threads are identified by their thread_id (the root
-- message id / Slack thread_ts). This is intentionally separate from
-- chats.unread_count so thread unread can be tracked without disturbing
-- chat-level activity ordering.
CREATE TABLE IF NOT EXISTS thread_reads (
    account_id           TEXT NOT NULL,
    thread_id            TEXT NOT NULL,
    unread_count         INTEGER NOT NULL DEFAULT 0,
    last_read_at         INTEGER,
    last_read_message_id TEXT,
    PRIMARY KEY (account_id, thread_id)
);
"#;
