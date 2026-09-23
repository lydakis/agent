//! Isolated native storage experiment, not the daemon's database format.
//! Compare raw BLOBs with independently compressed 16 KiB blocks. Both use
//! WAL/NORMAL, 32-object transactions, identical input bytes and read ranges.
//! Reports are component measurements, not end-to-end turn latency claims.
use miniz_oxide::{deflate::compress_to_vec_zlib, inflate::decompress_to_vec_zlib_with_limit};
use rusqlite::{Connection, OpenFlags, params};
use serde_json::{Value, json};
use std::{error::Error, path::Path, time::Instant};

type Result<T> = std::result::Result<T, Box<dyn Error>>;
const BLOCK: usize = 16 * 1024;
const PAGE: usize = 64 * 1024;
const MIN_COMPRESS: usize = 4096;

fn open(path: &Path) -> Result<Connection> {
    // Never overwrite a corpus, real store, or previous result.
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    let db = Connection::open(path)?;
    db.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;
        CREATE TABLE objects(id INTEGER PRIMARY KEY, size INTEGER NOT NULL, data BLOB);
        CREATE TABLE blocks(object INTEGER NOT NULL REFERENCES objects(id),
            block INTEGER NOT NULL, size INTEGER NOT NULL, compressed INTEGER NOT NULL,
            data BLOB NOT NULL, PRIMARY KEY(object,block)) WITHOUT ROWID;
        PRAGMA foreign_keys=ON;",
    )?;
    Ok(db)
}

#[derive(Clone, Copy)]
enum Codec {
    Zlib,
    Lz4,
}
impl Codec {
    fn encode(self, raw: &[u8]) -> Vec<u8> {
        match self {
            Self::Zlib => compress_to_vec_zlib(raw, 1),
            Self::Lz4 => lz4_flex::block::compress(raw),
        }
    }
    fn tag(self) -> i64 {
        match self {
            Self::Zlib => 1,
            Self::Lz4 => 2,
        }
    }
    fn name(self) -> &'static str {
        match self {
            Self::Zlib => "miniz_oxide 0.8.9 zlib level 1",
            Self::Lz4 => "lz4_flex 0.14.0 block",
        }
    }
}

fn put(db: &Connection, id: i64, raw: &[u8], compress: bool, codec: Codec) -> Result<()> {
    let mut blocks = Vec::new();
    // An incompressible prefix makes compression optional, never lossy.
    // Sampling caps wasted work on ciphertext/binary output; a mixed payload
    // with an incompressible prefix may deliberately miss a space saving.
    let worth_compressing = compress && raw.len() >= MIN_COMPRESS && {
        let sample = &raw[..MIN_COMPRESS];
        codec.encode(sample).len() < sample.len() - sample.len() / 8
    };
    if worth_compressing {
        for block in raw.chunks(BLOCK) {
            let encoded = codec.encode(block);
            if encoded.len() < block.len() {
                blocks.push((block.len(), codec.tag(), encoded));
            } else {
                blocks.push((block.len(), 0i64, block.to_vec()));
            }
        }
    }
    let stored: usize = blocks.iter().map(|(_, _, data)| data.len() + 32).sum();
    if !blocks.is_empty() && stored < raw.len() - raw.len() / 8 {
        db.prepare_cached("INSERT INTO objects VALUES (?,?,NULL)")?
            .execute(params![id, i64::try_from(raw.len())?])?;
        let mut insert = db.prepare_cached("INSERT INTO blocks VALUES (?,?,?,?,?)")?;
        for (n, (size, compressed, data)) in blocks.iter().enumerate() {
            insert.execute(params![id, n as i64, *size as i64, compressed, data])?;
        }
    } else {
        db.prepare_cached("INSERT INTO objects VALUES (?,?,?)")?
            .execute(params![id, i64::try_from(raw.len())?, raw])?;
    }
    Ok(())
}

