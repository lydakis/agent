#!/usr/bin/env python3
"""Take a stopped daemon's store back to an older release's schema.

    store_down.py STORE SCHEMA [--dry-run]

The daemon brings a store forward by itself when it starts, one schema at a
time. Each of those steps has its way back here, newest first, so a release
that knows only SCHEMA opens the store with nothing lost that it can hold:

    109 -> 108  nothing: 0.10.2 (108) reads `yields`
    108 -> 107  the `namespace`, `speeds` and `context_windows` settings go
    107 -> 106  instructions back in each bot's row; `yields` is `lift` again
                (0.10.1)

Stop the daemon first (`agent shutdown`): a store a daemon holds is refused.
The store is copied beside itself to STORE.schema-FROM.TIME.backup, then
taken back in one transaction, so a failure leaves it as it was. Every
schema change adds its way back here, with its step in src/store/db.rs.
"""

import fcntl
import json
import os
import sqlite3
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import store_lift_yields  # noqa: E402

NEWEST = 109
OLDEST = 106


def run(conn, script):
    """Each statement of `script` in the open transaction, which
    executescript would commit first."""
    for statement in script.split(";"):
        if statement.strip():
            conn.execute(statement)


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
BACK = {109: back_109, 108: back_108, 107: back_107}


def held(store):
    """The daemon's owner lock, taken, or SystemExit while a daemon holds it."""
    lock = open(store + ".owner-lock", "a")
    try:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        lock.close()
        raise SystemExit("a daemon holds this store; stop it first (agent shutdown)")
    return lock


def down(store, to, dry=False):
    """Take `store` back to schema `to`; gives the schema it was at."""
    store = os.path.realpath(store)
    if not os.path.isfile(store):
        raise SystemExit(f"no store at {store}")
    if os.stat(store).st_nlink != 1:
        raise SystemExit("the store has another hard link, so another daemon lock; copy it first")
    lock = held(store)
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
                conn.execute("VACUUM INTO ?", (f"{store}.schema-{at}.{int(time.time())}.backup",))
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
    args = [arg for arg in argv if arg != "--dry-run"]
    if len(args) != 2 or not args[1].isdigit():
        raise SystemExit(__doc__)
    store, to = args[0], int(args[1])
    at = down(store, to, dry=len(args) != len(argv))
    print(json.dumps({"store": os.path.realpath(store), "from": at, "schema": to,
                      "dry_run": len(args) != len(argv)}))


if __name__ == "__main__":
    main(sys.argv[1:])
