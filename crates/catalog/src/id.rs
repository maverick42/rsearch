//! Identifier generation: UUID v4 built from SQLite's
//! `randomblob(16)` (OS CSPRNG through the bundled rusqlite), with the
//! RFC 4122 version and variant bits applied manually.
//!
//! Used for project and saved-search ids. No new dependency and no
//! application PRNG: the entropy source is the SQLite library already
//! linked into the workspace.

use rusqlite::Connection;

use crate::CatalogError;

/// Generates a random UUID v4 in canonical textual form
/// (`xxxxxxxx-xxxx-4xxx-yxxx-xxxxxxxxxxxx`).
pub(crate) fn new_id(conn: &Connection) -> Result<String, CatalogError> {
    let raw: Vec<u8> = conn
        .query_row("SELECT randomblob(16)", [], |r| r.get(0))
        .map_err(CatalogError::Sqlite)?;
    let mut b = [0u8; 16];
    b.copy_from_slice(&raw);
    // RFC 4122 §4.4: version 4 and the 10xx variant bits.
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;

    let mut out = String::with_capacity(36);
    for (i, byte) in b.iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            out.push('-');
        }
        out.push_str(&format!("{byte:02x}"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_ids_are_canonical_uuid_v4() {
        let conn = Connection::open_in_memory().unwrap();
        for _ in 0..64 {
            let id = new_id(&conn).unwrap();
            assert_eq!(id.len(), 36, "{id}");
            for (i, ch) in id.chars().enumerate() {
                match i {
                    8 | 13 | 18 | 23 => assert_eq!(ch, '-', "{id}"),
                    14 => assert_eq!(ch, '4', "{id}: version nibble"),
                    19 => assert!(matches!(ch, '8' | '9' | 'a' | 'b'), "{id}: variant nibble"),
                    _ => assert!(ch.is_ascii_hexdigit() && !ch.is_ascii_uppercase(), "{id}"),
                }
            }
        }
    }

    #[test]
    fn successive_ids_differ() {
        let conn = Connection::open_in_memory().unwrap();
        let a = new_id(&conn).unwrap();
        let b = new_id(&conn).unwrap();
        assert_ne!(a, b);
    }
}
