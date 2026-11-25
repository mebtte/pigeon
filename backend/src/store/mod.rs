mod cleanup;
mod listing;
mod schema;
mod share_codes;
mod upload;

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fs2::FileExt;
use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::cooldown::PasswordCooldown;

pub use listing::{ListedShare, SharePage};
pub(crate) use share_codes::is_share_code;
pub use upload::StagedUpload;

const LINK_LIFETIME_SECONDS: i64 = 24 * 60 * 60;
const SHARE_ID_BYTES: usize = 16;
const TOKEN_BYTES: usize = 32;

pub type Result<T> = std::result::Result<T, StoreError>;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("share or download link not found")]
    NotFound,
    #[error("incorrect share password")]
    InvalidPassword,
    #[error("share password check is cooling down")]
    PasswordCooldown(Duration),
    #[error("invalid share parameters")]
    InvalidInput,
    #[error("data directory is in use by another Pigeon process")]
    DataDirLocked,
    #[error("operating system random source failed: {0}")]
    Entropy(getrandom::Error),
    #[error("could not allocate a unique share code")]
    ShareCodeUnavailable,
    #[error("data filesystem error: {0}")]
    Io(#[from] io::Error),
    #[error("SQLite error: {0}")]
    Sql(#[from] rusqlite::Error),
}

#[derive(Clone)]
pub struct Store {
    inner: Arc<Inner>,
}

struct Inner {
    root: PathBuf,
    files_dir: PathBuf,
    tmp_dir: PathBuf,
    state: Mutex<State>,
    // Keep the directory locked for the lifetime of any active response body.
    _data_lock: File,
}

struct State {
    conn: Connection,
    active_downloads: HashMap<String, usize>,
    password_cooldown: PasswordCooldown<String>,
}

pub struct NewShare {
    pub filename: String,
    pub size_bytes: u64,
    pub uploaded_by: String,
    pub expires_in: Option<Duration>,
    pub download_limit: Option<u64>,
    pub password: Option<String>,
}

#[derive(Debug)]
pub struct Share {
    id: String,
    pub code: String,
    pub filename: String,
    pub size_bytes: u64,
    pub created_at: i64,
    pub expires_at: Option<i64>,
    pub remaining_downloads: Option<u64>,
    pub requires_password: bool,
}

pub struct DownloadLink {
    pub token: String,
    pub expires_at: i64,
    pub filename: String,
    pub size_bytes: u64,
}

pub struct OpenDownload {
    pub file: File,
    pub lease: DownloadLease,
    pub filename: String,
    pub size_bytes: u64,
}

/// Hold this until the response stream and its file handle have been dropped.
pub struct DownloadLease {
    inner: Arc<Inner>,
    share_id: String,
}

impl Drop for DownloadLease {
    fn drop(&mut self) {
        let mut state = self.inner.lock();
        if let Some(count) = state.active_downloads.get_mut(&self.share_id) {
            *count -= 1;
            if *count == 0 {
                state.active_downloads.remove(&self.share_id);
            }
        }
    }
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    fn open_file(&self, id: &str, size_bytes: u64) -> Result<File> {
        if !is_hex(id, SHARE_ID_BYTES) {
            return Err(StoreError::NotFound);
        }
        let path = self.files_dir.join(id);
        // Reject non-files before open so a stray FIFO cannot block while we hold the state lock.
        let entry = checked_storage_io(fs::symlink_metadata(&path))?;
        if !entry.is_file() || entry.len() != size_bytes {
            return Err(StoreError::NotFound);
        }
        let file = checked_storage_io(File::open(&path))?;
        let opened = checked_storage_io(file.metadata())?;
        if !opened.is_file() || opened.len() != size_bytes {
            return Err(StoreError::NotFound);
        }
        Ok(file)
    }
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        create_dir(path)?;
        let root = path.canonicalize()?;
        let data_lock = private_file(&root.join(".pigeon.lock"), false)?;
        if let Err(error) = FileExt::try_lock_exclusive(&data_lock) {
            if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() {
                return Err(StoreError::DataDirLocked);
            }
            return Err(error.into());
        }

        let files_dir = root.join("files");
        let tmp_dir = root.join("tmp");
        create_owned_dir(&files_dir)?;
        create_owned_dir(&tmp_dir)?;
        let conn = schema::open(&root.join("pigeon.sqlite3"))?;
        let store = Self {
            inner: Arc::new(Inner {
                root,
                files_dir,
                tmp_dir,
                _data_lock: data_lock,
                state: Mutex::new(State {
                    conn,
                    active_downloads: HashMap::new(),
                    password_cooldown: PasswordCooldown::new(None),
                }),
            }),
        };
        store.recover()?;
        Ok(store)
    }

    pub fn available_bytes(&self) -> Result<u64> {
        Ok(fs2::available_space(&self.inner.root)?)
    }

    pub fn share(&self, code: &str) -> Result<Share> {
        if !is_share_code(code) {
            return Err(StoreError::NotFound);
        }
        let state = self.inner.lock();
        let now = unix_seconds()?;
        let share = state
            .conn
            .query_row(
                "SELECT id, filename, size_bytes, created_at, expires_at, remaining_downloads,
                    password IS NOT NULL, code
             FROM share_codes JOIN shares ON shares.id = share_codes.share_id
             WHERE code = ?1 AND (expires_at IS NULL OR expires_at > ?2)
                AND (remaining_downloads IS NULL OR remaining_downloads > 0)",
                params![code, now],
                share_from_row,
            )
            .optional()?
            .ok_or(StoreError::NotFound)?;
        self.inner.open_file(&share.id, share.size_bytes)?;
        Ok(share)
    }

    pub fn issue_link(&self, code: &str, password: Option<&str>) -> Result<DownloadLink> {
        self.issue_link_with_clock(code, password, Instant::now)
    }

    fn issue_link_with_clock(
        &self,
        code: &str,
        password: Option<&str>,
        attempt_now: impl FnOnce() -> Instant,
    ) -> Result<DownloadLink> {
        if !is_share_code(code) {
            return Err(StoreError::NotFound);
        }
        let mut state = self.inner.lock();
        let State {
            conn,
            password_cooldown,
            ..
        } = &mut *state;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = unix_seconds()?;
        let (id, share_expiry, expected_password, filename, size_bytes): (
            String,
            Option<i64>,
            Option<String>,
            String,
            u64,
        ) = tx
            .query_row(
                "SELECT id, expires_at, password, filename, size_bytes
             FROM share_codes JOIN shares ON shares.id = share_codes.share_id
             WHERE code = ?1 AND (expires_at IS NULL OR expires_at > ?2)
                AND (remaining_downloads IS NULL OR remaining_downloads > 0)",
                params![code, now],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .optional()?
            .ok_or(StoreError::NotFound)?;
        self.inner.open_file(&id, size_bytes)?;
        let issued_at = unix_seconds()?;
        if share_expiry.is_some_and(|deadline| deadline <= issued_at) {
            return Err(StoreError::NotFound);
        }
        if let Some(expected) = expected_password.as_deref() {
            // Gate the actual password comparison per share while holding the store lock.
            password_cooldown
                .check(id.clone(), attempt_now())
                .map_err(StoreError::PasswordCooldown)?;
            if Some(expected) != password {
                return Err(StoreError::InvalidPassword);
            }
        }

        let token = random_hex::<TOKEN_BYTES>()?;
        let digest = link_digest(&token);
        let expires_at = share_expiry
            .unwrap_or(i64::MAX)
            .min(issued_at + LINK_LIFETIME_SECONDS);
        // Deducting the last share slot and saving its usable link commit together.
        tx.execute(
            "UPDATE shares SET remaining_downloads = remaining_downloads - 1
             WHERE id = ?1 AND remaining_downloads IS NOT NULL",
            [&id],
        )?;
        tx.execute(
            "INSERT INTO download_links (token_hash, share_id, expires_at) VALUES (?1, ?2, ?3)",
            params![digest.as_slice(), id, expires_at],
        )?;
        tx.commit()?;
        Ok(DownloadLink {
            token,
            expires_at,
            filename,
            size_bytes,
        })
    }

    pub fn open_download(&self, token: &str) -> Result<OpenDownload> {
        if !is_hex(token, TOKEN_BYTES) {
            return Err(StoreError::NotFound);
        }
        let digest = link_digest(token);
        let mut state = self.inner.lock();
        let now = unix_seconds()?;
        let (share_id, filename, size_bytes): (String, String, u64) = state
            .conn
            .query_row(
                "SELECT shares.id, shares.filename, shares.size_bytes FROM download_links
             JOIN shares ON shares.id = download_links.share_id
             WHERE download_links.token_hash = ?1 AND download_links.expires_at > ?2",
                params![digest.as_slice(), now],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?
            .ok_or(StoreError::NotFound)?;

        // Opening the file and registering the active reader share cleanup's lock.
        let file = self.inner.open_file(&share_id, size_bytes)?;
        *state.active_downloads.entry(share_id.clone()).or_default() += 1;
        let lease = DownloadLease {
            share_id,
            inner: Arc::clone(&self.inner),
        };
        Ok(OpenDownload {
            file,
            lease,
            filename,
            size_bytes,
        })
    }
}

fn checked_storage_io<T>(result: io::Result<T>) -> Result<T> {
    match result {
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
            ) =>
        {
            Err(StoreError::NotFound)
        }
        result => Ok(result?),
    }
}

fn share_from_row(row: &Row<'_>) -> rusqlite::Result<Share> {
    Ok(Share {
        id: row.get(0)?,
        code: row.get(7)?,
        filename: row.get(1)?,
        size_bytes: row.get(2)?,
        created_at: row.get(3)?,
        expires_at: row.get(4)?,
        remaining_downloads: row.get(5)?,
        requires_password: row.get(6)?,
    })
}

fn create_dir(path: &Path) -> io::Result<()> {
    let mut builder = DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}

fn create_owned_dir(path: &Path) -> io::Result<()> {
    create_dir(path)?;
    if !fs::symlink_metadata(path)?.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid storage directory",
        ));
    }
    Ok(())
}

fn private_file(path: &Path, create_new: bool) -> io::Result<File> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.is_file() => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid storage file",
            ));
        }
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
        _ => {}
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    if create_new {
        options.create_new(true);
    } else {
        options.create(true).truncate(false);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

fn is_hex(value: &str, byte_length: usize) -> bool {
    value.len() == byte_length * 2
        && value
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

fn random_hex<const N: usize>() -> Result<String> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut bytes = [0; N];
    getrandom::fill(&mut bytes).map_err(StoreError::Entropy)?;
    let mut value = String::with_capacity(N * 2);
    for byte in bytes {
        value.push(char::from(HEX[usize::from(byte >> 4)]));
        value.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    Ok(value)
}

fn link_digest(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

fn unix_seconds() -> Result<i64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_secs()).ok())
        .ok_or_else(|| io::Error::other("system time out of range").into())
}
