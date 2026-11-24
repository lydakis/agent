//! Packed messages: an agent that has had no turn for a week has the items
//! of its turns kept as zstd frames, as Codex packs a rollout nobody has
//! touched for seven days (codex-rs/rollout: zstd, level 3, in the
//! background, and unpacked when read). Packing is lossless: every read
//! gets the item back byte for byte, through the SQL function `unpack`
//! (see `unpacked!` in db.rs). An item is kept packed only when that is
//! smaller; nothing is cut, summarized, or left out.
use crate::{Error, Result};
use rusqlite::{Connection, functions::FunctionFlags};

/// Codex's level for the rollouts it packs.
const LEVEL: i32 = 3;
/// How long an agent goes without a turn before its items are packed:
/// Codex's seven days.
pub const IDLE_MS: i64 = 7 * 24 * 60 * 60 * 1000;
/// The bytes of items one batch reads, packs and writes at most, past its
/// first turn: what a batch holds in memory, and what one writer job
/// rewrites, stay bounded however long a history is.
pub const BATCH_BYTES: usize = 8 * 1024 * 1024;

/// `item` packed, when that is smaller.
pub fn pack(packer: &mut zstd::bulk::Compressor<'_>, item: &[u8]) -> Result<Option<Vec<u8>>> {
    let packed = packer.compress(item)?;
    Ok((packed.len() < item.len()).then_some(packed))
}

/// A packer at Codex's level, for one batch.
pub fn packer() -> Result<zstd::bulk::Compressor<'static>> {
    Ok(zstd::bulk::Compressor::new(LEVEL)?)
}

/// The item `packed` holds, `len` bytes long.
pub fn unpack(packed: &[u8], len: usize) -> Result<Vec<u8>> {
    let item = zstd::bulk::decompress(packed, len)?;
    if item.len() != len {
        return Err(Error::new("store_item_unreadable"));
    }
    Ok(item)
}

/// `unpack(item,packed)` on `conn`: the item a packed node holds.
pub fn unpacking(conn: &Connection) -> Result<()> {
    conn.create_scalar_function(
        "unpack",
        2,
        FunctionFlags::SQLITE_UTF8
            | FunctionFlags::SQLITE_DETERMINISTIC
            | FunctionFlags::SQLITE_INNOCUOUS,
        |context| {
            let packed = context.get_raw(0).as_blob()?;
            let len: i64 = context.get(1)?;
            unpack(packed, len.max(0) as usize)
                .map_err(|error| rusqlite::Error::UserFunctionError(Box::new(error)))
        },
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_item_reads_back_byte_for_byte() {
        let item = br#"{"type":"message","role":"user","content":[{"type":"input_text","text":"say it again, say it again, say it again"}]}"#;
        let packed = pack(&mut packer().unwrap(), item).unwrap().unwrap();
        assert!(packed.len() < item.len());
        assert_eq!(unpack(&packed, item.len()).unwrap(), item);
        let conn = Connection::open_in_memory().unwrap();
        unpacking(&conn).unwrap();
        let read: Vec<u8> = conn
            .query_row(
                "SELECT unpack(?1,?2)",
                rusqlite::params![packed, item.len() as i64],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(read, item);
    }

    #[test]
    fn an_item_packing_would_not_shrink_stays_plain() {
        assert_eq!(pack(&mut packer().unwrap(), b"{}").unwrap(), None);
    }

    #[test]
    fn a_wrong_length_is_refused() {
        let item = vec![b'a'; 4096];
        let packed = pack(&mut packer().unwrap(), &item).unwrap().unwrap();
        assert!(unpack(&packed, 4095).is_err());
    }
}