fn decode(data: &[u8], codec: i64, expected: usize) -> Result<Vec<u8>> {
    if expected > BLOCK {
        return Err("oversized block".into());
    }
    let raw = match codec {
        0 => data.to_vec(),
        1 => decompress_to_vec_zlib_with_limit(data, expected)
            .map_err(|_| "invalid compressed block")?,
        2 => {
            let mut raw = vec![0; expected];
            let len = lz4_flex::block::decompress_into(data, &mut raw)
                .map_err(|_| "invalid compressed block")?;
            raw.truncate(len);
            raw
        }
        _ => return Err("unknown block codec".into()),
    };
    if raw.len() != expected {
        return Err("incorrect block length".into());
    }
    Ok(raw)
}

fn read(db: &Connection, id: i64, offset: usize, limit: usize) -> Result<Vec<u8>> {
    if limit > PAGE {
        return Err("page exceeds limit".into());
    }
    let (size, chunked, inline): (i64, bool, Option<Vec<u8>>) = db
        .prepare_cached("SELECT size,data IS NULL,substr(data,?,?) FROM objects WHERE id=?")?
        .query_row(
            params![
                i64::try_from(offset)?
                    .checked_add(1)
                    .ok_or("invalid offset")?,
                limit as i64,
                id
            ],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
    let size = usize::try_from(size)?;
    if offset > size {
        return Err("offset exceeds payload".into());
    }
    if offset == size || limit == 0 {
        return Ok(Vec::new());
    }
    if !chunked {
        return Ok(inline.ok_or("missing inline payload")?);
    }
    let end = size.min(offset.saturating_add(limit));
    let mut output = Vec::with_capacity(end - offset);
    if end == offset {
        return Ok(output);
    }
    let mut stmt = db.prepare_cached(
        "SELECT block,size,compressed,data FROM blocks WHERE object=? AND block BETWEEN ? AND ? ORDER BY block")?;
    let mut rows = stmt.query(params![
        id,
        (offset / BLOCK) as i64,
        ((end - 1) / BLOCK) as i64
    ])?;
    let mut wanted = offset / BLOCK;
    while let Some(row) = rows.next()? {
        let n = usize::try_from(row.get::<_, i64>(0)?)?;
        if n != wanted {
            return Err("missing or unordered block".into());
        }
        let raw = decode(
            &row.get::<_, Vec<u8>>(3)?,
            row.get(2)?,
            usize::try_from(row.get::<_, i64>(1)?)?,
        )?;
        let start = n.checked_mul(BLOCK).ok_or("invalid block")?;
        let expected = BLOCK.min(size - start);
        if raw.len() != expected {
            return Err("block does not match payload size".into());
        }
        output.extend_from_slice(&raw[offset.saturating_sub(start)..raw.len().min(end - start)]);
        wanted += 1;
    }
    if output.len() != end - offset {
        return Err("incomplete payload".into());
    }
    Ok(output)
}

#[cfg(unix)]
fn cpu_seconds() -> f64 {
    // getrusage initializes this POD value and does not retain its pointer.
    unsafe {
        let mut usage: libc::rusage = std::mem::zeroed();
        assert_eq!(libc::getrusage(libc::RUSAGE_SELF, &mut usage), 0);
        usage.ru_utime.tv_sec as f64
            + usage.ru_stime.tv_sec as f64
            + (usage.ru_utime.tv_usec + usage.ru_stime.tv_usec) as f64 / 1e6
    }
}

fn trial(corpus: &Connection, path: &Path, compressed: bool, codec: Codec) -> Result<Value> {
    let mut db = open(path)?;
    let mut source = corpus.prepare("SELECT id,data FROM payloads ORDER BY id")?;
    let mut rows = source.query([])?;
    let mut metadata = Vec::new();
    let cpu = cpu_seconds();
    let start = Instant::now();
    let mut tx = db.transaction()?;
    let mut payload_bytes = 0;
    while let Some(row) = rows.next()? {
        let id: i64 = row.get(0)?;
        let raw: Vec<u8> = row.get(1)?;
        put(&tx, id, &raw, compressed, codec)?;
        payload_bytes += raw.len();
        metadata.push((id, raw.len()));
        if metadata.len() % 32 == 0 {
            tx.commit()?;
            tx = db.transaction()?;
        }
    }
    drop(rows);
    tx.commit()?;
    let write_ms = start.elapsed().as_secs_f64() * 1000.0;
    let write_cpu_ms = (cpu_seconds() - cpu) * 1000.0;
    let wal_bytes = std::fs::metadata(path.with_extension("sqlite-wal"))?.len();
    // Include checkpoint wall time separately; it is not hidden as teardown.
    let checkpoint = Instant::now();
    db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
    let checkpoint_ms = checkpoint.elapsed().as_secs_f64() * 1000.0;
    let database_bytes = std::fs::metadata(path)?.len();
    let chunked: i64 =
        db.query_row("SELECT count(*) FROM objects WHERE data IS NULL", [], |r| {
            r.get(0)
        })?;
    drop(db);
    let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    // A reproducible mix of 4 KiB random slices and whole bounded pages.
    // The OS cache is uncontrolled; these are not cold-disk measurements.
    let mut micros = Vec::new();
    let cpu = cpu_seconds();
    let start = Instant::now();
    let mut state = 7u64;
    for n in 0..4096 {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let (id, size) = metadata[state as usize % metadata.len()];
        let offset = if size == 0 {
            0
        } else {
            (state >> 16) as usize % size
        };
        let limit = if n % 8 == 0 { PAGE } else { 4096 };
        let at = Instant::now();
        let result = read(&db, id, offset, limit)?;
        std::hint::black_box(result);
        micros.push(at.elapsed().as_secs_f64() * 1e6);
    }
    let read_ms = start.elapsed().as_secs_f64() * 1000.0;
    let read_cpu_ms = (cpu_seconds() - cpu) * 1000.0;
    micros.sort_by(f64::total_cmp);
    // Verify every original byte after reopen, outside measurement windows.
    let mut rows = source.query([])?;
    while let Some(row) = rows.next()? {
        let id: i64 = row.get(0)?;
        let raw: Vec<u8> = row.get(1)?;
        for (n, block) in raw.chunks(BLOCK).enumerate() {
            if read(&db, id, n * BLOCK, BLOCK)? != block {
                return Err("round-trip mismatch".into());
            }
        }
        if raw.is_empty() && !read(&db, id, 0, BLOCK)?.is_empty() {
            return Err("empty payload mismatch".into());
        }
    }
    Ok(
        json!({"compressed":compressed,"objects":metadata.len(),"chunked_objects":chunked,
        "payload_bytes":payload_bytes,"database_bytes":database_bytes,"wal_bytes_at_end":wal_bytes,
        "write_ms":write_ms,"write_cpu_ms":write_cpu_ms,"checkpoint_ms":checkpoint_ms,
        "read_ms":read_ms,"read_cpu_ms":read_cpu_ms,"read_p50_us":micros[2048],
        "read_p95_us":micros[3891],"round_trip_verified":true}),
    )
}

fn main() -> Result<()> {
    let mut corpus = None;
    let mut out = None;
    let mut repeats = 5usize;
    let mut codec = Codec::Zlib;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--help" || arg == "-h" {
            println!(
                "Usage: storage-codec-screen --corpus FILE --out DIRECTORY [--repeats N] [--codec zlib|lz4]"
            );
            return Ok(());
        }
        let value = args.next().ok_or("expected option value")?;
        match arg.as_str() {
            "--corpus" => corpus = Some(value),
            "--out" => out = Some(value),
            "--repeats" => repeats = value.parse()?,
            "--codec" => {
                codec = match value.as_str() {
                    "zlib" => Codec::Zlib,
                    "lz4" => Codec::Lz4,
                    _ => return Err("codec must be zlib or lz4".into()),
                }
            }
            _ => return Err("options: --corpus FILE --out DIRECTORY --repeats N".into()),
        }
    }
    if repeats == 0 {
        return Err("--repeats must be positive".into());
    }
    let corpus = Connection::open_with_flags(
        corpus.ok_or("--corpus required")?,
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let count: i64 = corpus.query_row("SELECT count(*) FROM payloads", [], |r| r.get(0))?;
    if count == 0 {
        return Err("empty corpus".into());
    }
    let out = out.ok_or("--out required")?;
    let out = Path::new(&out);
    std::fs::create_dir(out)?;
    let mut results = Vec::new();
    for round in 0..=repeats {
        for compressed in if round % 2 == 0 {
            [false, true]
        } else {
            [true, false]
        } {
            let path = out.join(format!("{round}-{compressed}.sqlite"));
            let mut result = trial(&corpus, &path, compressed, codec)?;
            result["warmup"] = json!(round == 0);
            result["round"] = json!(round);
            println!("{result}");
            results.push(result);
            // The corpus and aggregate report are sufficient to reproduce.
            std::fs::remove_file(&path)?;
            for suffix in ["sqlite-wal", "sqlite-shm"] {
                match std::fs::remove_file(path.with_extension(suffix)) {
                    Ok(()) => (),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                    Err(e) => return Err(e.into()),
                }
            }
        }
    }
    std::fs::write(
        out.join("result.json"),
        serde_json::to_vec_pretty(&json!({
        "schema":"storage_codec_v1","sqlite_version":rusqlite::version(),"codec":codec.name(),
        "block_bytes":BLOCK,"max_page_bytes":PAGE,"sample_bytes":MIN_COMPRESS,
        "transaction_objects":32,"samples":results}))?,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn database() -> Connection {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE objects(id INTEGER PRIMARY KEY,size INTEGER,data BLOB);
            CREATE TABLE blocks(object INTEGER,block INTEGER,size INTEGER,compressed INTEGER,data BLOB,
            PRIMARY KEY(object,block)) WITHOUT ROWID;").unwrap();
        db
    }

    #[test]
    fn exact_bytes_and_bounded_pages_across_block_boundaries() {
        for (compressed, codec) in [
            (false, Codec::Zlib),
            (true, Codec::Zlib),
            (true, Codec::Lz4),
        ] {
            let db = database();
            let raw = "é\0🙂tool result\n".repeat(12000).into_bytes();
            put(&db, 1, &raw, compressed, codec).unwrap();
            for offset in [0, 1, BLOCK - 1, BLOCK, raw.len() - 1, raw.len()] {
                for limit in [0, 1, 4096, BLOCK, PAGE] {
                    assert_eq!(
                        read(&db, 1, offset, limit).unwrap(),
                        raw[offset..raw.len().min(offset + limit)]
                    );
                }
            }
            assert!(read(&db, 1, 0, PAGE + 1).is_err());
            assert!(read(&db, 1, raw.len() + 1, 1).is_err());
            put(&db, 2, b"", compressed, codec).unwrap();
            assert!(read(&db, 2, 0, 4096).unwrap().is_empty());
        }
    }

    #[test]
    fn corrupted_missing_and_oversized_blocks_fail() {
        let db = database();
        put(&db, 1, &vec![b'x'; BLOCK * 3], true, Codec::Zlib).unwrap();
        db.execute("DELETE FROM blocks WHERE block=1", []).unwrap();
        assert!(read(&db, 1, BLOCK, BLOCK).is_err());
        db.execute("UPDATE blocks SET data=x'00' WHERE block=0", [])
            .unwrap();
        assert!(read(&db, 1, 0, 1).is_err());
        let bomb = compress_to_vec_zlib(&vec![b'x'; BLOCK + 1], 1);
        assert!(decode(&bomb, 1, BLOCK).is_err());
        assert!(decode(b"", 0, BLOCK + 1).is_err());
        let bomb = lz4_flex::block::compress(&vec![b'x'; BLOCK + 1]);
        assert!(decode(&bomb, 2, BLOCK).is_err());
        assert!(decode(&[0], 2, BLOCK).is_err());
        assert!(decode(b"", 99, 0).is_err());
    }
}
