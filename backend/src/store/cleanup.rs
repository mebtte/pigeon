use std::fs;
use std::io;
use std::path::Path;

use rusqlite::{OptionalExtension, TransactionBehavior, params};

use super::{
    Result, SHARE_ID_BYTES, State, Store, StoreError, is_hex, is_share_code, unix_seconds,
};

const CLEANUP_BATCH: usize = 128;

impl Store {
    pub fn delete_share(&self, code: &str, owner: &str) -> Result<()> {
        if !is_share_code(code) {
            return Err(StoreError::NotFound);
        }
        let mut state = self.inner.lock();
        let tx = state
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let id = tx
            .query_row(
                "UPDATE shares SET remaining_downloads = 0
                 WHERE id = (SELECT share_id FROM share_codes WHERE code = ?1)
                    AND uploaded_by = ?2
                 RETURNING id",
                params![code, owner],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or(StoreError::NotFound)?;
        if !is_hex(&id, SHARE_ID_BYTES) {
            return Err(StoreError::NotFound);
        }
        // Close the share and invalidate every outstanding URL in one commit.
        tx.execute("DELETE FROM download_links WHERE share_id = ?1", [&id])?;
        tx.commit()?;

        if !state.active_downloads.contains_key(&id)
            && self.remove_share_file_and_row(&state, &id).is_err()
        {
            tracing::warn!("Revoked share cleanup failed and will be retried");
        }
        Ok(())
    }

    pub fn cleanup(&self) -> Result<()> {
        self.cleanup_at(unix_seconds()?)
    }

    pub(super) fn cleanup_at(&self, now: i64) -> Result<()> {
        self.inner
            .lock()
            .conn
            .execute("DELETE FROM download_links WHERE expires_at <= ?1", [now])?;

        // Walk in batches without holding the lock between candidates. A file
        // temporarily in use or inaccessible won't block later shares.
        let mut after = String::new();
        let mut first_error = None;
        loop {
            let ids = {
                let state = self.inner.lock();
                let mut query = state.conn.prepare(
                    "SELECT id FROM shares WHERE id > ?2
                     AND (expires_at <= ?1 OR remaining_downloads = 0)
                     AND NOT EXISTS (
                         SELECT 1 FROM download_links WHERE share_id = shares.id AND expires_at > ?1
                     )
                     ORDER BY id LIMIT ?3",
                )?;
                let rows = query.query_map(params![now, after, CLEANUP_BATCH], |row| {
                    row.get::<_, String>(0)
                })?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            };
            let Some(last_id) = ids.last() else {
                break;
            };
            after.clone_from(last_id);
            for id in ids {
                if let Err(error) = self.cleanup_share(&id, now) {
                    first_error.get_or_insert(error);
                }
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    fn cleanup_share(&self, id: &str, now: i64) -> Result<()> {
        let state = self.inner.lock();
        if state.active_downloads.contains_key(id) {
            return Ok(());
        }
        let removable = state.conn.query_row(
            "SELECT EXISTS (
                SELECT 1 FROM shares WHERE id = ?1 AND (expires_at <= ?2 OR remaining_downloads = 0)
                AND NOT EXISTS (SELECT 1 FROM download_links WHERE share_id = ?1 AND expires_at > ?2)
            )",
            params![id, now],
            |row| row.get::<_, bool>(0),
        )?;
        if removable && is_hex(id, SHARE_ID_BYTES) {
            self.remove_share_file_and_row(&state, id)?;
        }
        Ok(())
    }

    fn remove_share_file_and_row(&self, state: &State, id: &str) -> Result<()> {
        // If we restart after unlinking, a missing file counts as already removed.
        remove_file(&self.inner.files_dir.join(id))?;
        state
            .conn
            .execute("DELETE FROM shares WHERE id = ?1", [id])?;
        Ok(())
    }

    pub(super) fn recover(&self) -> Result<()> {
        for entry in fs::read_dir(&self.inner.tmp_dir)? {
            let entry = entry?;
            if owned_filename(&entry) {
                remove_file(&entry.path())?;
            }
        }
        for entry in fs::read_dir(&self.inner.files_dir)? {
            let entry = entry?;
            if !owned_filename(&entry) {
                continue;
            }
            let filename = entry.file_name();
            let Some(id) = filename.to_str() else {
                continue;
            };
            let exists = self.inner.lock().conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM shares WHERE id = ?1)",
                [id],
                |row| row.get::<_, bool>(0),
            )?;
            if !exists {
                remove_file(&entry.path())?;
            }
        }
        match self.cleanup_at(unix_seconds()?) {
            Err(StoreError::Io(_)) => {
                tracing::warn!("Startup file cleanup failed and will be retried");
                Ok(())
            }
            result => result,
        }
    }
}

fn owned_filename(entry: &fs::DirEntry) -> bool {
    entry
        .file_name()
        .to_str()
        .is_some_and(|name| is_hex(name, SHARE_ID_BYTES))
}

fn remove_file(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}
