//! Lossless storage for large artifacts. Transcript items keep their native JSON.
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

pub(super) fn put(db: &Connection, turn: i64, call: &str, stream: &str, raw: &[u8]) -> Result<()> {
    let encoded = encode(raw);
    db.prepare("INSERT INTO artifacts(turn,call_id,stream,data,raw_bytes) VALUES (?,?,?,?,?)")?
        .execute(params![
            turn,
            call,
            stream,
            encoded.as_deref().unwrap_or(raw),
            if encoded.is_some() {
                raw.len() as i64
            } else {
                0
            }
        ])?;
    Ok(())
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
    let row: Option<(i64, i64, Option<Vec<u8>>)> = db
        .prepare(
            "SELECT raw_bytes,length(data),CASE WHEN raw_bytes=0 THEN substr(data,?4,?5)
         ELSE substr(data,1,?6) END FROM artifacts WHERE turn=?1 AND call_id=?2 AND stream=?3",
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
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((raw_size, stored_size, bytes)) = row else {
        return Ok(None);
    };
    let bytes = bytes.unwrap_or_default();
    if raw_size == 0 {
        let total = u64::try_from(stored_size).map_err(|_| Error::new("storage_error"))?;
        if offset > total {
            return fail("invalid_artifact_page");
        }
        return Ok(Some((total, bytes)));
    }
    let total = usize::try_from(raw_size).map_err(|_| Error::new("storage_error"))?;
    if total > ARTIFACT_BYTES {
        return fail("storage_error");
    }
    if offset > total as u64 {
        return fail("invalid_artifact_page");
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
        db.execute_batch("CREATE TABLE artifacts(turn INTEGER,call_id TEXT,stream TEXT,data BLOB,raw_bytes INTEGER)").unwrap();
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
            put(&db, 1, "call", "stdout", &raw).unwrap();
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
