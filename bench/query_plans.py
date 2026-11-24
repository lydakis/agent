"""EXPLAIN QUERY PLAN for runtime statement literals in the store modules.

Run from the repository root against a store at the current schema (open it
once with the current binary to migrate a copy). Flags full scans on growing
tables and temporary B-trees. Structural cases are listed, not counted:
`SCAN CONSTANT ROW` from EXISTS subqueries, the recursive-CTE step scans that
are bounded by a window or one turn, or by one bot's descendants (`kin`,
each step found by the `bots_maker` index), the `held` turns (each half
found by the `turns_ready` or `turns_paced` index), and per-turn sorts over a turn's tool
rows, plus schema metadata (`sqlite_sequence` holds one row per
autoincrement table). Statements inside
the one-time migration function are skipped. Exit
status 1 when any runtime statement scans a table by an unindexed column.

    .local/venv/bin/python -m bench.query_plans .local/bench/some/state.sqlite
"""
import re
import sqlite3
import sys
from pathlib import Path

# Clauses a listing formats in by name, each planned with every value it
# takes: keep in step with `list`, `usage`, `turns` and `messages` in
# src/store/db.rs.
OPTIONAL = {'{glob}': ('', " AND t.bot GLOB :pattern AND t.bot >= :prefix AND t.bot < :prefix || CAST(x'ff' AS TEXT)",
                       " AND t.bot GLOB :pattern AND t.bot >= :prefix AND t.bot < :prefix || CAST(x'ff' AS TEXT)"
                       ' AND t.started_ms < :until',
                       ' AND t.bot IN (SELECT value FROM json_each(:names)) AND +t.started_ms < :until'),
            '{from}': ('name > ?1', "name >= max(?1 || char(1), ?5) AND name < ?5 || CAST(x'ff' AS TEXT) AND name GLOB ?3",
                       'name > ?1 AND name IN (SELECT value FROM json_each(?6))'),
            '{maker}': ('', ' AND created_by_id = ?4'),
            '{running}': ('', ' AND running_turn IS NOT NULL'),
            '{open}': ('', ' AND closed_ms IS NULL'),
            '{order}': ('ASC', 'DESC'),
            '{line}': ('', "AND m.id IN (SELECT id FROM messages WHERE bot=?1 AND status='queued' "
                           "UNION ALL SELECT id FROM messages WHERE bot=?1 AND status='ready')"),
            '{bots}': ('m.bot=?1', 'm.bot GLOB ?1'),
            '{kinds}': ('', ' AND m.from_bot IS NULL AND m.origin IS NULL', ' AND m.from_bot IS NOT NULL'),
            '{quiet}': ('', ' AND running_turn IS NULL AND COALESCE(last_finished_ms,last_started_ms,-1) < ?5'),
            '{text}': ('',),
            '{text_node}': ('',)}

STRUCTURAL_SCANS = {'SCAN CONSTANT ROW', 'SCAN c', 'SCAN chain', 'SCAN held', 'SCAN kin', 'SCAN sqlite_master',
                    'SCAN sqlite_sequence'}


def unpacked(name=''):
    """What `unpacked!` in src/store/db.rs writes: a node's item, read
    through `unpack` when it is packed."""
    n = f'{name}.' if name else ''
    return f'CASE WHEN {n}packed IS NULL THEN {n}item ELSE unpack({n}item,{n}packed) END'


def expanded(source):
    """`source` with the SQL its macros build written out: each one-argument
    `macro_rules!` whose body is a `concat!` of literals, `unpacked!`, and
    every `concat!` of literals then joined into one literal."""
    for name, arg, body in re.findall(
            r'macro_rules! (\w+) \{\s*\(\$(\w+):expr\) => \{\s*concat!\((.*?)\)\s*\};\s*\}', source, re.S):
        source = re.sub(name + r'!\(((?:[^()]|\([^()]*\))*)\)',
                        lambda m, body=body, arg=arg: 'concat!(' + body.replace('$' + arg, m.group(1)) + ')', source)
    source = re.sub(r'unpacked!\((\w*)\)', lambda m: '"' + unpacked(m.group(1)) + '"', source)

    def joined(match):
        parts = re.findall(r'"((?:[^"\\]|\\.)*)"', match.group(1))
        return '"' + ''.join(parts) + '"'
    return re.sub(r'concat!\(\s*((?:"(?:[^"\\]|\\.)*"\s*,?\s*)+)\)', joined, source)


