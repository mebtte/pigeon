use rusqlite::{Connection, Transaction, params};

use super::{Result, StoreError};

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
const CODE_LENGTH: usize = 6;
const MAX_ATTEMPTS: usize = 32;

pub(crate) fn is_share_code(code: &str) -> bool {
    code.len() == CODE_LENGTH
        && code
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

pub(super) fn backfill(conn: &mut Connection) -> Result<()> {
    let tx = conn.transaction()?;
    let ids = {
        let mut statement = tx.prepare(
            "SELECT shares.id FROM shares
             LEFT JOIN share_codes ON share_codes.share_id = shares.id
             WHERE share_codes.code IS NULL",
        )?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for id in ids {
        reserve(&tx, &id)?;
    }
    Ok(tx.commit()?)
}

pub(super) fn reserve(tx: &Transaction<'_>, id: &str) -> Result<String> {
    reserve_with(tx, id, random_code)
}

pub(super) fn reserve_with(
    tx: &Transaction<'_>,
    id: &str,
    mut candidate: impl FnMut() -> Result<String>,
) -> Result<String> {
    for _ in 0..MAX_ATTEMPTS {
        let code = candidate()?;
        // A code remains reserved after the associated share is deleted.
        if tx.execute(
            "INSERT INTO share_codes (code, share_id) VALUES (?1, ?2)
             ON CONFLICT(code) DO NOTHING",
            params![code, id],
        )? == 1
        {
            return Ok(code);
        }
    }
    Err(StoreError::ShareCodeUnavailable)
}

fn random_code() -> Result<String> {
    let mut bytes = [0; CODE_LENGTH];
    getrandom::fill(&mut bytes).map_err(StoreError::Entropy)?;
    // Each character uses six independent random bits from the 64-symbol alphabet.
    Ok(bytes
        .into_iter()
        .map(|byte| char::from(ALPHABET[usize::from(byte & 63)]))
        .collect())
}
