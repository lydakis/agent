//! Lossless storage for large artifacts. Transcript items keep their native
//! JSON while their agent works; an idle agent's are packed (see pack.rs).
//! A compressed BLOB starts with a bounded directory of little-endian offsets,
//! followed by independent 16 KiB blocks, each tagged raw (0) or LZ4 (1).
//! Keeping one BLOB per artifact preserves cheap retention and avoids per-block
//! rows/indexes. Paging fetches only the directory and intersecting blocks.
use crate::{
    Error, Result, fail,
    tools::{ARTIFACT_BYTES, PREVIEW_BYTES},
};
use rusqlite::{Connection, OptionalExtension, params};

const BLOCK: usize = 16 * 1024;
const SAMPLE: usize = 4096;
const DIRECTORY: usize = (ARTIFACT_BYTES.div_ceil(BLOCK) + 1) * 4;

fn encode(raw: &[u8]) -> Option<Vec<u8>> {
    if raw.len() <= PREVIEW_BYTES || raw.len() > ARTIFACT_BYTES {
        return None;
    }
    if lz4_flex::block::compress(&raw[..SAMPLE]).len() >= SAMPLE - SAMPLE / 8 {
        return None;
    }
    let count = raw.len().div_ceil(BLOCK);
    let mut out = vec![0; (count + 1) * 4];
    for (index, block) in raw.chunks(BLOCK).enumerate() {
        let start = out.len() as u32;
        out[index * 4..index * 4 + 4].copy_from_slice(&start.to_le_bytes());
        let encoded = lz4_flex::block::compress(block);
        if encoded.len() < block.len() {
            out.push(1);
            out.extend_from_slice(&encoded);
        } else {
            out.push(0);
            out.extend_from_slice(block);
        }
    }
    let end = out.len() as u32;
    out[count * 4..count * 4 + 4].copy_from_slice(&end.to_le_bytes());
    (out.len() < raw.len() - raw.len() / 8).then_some(out)
}

use crate::tools::FILE_RETAINED_BYTES;

