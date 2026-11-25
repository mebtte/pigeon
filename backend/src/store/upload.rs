use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use rusqlite::{TransactionBehavior, params};

use super::{
    Inner, NewShare, Result, SHARE_ID_BYTES, Share, Store, StoreError, private_file, random_hex,
    share_codes, unix_seconds,
};

pub struct StagedUpload {
    path: PathBuf,
    file: Option<File>,
    moved: bool,
    inner: Arc<Inner>,
}

impl StagedUpload {
    /// The streaming caller owns this writer and closes it before finalizing.
    pub fn take_file(&mut self) -> File {
        self.file.take().expect("staging writer already taken")
    }
}

impl Drop for StagedUpload {
    fn drop(&mut self) {
        drop(self.file.take());
        if !self.moved {
            let _ = fs::remove_file(&self.path);
        }
    }
}

impl Store {
    pub fn start_upload(&self) -> Result<StagedUpload> {
        loop {
            let path = self.inner.tmp_dir.join(random_hex::<SHARE_ID_BYTES>()?);
            match private_file(&path, true) {
                Ok(file) => {
                    return Ok(StagedUpload {
                        path,
                        file: Some(file),
                        moved: false,
                        inner: Arc::clone(&self.inner),
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        }
    }

    pub fn finish_upload(&self, mut stage: StagedUpload, metadata: NewShare) -> Result<Share> {
        if !Arc::ptr_eq(&stage.inner, &self.inner)
            || metadata.filename.is_empty()
            || metadata.uploaded_by.is_empty()
            || metadata.download_limit == Some(0)
            || metadata.password.as_deref() == Some("")
        {
            return Err(StoreError::InvalidInput);
        }
        let size_bytes =
            i64::try_from(metadata.size_bytes).map_err(|_| StoreError::InvalidInput)?;
        let remaining_downloads = metadata
            .download_limit
            .map(i64::try_from)
            .transpose()
            .map_err(|_| StoreError::InvalidInput)?;
        let lifetime = match metadata.expires_in {
            Some(duration) => {
                let seconds =
                    i64::try_from(duration.as_secs()).map_err(|_| StoreError::InvalidInput)?;
                if seconds == 0 {
                    return Err(StoreError::InvalidInput);
                }
                Some(seconds)
            }
            None => None,
        };

        {
            let file = match stage.file.take() {
                Some(file) => file,
                None => OpenOptions::new().write(true).open(&stage.path)?,
            };
            if file.metadata()?.len() != metadata.size_bytes {
                return Err(StoreError::InvalidInput);
            }
            file.sync_all()?;
        }

        // Publish the complete file before the row. A crash between the two
        // leaves an orphan that the next startup will reclaim.
        let mut state = self.inner.lock();
        let (id, target) = loop {
            let id = random_hex::<SHARE_ID_BYTES>()?;
            let target = self.inner.files_dir.join(&id);
            let row_exists = state.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM shares WHERE id = ?1)",
                [&id],
                |row| row.get::<_, bool>(0),
            )?;
            if !row_exists && !target.try_exists()? {
                break (id, target);
            }
        };
        let created_at = unix_seconds()?;
        let expires_at = match lifetime {
            Some(seconds) => Some(
                created_at
                    .checked_add(seconds)
                    .ok_or(StoreError::InvalidInput)?,
            ),
            None => None,
        };
        fs::rename(&stage.path, &target)?;
        stage.moved = true;
        let publish = (|| -> Result<String> {
            // Persist the directory entry before committing metadata where the OS supports it.
            #[cfg(unix)]
            File::open(&self.inner.files_dir)?.sync_all()?;
            // Publish the row and its public code together.
            let tx = state
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            tx.execute(
                "INSERT INTO shares (id, filename, size_bytes, uploaded_by, created_at,
                                     expires_at, remaining_downloads, password)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    id,
                    metadata.filename,
                    size_bytes,
                    metadata.uploaded_by,
                    created_at,
                    expires_at,
                    remaining_downloads,
                    metadata.password
                ],
            )?;
            let code = share_codes::reserve(&tx, &id)?;
            tx.commit()?;
            Ok(code)
        })();
        let code = match publish {
            Ok(code) => code,
            Err(error) => {
                // If SQLite's final outcome is uncertain, leave the file for startup reconciliation.
                let row_exists = state.conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM shares WHERE id = ?1)",
                    [&id],
                    |row| row.get::<_, bool>(0),
                );
                if matches!(row_exists, Ok(false)) {
                    let _ = fs::remove_file(&target);
                }
                return Err(error);
            }
        };
        Ok(Share {
            id,
            code,
            filename: metadata.filename,
            size_bytes: metadata.size_bytes,
            created_at,
            expires_at,
            remaining_downloads: metadata.download_limit,
            requires_password: metadata.password.is_some(),
        })
    }
}