def statements(source):
    source = expanded(source)
    constants = {name: value.replace('\\"', '"') for name, value in re.findall(
        r'const (COLUMNS|READING_ITEM|TURN_VIEW|PICTURELESS|REPLY_NODE|HELD|MESSAGE_FROM|MESSAGE_TEXT|MESSAGE_NODE|'
        r'PREVIEW|PREVIEW_NODE|N_ITEM): &str =\s*"((?:[^"\\]|\\.)*)";', source, re.S)}
    literals = re.findall(r'r?#?"((?:[^"\\]|\\.)*)"#?', source, re.S)
    for literal in literals:
        text = re.sub(r'\s+', ' ', literal.replace('\\n', ' ')).strip()
        if not re.match(r'(WITH|SELECT|INSERT|UPDATE|DELETE)\b', text) or ';' in text:
            continue
        for name, value in constants.items():
            text = text.replace('{}' if name == 'COLUMNS' else '{' + name + '}', value)
        # Both delete_bot and prune generate one statement for each table.
        # Only prune's processes variant excludes still-running commands.
        tables = ('artifacts', 'processes', 'tools') if '{table}' in text else (None,)
        for table in tables:
            variant = text
            if table is not None:
                variant = variant.replace('{table}', table).replace('{key}', 'turn')
                variant = variant.replace('{finished}', "AND status!='running'" if table == 'processes' else '')
            variants = [variant]
            for placeholder, values in OPTIONAL.items():
                variants = [v.replace(placeholder, value) for v in variants for value in values]
            for variant in dict.fromkeys(variants):
                yield re.sub(r'\s+', ' ', variant).strip()


def connect(path):
    """A connection that plans as the daemon's do: with the query planner
    stability guarantee (`plan_stably` in src/store/db.rs), so no plan reads
    a bound value, and with foreign keys on, to include the daemon's
    constraint-check plans."""
    conn = sqlite3.connect(path)
    # The daemon's `unpack`, for planning: what it returns does not matter.
    conn.create_function('unpack', 2, lambda item, _: item, deterministic=True)
    conn.setconfig(sqlite3.SQLITE_DBCONFIG_ENABLE_QPSG, True)
    conn.execute('PRAGMA foreign_keys=ON')
    return conn


def main():
    if len(sys.argv) != 2:
        print(__doc__)
        return 2
    root = Path(__file__).resolve().parent.parent
    source = '\n'.join((root / name).read_text().split('#[cfg(test)]', 1)[0] for name in ('src/store/db.rs', 'src/store/artifact.rs'))
    # One-time migrations may read a whole table by design; audit the runtime
    # paths. Every `fn migrate*` is one, including the backfills it calls.
    while '\nfn migrate' in source:
        head, _, tail = source.partition('\nfn migrate')
        source = head + '\nfn ' + tail.partition('\nfn ')[2]
    conn = connect(sys.argv[1])
    seen, scans, structural = set(), [], []
    for statement in statements(source):
        if statement in seen:
            continue
        seen.add(statement)
        # As SQLite numbers them: `?N` is N, a bare `?` one past the
        # largest so far.
        count = 0
        for number in re.findall(r'\?(\d*)', statement):
            count = max(count, int(number)) if number else count + 1
        # `:name`, as `usage` names its own.
        named = {name: 1 for name in re.findall(r'(?<![\w:]):([a-z_]+)', statement)}
        try:
            rows = conn.execute('EXPLAIN QUERY PLAN ' + statement, named or tuple([1] * count)).fetchall()
        except sqlite3.Error as error:
            print(f'cannot plan: {statement[:100]} -> {error}')
            return 1
        steps = [row[3] for row in rows]
        flagged = [s for s in steps if (s.startswith('SCAN') and 'INDEX' not in s) or 'TEMP B-TREE' in s]
        if not flagged:
            continue
        if all(s in STRUCTURAL_SCANS or 'TEMP B-TREE' in s for s in flagged):
            structural.append((statement, steps))
        else:
            scans.append((statement, steps))
    for statement, steps in scans:
        print('SCAN:', statement[:140])
        print('     ', ' | '.join(steps))
    print(f'{len(seen)} statements planned, {len(scans)} table scans, {len(structural)} structural flags '
          f'(SQLite {sqlite3.sqlite_version}, stable plans, foreign_keys=ON)')
    return 1 if scans else 0


if __name__ == '__main__':
    sys.exit(main())
