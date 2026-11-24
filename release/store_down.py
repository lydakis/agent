#!/usr/bin/env python3
"""Take a stopped daemon's store back to an older release's schema.

    store_down.py STORE SCHEMA [--dry-run]

The daemon brings a store forward by itself when it starts, one schema at a
time. Each of those steps has its way back here, newest first, so a release
that knows only SCHEMA opens the store with nothing lost that it can hold:

    114 -> 113  packed messages unpacked; instructions, and the texts the
                events of kept configures hold, back in place of their
                digests (needs Python 3.14+, or the zstandard module, when
                any message was packed)
    113 -> 112  prompts no longer hold the effort their turn asked for
    112 -> 111  turns no longer count the daemon's process ending under them
    111 -> 110  switches between model families go; each agent stays on
                the model and history it has now (see back_111)
    110 -> 109  no `profile` on each bot
    109 -> 108  nothing: 0.10.2 (108) reads `yields`
    108 -> 107  the `namespace`, `speeds` and `context_windows` settings go
    107 -> 106  instructions back in each bot's row; `yields` is `lift` again
                (0.10.1)

Stop the daemon first (`agent shutdown`): a store a daemon holds is refused.
The store is copied beside itself to STORE.schema-FROM.TIME.backup, with
the store's own permissions, then taken back in one transaction, so a
failure leaves it as it was. Every schema change adds its way back here,
with its step in src/store/db.rs.

Every release installs this script with its helper, store_lift_yields.py:
PREFIX/opt/sonny/libexec/ from the Linux tarball, $(brew --prefix
agent)/libexec/ from Homebrew. Run the one of the release being left, since
only it knows that release's steps.
"""

import json
import os
import sqlite3
import stat
import sys
import time

# Its helper is beside it wherever it is installed, which it leaves as it was.
sys.dont_write_bytecode = True
sys.path.insert(0, os.path.dirname(os.path.realpath(__file__)))

import store_lift_yields  # noqa: E402

NEWEST = 114
OLDEST = 106


def run(conn, script):
    """Each statement of `script` in the open transaction, which
    executescript would commit first."""
    for statement in script.split(";"):
        if statement.strip():
            conn.execute(statement)


def on_lineage(conn, head, node):
    """Whether `node` is `head` or one of its ancestors."""
    return conn.execute(
        "WITH RECURSIVE up(id,parent) AS (SELECT id,parent FROM nodes WHERE id=?1"
        " UNION ALL SELECT n.id,n.parent FROM nodes n JOIN up ON n.id=up.parent)"
        " SELECT EXISTS(SELECT 1 FROM up WHERE id=?2)",
        (head, node),
    ).fetchone()[0] == 1


def unpacker():
    """What unpacks a packed item, with its length: Python's own zstd from
    3.14, else the zstandard module; None with neither."""
    try:
        from compression import zstd
    except ImportError:
        pass
    else:
        return lambda packed, size: zstd.decompress(packed)
    try:
        import zstandard
    except ImportError:
        return None
    unpack = zstandard.ZstdDecompressor()
    return lambda packed, size: unpack.decompress(packed, max_output_size=size)