/// A retained stream's bytes: whole as a tool kept them, or packed as the
/// store keeps them, in blocks compressed where that pays. Packing is work
/// that grows with the stream, up to a mebibyte, so it is done off the
/// runtime thread before the outcome is recorded (`packed`), and the
/// store's one writer, which every bot's turns share, only inserts it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Retained {
    Whole(Vec<u8>),
    /// Its packed bytes and its size whole.
    Packed(Vec<u8>, usize),
}
impl Retained {
    /// Its size whole.
    pub fn len(&self) -> usize {
        match self {
            Retained::Whole(raw) => raw.len(),
            Retained::Packed(_, size) => *size,
        }
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Its bytes whole, while not yet packed.
    pub fn whole(&self) -> Option<&[u8]> {
        match self {
            Retained::Whole(raw) => Some(raw),
            Retained::Packed(..) => None,
        }
    }
    /// Whether it is worth packing: whole, past the preview and within the
    /// bound. Packing one whose start does not compress leaves it whole.
    pub fn packs(&self) -> bool {
        matches!(self, Retained::Whole(raw) if raw.len() > PREVIEW_BYTES && raw.len() <= ARTIFACT_BYTES)
    }
    /// Packed as the store keeps it, where compressing pays; whole
    /// otherwise.
    pub fn packed(self) -> Retained {
        match self {
            Retained::Whole(raw) => match encode(&raw) {
                Some(data) => Retained::Packed(data, raw.len()),
                None => Retained::Whole(raw),
            },
            packed => packed,
        }
    }
}

/// Insert a retained stream as it was handed in: packed beforehand, or
/// whole. The writer compresses nothing.
pub(super) fn put(
    db: &Connection,
    turn: i64,
    call: &str,
    stream: &str,
    retained: &Retained,
) -> Result<()> {
    let (data, raw_bytes) = match retained {
        Retained::Whole(raw) => (raw.as_slice(), 0),
        Retained::Packed(data, size) => (data.as_slice(), *size as i64),
    };
    db.prepare_cached(
        "INSERT INTO artifacts(turn,call_id,stream,data,raw_bytes) VALUES (?,?,?,?,?)",
    )?
    .execute(params![turn, call, stream, data, raw_bytes])?;
    Ok(())
}

/// A retained output written whole to `file`, which stands in for the
/// store's copy: the row keeps no data, only where it is.
pub(super) fn put_file(
    db: &Connection,
    turn: i64,
    call: &str,
    stream: &str,
    file: &str,
) -> Result<()> {
    db.prepare(
        "INSERT INTO artifacts(turn,call_id,stream,data,raw_bytes,file) VALUES (?,?,?,x'',0,?)",
    )?
    .execute(params![turn, call, stream, file])?;
    Ok(())
}

/// The files that hold `turn`'s retained outputs, for removal with it.
pub(super) fn files(db: &Connection, turn: i64) -> Result<Vec<String>> {
    Ok(db
        .prepare_cached("SELECT file FROM artifacts WHERE turn=? AND file IS NOT NULL")?
        .query_map([turn], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?)
}

/// Remove the files of outputs whose records are gone, once that is
/// durable. A file someone already removed is gone either way; one a link
/// replaced loses only the link, never what it points to.
pub(super) fn remove(files: &[String]) {
    for file in files {
        let _ = std::fs::remove_file(file);
    }
}

/// The file that holds a retained output, when one does.
pub(super) fn file(db: &Connection, turn: i64, call: &str, stream: &str) -> Result<Option<String>> {
    Ok(db
        .prepare_cached(
            "SELECT file FROM artifacts WHERE turn=?1 AND call_id=?2 AND stream=?3 AND file IS NOT NULL",
        )?
        .query_row(params![turn, call, stream], |r| r.get(0))
        .optional()?)
}

/// A page of a retained output's file, as a stored copy is paged; none
/// when the file is gone or is no longer a file. Links are not followed,
/// and opening never waits on a pipe put in its place.
fn read_file(file: &str, offset: u64, limit: usize) -> Result<Option<(u64, Vec<u8>)>> {
    use std::io::{Read, Seek, SeekFrom};
    use std::os::unix::fs::OpenOptionsExt;
    let mut opened = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(file)
    {
        Ok(opened) => opened,
        Err(_) => return Ok(None),
    };
    let found = opened.metadata()?;
    if !found.is_file() {
        return Ok(None);
    }
    let total = found.len();
    // One that is larger was not written here.
    if total > FILE_RETAINED_BYTES {
        return fail("storage_error");
    }
    if offset > total {
        return Err(past_end(total));
    }
    opened.seek(SeekFrom::Start(offset))?;
    let mut bytes = Vec::new();
    opened
        .take(limit.min((total - offset) as usize) as u64)
        .read_to_end(&mut bytes)?;
    Ok(Some((total, bytes)))
}

fn past_end(size: u64) -> Error {
    Error::with(
        "invalid_artifact_page",
        format!("offset is past the end, {size} bytes"),
    )
    .facts(serde_json::json!({"size":size}))
}

pub(super) fn read(
    db: &Connection,
    turn: i64,
    call: &str,
    stream: &str,
    offset: u64,
    limit: usize,
) -> Result<Option<(u64, Vec<u8>)>> {
    if offset > i64::MAX as u64 - 1 {
        return fail("invalid_artifact_page");
    }
    type Row = (i64, i64, Option<Vec<u8>>, Option<String>);
    let row: Option<Row> = db
        .prepare(
            "SELECT raw_bytes,length(data),CASE WHEN raw_bytes=0 THEN substr(data,?4,?5)
         ELSE substr(data,1,?6) END,file FROM artifacts WHERE turn=?1 AND call_id=?2 AND stream=?3",
        )?
        .query_row(
            params![
                turn,
                call,
                stream,
                offset as i64 + 1,
                limit.min(i64::MAX as usize) as i64,
                DIRECTORY as i64
            ],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    let Some((raw_size, stored_size, bytes, file)) = row else {
        return Ok(None);
    };
    if let Some(file) = file {
        return read_file(&file, offset, limit);
    }
    let bytes = bytes.unwrap_or_default();
    if raw_size == 0 {
        let total = u64::try_from(stored_size).map_err(|_| Error::new("storage_error"))?;
        if offset > total {
            return Err(past_end(total));
        }
        return Ok(Some((total, bytes)));
    }
    let total = usize::try_from(raw_size).map_err(|_| Error::new("storage_error"))?;
    if total > ARTIFACT_BYTES {
        return fail("storage_error");
    }
    if offset > total as u64 {
        return Err(past_end(total as u64));
    }
    let count = total.div_ceil(BLOCK);
    let header = (count + 1) * 4;
    if bytes.len() < header {
        return fail("storage_error");
    }
    let offsets: Vec<usize> = bytes[..header]
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| u32::from_le_bytes(*b) as usize)
        .collect();
    if offsets[0] != header
        || offsets[count] as i64 != stored_size
        || offsets
            .windows(2)
            .any(|w| w[0] >= w[1] || w[1] - w[0] > BLOCK + 1)
    {
        return fail("storage_error");
    }
    let offset = offset as usize;
    let end = total.min(offset.saturating_add(limit));
    if offset == end {
        return Ok(Some((total as u64, Vec::new())));
    }
    let first = offset / BLOCK;
    let last = (end - 1) / BLOCK;
    let encoded: Vec<u8> = db
        .prepare(
            "SELECT substr(data,?4,?5) FROM artifacts WHERE turn=?1 AND call_id=?2 AND stream=?3",
        )?
        .query_row(
            params![
                turn,
                call,
                stream,
                offsets[first] as i64 + 1,
                (offsets[last + 1] - offsets[first]) as i64
            ],
            |r| r.get(0),
        )?;
    if encoded.len() != offsets[last + 1] - offsets[first] {
        return fail("storage_error");
    }
    let mut output = Vec::with_capacity(end - offset);
    for index in first..=last {
        let block = &encoded[offsets[index] - offsets[first]..offsets[index + 1] - offsets[first]];
        let expected = BLOCK.min(total - index * BLOCK);
        let mut scratch = [0; BLOCK];
        let raw = match block[0] {
            0 if block.len() == expected + 1 => &block[1..],
            1 => {
                let length =
                    lz4_flex::block::decompress_into(&block[1..], &mut scratch[..expected])
                        .map_err(|_| Error::new("storage_error"))?;
                if length != expected {
                    return fail("storage_error");
                }
                &scratch[..expected]
            }
            _ => return fail("storage_error"),
        };
        let start = index * BLOCK;
        output.extend_from_slice(&raw[offset.saturating_sub(start)..expected.min(end - start)]);
    }
    Ok(Some((total as u64, output)))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn arbitrary_bytes_pages_and_corruption() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE artifacts(turn INTEGER,call_id TEXT,stream TEXT,data BLOB,raw_bytes INTEGER,file TEXT)").unwrap();
        let mut state = 7u64;
        let mut mixed = vec![0; ARTIFACT_BYTES];
        for b in &mut mixed[BLOCK..2 * BLOCK] {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            *b = state as u8;
        }
        for raw in [Vec::new(), vec![255; 100], mixed] {
            db.execute("DELETE FROM artifacts", []).unwrap();
            let packed = Retained::Whole(raw.clone()).packed();
            // Only the mebibyte, mostly zeros, is worth compressing.
            assert_eq!(
                matches!(packed, Retained::Packed(..)),
                raw.len() == ARTIFACT_BYTES
            );
            assert_eq!(packed.len(), raw.len());
            put(&db, 1, "call", "stdout", &packed).unwrap();
            for offset in [0, raw.len() / 2, raw.len().saturating_sub(1), raw.len()] {
                for limit in [0, 4, BLOCK + 1, ARTIFACT_BYTES] {
                    let (_, page) = read(&db, 1, "call", "stdout", offset as u64, limit)
                        .unwrap()
                        .unwrap();
                    assert_eq!(page, raw[offset..raw.len().min(offset + limit)]);
                }
            }
        }
        let bomb = lz4_flex::block::compress(&vec![0; BLOCK * 2]);
        let mut blob = Vec::new();
        blob.extend_from_slice(&8u32.to_le_bytes());
        blob.extend_from_slice(&((9 + bomb.len()) as u32).to_le_bytes());
        blob.push(1);
        blob.extend_from_slice(&bomb);
        db.execute(
            "UPDATE artifacts SET data=?,raw_bytes=?",
            params![blob, BLOCK as i64],
        )
        .unwrap();
        assert!(read(&db, 1, "call", "stdout", 0, 4).is_err());
        db.execute("UPDATE artifacts SET data=x'0000'", []).unwrap();
        assert_eq!(
            read(&db, 1, "call", "stdout", 0, 4).unwrap_err().code,
            "storage_error"
        );
        db.execute(
            "UPDATE artifacts SET raw_bytes=?",
            [ARTIFACT_BYTES as i64 + 1],
        )
        .unwrap();
        assert!(read(&db, 1, "call", "stdout", 0, 4).is_err());
    }
}
