"""What packing and keeping texts once save on a store, read without a write.

    python -m bench.store_packing STORE [--every N] [--now-ms MS]

Opens STORE read-only (a daemon may hold it) and reports, as JSON:

- `packing`: the items of turns of agents with no turn running and none
  ended in the last 7 days, as the daemon packs them (zstd level 3, each
  item kept packed only when that is smaller): their bytes before and after,
  and the same for every item, as if every agent were idle. The daemon packs
  the items of each agent's history; this counts each item with the agent
  whose turn wrote it, near enough for an estimate.
- `texts`: the bytes of instructions and of what the events of kept
  configures carry, before and kept once by digest.

With --every N only every Nth item is packed, and its sizes count N times:
an estimate, said as such. Packed items of a store at schema 114 or later
count as they are. Needs Python 3.14, or the zstandard module.
"""
import argparse
import hashlib
import json
import sqlite3
import sys
import time

IDLE_MS = 7 * 24 * 60 * 60 * 1000
HELD = ("instructions", "compaction_instructions", "declare")


def packer():
    try:
        from compression import zstd
    except ImportError:
        pass
    else:
        return lambda item: zstd.compress(item, 3)
    try:
        import zstandard
    except ImportError:
        raise SystemExit("needs Python 3.14, or the zstandard module")
    return zstandard.ZstdCompressor(level=3).compress


def columns(conn, table):
    return {row[1] for row in conn.execute(f"PRAGMA table_info({table})")}


def packing(conn, now_ms, every):
    pack = packer()
    packed_column = "packed" in columns(conn, "nodes")
    idle = {name for (name,) in conn.execute(
        "SELECT name FROM bots WHERE running_turn IS NULL AND status!='deleting'"
        " AND last_turn IS NOT NULL AND COALESCE(last_finished_ms,0)<=?", (now_ms - IDLE_MS,))}
    sums = {key: {"items": 0, "bytes": 0, "packed_bytes": 0} for key in ("idle", "all")}
    # A node's turn is that of the turn start at or above it (`lineage.start`).
    read = "SELECT n.id,n.item,t.bot" + (",n.packed" if packed_column else ",NULL") + \
        " FROM nodes n LEFT JOIN lineage l ON l.node=n.id LEFT JOIN nodes s ON s.id=l.start" \
        " LEFT JOIN turns t ON t.id=s.turn ORDER BY n.id"
    started = time.monotonic()
    for index, (_, item, bot, packed) in enumerate(conn.execute(read)):
        keys = ["all"] + (["idle"] if bot in idle else [])
        if packed is not None:
            size, after = packed, len(item)
        elif index % every:
            continue
        else:
            size = len(item)
            squeezed = pack(bytes(item))
            after = len(squeezed) if len(squeezed) < size else size
        weight = 1 if packed is not None else every
        for key in keys:
            sums[key]["items"] += weight
            sums[key]["bytes"] += size * weight
            sums[key]["packed_bytes"] += after * weight
    for value in sums.values():
        value["saved_bytes"] = value["bytes"] - value["packed_bytes"]
        value["ratio"] = round(value["bytes"] / value["packed_bytes"], 2) if value["packed_bytes"] else None
    sums["idle_agents"] = len(idle)
    sums["seconds"] = round(time.monotonic() - started, 1)
    return sums


def texts(conn):
    if "texts" in {row[0] for row in conn.execute("SELECT name FROM sqlite_master WHERE type='table'")}:
        return {"kept_once_already": True,
                "bytes": conn.execute("SELECT COALESCE(SUM(length(text)),0) FROM texts").fetchone()[0]}
    rows = conn.execute("SELECT instructions,compaction_instructions FROM bot_instructions").fetchall()
    held = {}
    before = 0
    for row in rows:
        for text in row:
            if text is not None:
                before += len(text.encode())
                held[hashlib.sha256(text.encode()).hexdigest()] = len(text.encode())
    events_before = events_after = 0
    for (data,) in conn.execute(
            "SELECT data FROM events WHERE kind IN ('configure_pending','configure_failed')"):
        events_before += len(data.encode())
        value = json.loads(data)
        pending = value.get("configure_pending") or {}
        names = {}
        for field in HELD:
            text = pending.get(field)
            if field == "declare" and isinstance(text, list):
                text = json.dumps(text, separators=(",", ":"), sort_keys=True)
            elif field == "declare" or not isinstance(text, str):
                continue
            digest = hashlib.sha256(text.encode()).hexdigest()
            held[digest] = len(text.encode())
            names[field] = digest
            del pending[field]
        if names:
            value["texts"] = names
        events_after += len(json.dumps(value, separators=(",", ":")).encode())
    after = len(rows) * 64 + sum(held.values())
    return {"instructions_rows": len(rows), "distinct_texts": len(held),
            "bytes": before + events_before, "kept_once_bytes": after + events_after,
            "saved_bytes": before + events_before - after - events_after}


def main(argv):
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("store")
    parser.add_argument("--every", type=int, default=1, help="pack every Nth item and count it N times")
    parser.add_argument("--now-ms", type=int, default=int(time.time() * 1000))
    args = parser.parse_args(argv)
    conn = sqlite3.connect(f"file:{args.store}?mode=ro", uri=True)
    try:
        report = {"store": args.store, "every": args.every,
                  "schema": conn.execute("PRAGMA user_version").fetchone()[0],
                  "pages": conn.execute("PRAGMA page_count").fetchone()[0],
                  "page_size": conn.execute("PRAGMA page_size").fetchone()[0],
                  "texts": texts(conn), "packing": packing(conn, args.now_ms, max(args.every, 1))}
    finally:
        conn.close()
    json.dump(report, sys.stdout, indent=2)
    print()


if __name__ == "__main__":
    main(sys.argv[1:])