def back_114(conn):
    """Every packed item unpacked, byte for byte, one at a time; each bot's
    instructions whole in its row again; each event of a kept configure
    with its texts back where they were, in the JSON the daemon writes;
    `texts` goes. The store keeps giving freed pages back to the disk."""
    ids = [row[0] for row in conn.execute("SELECT id FROM nodes WHERE packed IS NOT NULL ORDER BY id")]
    unpack = unpacker() if ids else None
    if ids and unpack is None:
        raise SystemExit(f"{len(ids)} packed messages need Python 3.14 or later, or the zstandard module")
    for node in ids:
        packed, size = conn.execute("SELECT item,packed FROM nodes WHERE id=?", (node,)).fetchone()
        item = unpack(packed, size)
        if len(item) != size:
            raise SystemExit(f"node {node} unpacks to {len(item)} bytes, not {size}")
        conn.execute("UPDATE nodes SET item=? WHERE id=?", (item, node))
    events = [row[0] for row in conn.execute(
        "SELECT id FROM events WHERE kind IN ('configure_pending','configure_failed')"
        " AND json_type(data,'$.texts') IS NOT NULL ORDER BY id")]
    for event in events:
        data = json.loads(conn.execute("SELECT data FROM events WHERE id=?", (event,)).fetchone()[0])
        for field, text_id in data.pop("texts").items():
            text = conn.execute("SELECT text FROM texts WHERE id=?", (text_id,)).fetchone()[0]
            data["configure_pending"][field] = json.loads(text) if field == "declare" else text
        conn.execute("UPDATE events SET data=? WHERE id=?", (
            json.dumps(data, separators=(",", ":"), ensure_ascii=False, sort_keys=True), event))
    run(
        conn,
        "CREATE TABLE plain_instructions(id INTEGER PRIMARY KEY REFERENCES bots(id),"
        "    instructions TEXT NOT NULL, compaction_instructions TEXT);"
        "INSERT INTO plain_instructions(id,instructions,compaction_instructions)"
        "    SELECT i.id,t.text,c.text FROM bot_instructions i JOIN texts t ON t.id=i.instructions"
        "    LEFT JOIN texts c ON c.id=i.compaction_instructions;"
        "DROP TABLE bot_instructions;"
        "ALTER TABLE plain_instructions RENAME TO bot_instructions;"
        "DROP TABLE texts;"
        "ALTER TABLE nodes DROP COLUMN packed;"
        "ALTER TABLE bots DROP COLUMN packed_node;"
    )


def back_113(conn):
    run(conn, "ALTER TABLE nodes DROP COLUMN effort")


def back_112(conn):
    run(conn, "ALTER TABLE turns DROP COLUMN restarts; ALTER TABLE turns DROP COLUMN restart_head")


def back_111(conn):
    """A switched agent's history was written again in the other family's
    items, each prompt node holding its turn beside the original's. A turn
    keeps one node again: the one its agent's history reaches, else the
    newest. Each agent stays on the model and branch it has; the branches
    it left, and any a switch was still writing, stay as history no agent
    reaches."""
    shared = conn.execute(
        "SELECT n.turn,b.head FROM nodes n LEFT JOIN turns t ON t.id=n.turn"
        " LEFT JOIN bots b ON b.name=t.bot"
        " WHERE n.turn IS NOT NULL GROUP BY n.turn HAVING count(*)>1"
    ).fetchall()
    for turn, head in shared:
        nodes = [row[0] for row in conn.execute("SELECT id FROM nodes WHERE turn=? ORDER BY id DESC", (turn,))]
        keep = next((node for node in nodes if head is not None and on_lineage(conn, head, node)), nodes[0])
        conn.execute("UPDATE nodes SET turn=NULL WHERE turn=? AND id!=?", (turn, keep))
    run(
        conn,
        "DROP INDEX nodes_turn;"
        "CREATE UNIQUE INDEX nodes_turn ON nodes(turn) WHERE turn IS NOT NULL;"
        "DROP TABLE switches;"
        "DROP TABLE translated;"
        "DROP TABLE translating;"
        "DROP TABLE worded;"
        "DROP INDEX bots_family_start;"
        "ALTER TABLE bots DROP COLUMN family_start;"
    )


def back_110(conn):
    run(conn, "ALTER TABLE bots DROP COLUMN profile")


def back_109(conn):
    pass


def back_108(conn):
    run(
        conn,
        "UPDATE bots SET settings=json_remove(settings,'$.namespace','$.speeds','$.context_windows')"
        " WHERE json_type(settings,'$.namespace') IS NOT NULL"
        "    OR json_type(settings,'$.speeds') IS NOT NULL"
        "    OR json_type(settings,'$.context_windows') IS NOT NULL;"
        "UPDATE bots SET configure_pending=json_remove(configure_pending,"
        "    '$.settings.namespace','$.settings.speeds','$.settings.context_windows')"
        " WHERE json_type(configure_pending,'$.settings') IS NOT NULL;"
        # A keyed creation's record, so a resend from the older release,
        # which cannot carry them, still matches it.
        "UPDATE bots SET creation=json_remove(creation,'$.request.settings.namespace',"
        "    '$.request.settings.speeds','$.request.settings.context_windows')"
        " WHERE json_type(creation,'$.request.settings') IS NOT NULL;"
    )


