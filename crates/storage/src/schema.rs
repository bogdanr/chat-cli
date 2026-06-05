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
"#;
