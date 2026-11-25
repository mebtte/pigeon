use std::path::Path;

use rusqlite::Connection;

use super::{Result, private_file, share_codes};

pub(super) fn open(path: &Path) -> Result<Connection> {
    // Create the database with private permissions before SQLite opens it.
    private_file(path, false)?;
    let mut conn = Connection::open(path)?;
    conn.execute_batch(
        "PRAGMA foreign_keys = ON;
         PRAGMA journal_mode = WAL;
         PRAGMA synchronous = FULL;

         CREATE TABLE IF NOT EXISTS shares (
             id TEXT PRIMARY KEY,
             filename TEXT NOT NULL,
             size_bytes INTEGER NOT NULL CHECK (size_bytes >= 0),
             uploaded_by TEXT NOT NULL,
             created_at INTEGER NOT NULL,
             expires_at INTEGER CHECK (expires_at > created_at),
             remaining_downloads INTEGER CHECK (remaining_downloads >= 0),
             password TEXT
         ) STRICT;

         CREATE TABLE IF NOT EXISTS share_codes (
             code TEXT PRIMARY KEY CHECK (length(code) = 6 AND code NOT GLOB '*[^A-Za-z0-9_-]*'),
             share_id TEXT UNIQUE REFERENCES shares(id) ON DELETE SET NULL
         ) STRICT;

         CREATE TABLE IF NOT EXISTS download_links (
             token_hash BLOB PRIMARY KEY CHECK (length(token_hash) = 32),
             share_id TEXT NOT NULL REFERENCES shares(id) ON DELETE CASCADE,
             expires_at INTEGER NOT NULL
         ) STRICT;

         CREATE INDEX IF NOT EXISTS share_expiry ON shares(expires_at)
             WHERE expires_at IS NOT NULL;
         CREATE INDEX IF NOT EXISTS exhausted_shares ON shares(id)
             WHERE remaining_downloads = 0;
         CREATE INDEX IF NOT EXISTS shares_by_owner_created
             ON shares(uploaded_by, created_at DESC, id DESC);
         DROP INDEX IF EXISTS shares_by_created;
         CREATE INDEX IF NOT EXISTS download_links_by_share
             ON download_links(share_id, expires_at);
         CREATE INDEX IF NOT EXISTS download_link_expiry ON download_links(expires_at);",
    )?;
    share_codes::backfill(&mut conn)?;
    Ok(conn)
}
