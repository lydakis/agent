#!/usr/bin/env python3
"""Move a stopped daemon's store between `lift` and `yields`.

0.10.2 renamed a declared tool's `lift` to `yields` (#1203) with no store
migration, so a store whose saved tool lists still say `lift` stops the
0.10.2 daemon with "unknown field `lift`". Schema 109 does this in the
daemon; this script does the same by hand, for a store an older daemon
last wrote, or back again.

    up    `lift` -> `yields`: before the first start of 0.10.2.
    down  `yields` -> `lift`: before starting a release older than 0.10.2.

Each declared list is kept once under the SHA-256 of its JSON, so a
rewritten list moves to its new digest and every bot, creation record and
kept `configure` that named it follows. Nothing else changes. Stop the
daemon first (`agent shutdown`) and back the store up; the whole rewrite
is one transaction, so a failure leaves the store as it was.

Usage: store_lift_yields.py up|down [STORE]   (default ~/.agent/state.sqlite)
       add --dry-run to count without writing.
"""

import hashlib
import json
import os
import sqlite3
import sys

RENAMES = {"up": ("lift", "yields"), "down": ("yields", "lift")}


def renamed(tools, old, new):
    """`tools` with each tool's `old` key named `new`, in the same place.

    The key keeps its position, which is the field's place in the daemon's
    `Declared`, so the JSON is the bytes the daemon itself writes."""
    changed = False
    out = []
    for tool in tools:
        if isinstance(tool, dict) and old in tool:
            if new in tool:
                raise SystemExit(f"a tool has both {old!r} and {new!r}: {tool.get('name')!r}")
            tool = {(new if key == old else key): value for key, value in tool.items()}
            changed = True
        out.append(tool)
    return out, changed


def dumps(value):
    # serde_json's compact form: no spaces, UTF-8 as is.
    return json.dumps(value, separators=(",", ":"), ensure_ascii=False)


def digest_of(text):
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def migrate(conn, old, new):
    counts = {"lists": 0, "bots": 0, "creations": 0, "configures": 0}
    has = {
        row[0]
        for row in conn.execute("SELECT name FROM sqlite_master WHERE type='table'")
    }
    if "bots" not in has or "tool_lists" not in has:
        raise SystemExit("not an agent store at schema 101 or later (no bots or tool_lists)")
    moved = {}
    for digest, text in conn.execute("SELECT digest,tools FROM tool_lists").fetchall():
        tools, changed = renamed(json.loads(text), old, new)
        if not changed:
            continue
        json_text = dumps(tools)
        moved[digest] = (digest_of(json_text), json_text)
    for digest, (fresh, json_text) in moved.items():
        conn.execute(
            "INSERT OR IGNORE INTO tool_lists(digest,tools) VALUES (?,?)", (fresh, json_text)
        )
        counts["bots"] += conn.execute(
            "UPDATE bots SET declared=? WHERE declared=?", (fresh, digest)
        ).rowcount
        counts["creations"] += conn.execute(
            "UPDATE bots SET creation=json_set(creation,'$.request.declare',?)"
            " WHERE json_type(creation,'$.request.declare')='text'"
            " AND json_extract(creation,'$.request.declare')=?",
            (fresh, digest),
        ).rowcount
        counts["configures"] += conn.execute(
            "UPDATE bots SET configure_pending=json_set(configure_pending,'$.declare',?)"
            " WHERE json_type(configure_pending,'$.declare')='text'"
            " AND json_extract(configure_pending,'$.declare')=?",
            (fresh, digest),
        ).rowcount
        conn.execute("DELETE FROM tool_lists WHERE digest=?", (digest,))
        counts["lists"] += 1
    # A `configure` kept for a running turn's end holds its tools whole.
    kept = conn.execute(
        "SELECT id,configure_pending FROM bots"
        " WHERE json_type(configure_pending,'$.declare')='array'"
    ).fetchall()
    for bot, text in kept:
        pending = json.loads(text)
        pending["declare"], changed = renamed(pending["declare"], old, new)
        if changed:
            conn.execute(
                "UPDATE bots SET configure_pending=? WHERE id=?", (dumps(pending), bot)
            )
            counts["configures"] += 1
    return counts


def main(argv):
    args = [arg for arg in argv if arg != "--dry-run"]
    dry = len(args) != len(argv)
    if not args or args[0] not in RENAMES or len(args) > 2:
        raise SystemExit(__doc__)
    old, new = RENAMES[args[0]]
    store = args[1] if len(args) == 2 else os.path.expanduser("~/.agent/state.sqlite")
    if not os.path.isfile(store):
        raise SystemExit(f"no store at {store}")
    conn = sqlite3.connect(store, isolation_level=None, timeout=5)
    version = conn.execute("PRAGMA user_version").fetchone()[0]
    conn.execute("BEGIN IMMEDIATE")
    try:
        counts = migrate(conn, old, new)
        problems = conn.execute("PRAGMA foreign_key_check").fetchall()
        if problems:
            raise SystemExit(f"foreign key check failed: {problems[:5]}")
        conn.execute("ROLLBACK" if dry else "COMMIT")
    except BaseException:
        if conn.in_transaction:
            conn.execute("ROLLBACK")
        raise
    left = conn.execute(
        "SELECT count(*) FROM tool_lists, json_each(tool_lists.tools) AS tool"
        " WHERE json_type(tool.value,?) IS NOT NULL",
        (f"$.{old}",),
    ).fetchone()[0]
    print(
        json.dumps(
            {
                "store": store,
                "schema": version,
                "renamed": f"{old} -> {new}",
                "dry_run": dry,
                **counts,
                f"tools_still_{old}": left if not dry else None,
            }
        )
    )
    if left and not dry:
        raise SystemExit(f"{left} declared tools still say {old!r}")


if __name__ == "__main__":
    main(sys.argv[1:])