def back_107(conn):
    store_lift_yields.migrate(conn, "yields", "lift")
    run(
        conn,
        "ALTER TABLE bots ADD COLUMN instructions TEXT NOT NULL DEFAULT '';"
        "ALTER TABLE bots ADD COLUMN compaction_instructions TEXT;"
        "UPDATE bots SET (instructions,compaction_instructions)="
        "    (SELECT instructions,compaction_instructions FROM bot_instructions i WHERE i.id=bots.id);"
        "DROP TABLE bot_instructions;"
    )


# Each schema's way back to the one before it.
BACK = {114: back_114, 113: back_113, 112: back_112, 111: back_111, 110: back_110, 109: back_109, 108: back_108, 107: back_107}


def back_up(conn, store, copy):
    """`store` copied to `copy` with `VACUUM INTO`, as the daemon backs up
    before it migrates (src/store/db.rs, back_up): the copy holds everything
    the store does, so it is made empty first, readable by no one else
    whatever the umask, then given the store's own permissions before a
    byte goes in. A copy that fails is removed."""
    fd = os.open(copy, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    try:
        try:
            os.fchmod(fd, stat.S_IMODE(os.stat(store).st_mode))
        finally:
            os.close(fd)
        conn.execute("VACUUM INTO ?", (copy,))
    except BaseException:
        os.unlink(copy)
        raise


def down(store, to, dry=False):
    """Take `store` back to schema `to`; gives the schema it was at."""
    store = os.path.realpath(store)
    if not os.path.isfile(store):
        raise SystemExit(f"no store at {store}")
    lock = store_lift_yields.held(store)
    try:
        # Autocommit, so the transaction below is the only one.
        conn = sqlite3.connect(store, isolation_level=None)
        try:
            at = conn.execute("PRAGMA user_version").fetchone()[0]
            if at > NEWEST:
                raise SystemExit(f"store schema {at} is newer than {NEWEST}; use that release's store_down.py")
            if not OLDEST <= to <= at:
                raise SystemExit(f"store schema {at} goes back to {OLDEST} through {at}, not {to}")
            if to == at:
                return at
            if not dry:
                back_up(conn, store, f"{store}.schema-{at}.{int(time.time())}.backup")
            # A table others point at is dropped; checked before the commit.
            conn.execute("PRAGMA foreign_keys=OFF")
            conn.execute("BEGIN IMMEDIATE")
            try:
                for schema in range(at, to, -1):
                    BACK[schema](conn)
                broken = conn.execute("PRAGMA foreign_key_check").fetchall()
                if broken:
                    raise SystemExit(f"foreign keys broken in {sorted({row[0] for row in broken})}")
                conn.execute(f"PRAGMA user_version={int(to)}")
                conn.execute("ROLLBACK" if dry else "COMMIT")
            except BaseException:
                if conn.in_transaction:
                    conn.execute("ROLLBACK")
                raise
            return at
        finally:
            conn.close()
    finally:
        lock.close()


def main(argv):
    if argv in (["--help"], ["-h"]):
        print(__doc__.strip())
        return
    args = [arg for arg in argv if arg != "--dry-run"]
    if len(args) != 2 or not args[1].isdigit():
        raise SystemExit(__doc__)
    store, to = args[0], int(args[1])
    at = down(store, to, dry=len(args) != len(argv))
    print(json.dumps({"store": os.path.realpath(store), "from": at, "schema": to,
                      "dry_run": len(args) != len(argv)}))


if __name__ == "__main__":
    main(sys.argv[1:])
