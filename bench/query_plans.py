"""EXPLAIN QUERY PLAN for runtime statement literals in the store modules.

Run from the repository root against a store at the current schema (open it
once with the current binary to migrate a copy). Flags full scans on growing
tables and temporary B-trees. Structural cases are listed, not counted:
`SCAN CONSTANT ROW` from EXISTS subqueries, the recursive-CTE step scans that
are bounded by a window or one turn, and per-turn sorts over a turn's tool
rows, plus the singleton configuration and schema metadata. Statements inside
the one-time migration function are skipped. Exit
status 1 when any runtime statement scans a table by an unindexed column.

    .local/venv/bin/python -m bench.query_plans .local/bench/some/state.sqlite
"""
import re
import sqlite3
import sys
from pathlib import Path

STRUCTURAL_SCANS = {'SCAN CONSTANT ROW', 'SCAN c', 'SCAN chain',
                    'SCAN configuration', 'SCAN sqlite_master'}


def statements(source):
    constants = dict(re.findall(r'const (COLUMNS|READING_ITEM): &str = "([^"]*)";', source, re.S))
    literals = re.findall(r'r?#?"((?:[^"\\]|\\.)*)"#?', source, re.S)
    for literal in literals:
        text = re.sub(r'\s+', ' ', literal.replace('\\n', ' ')).strip()
        if not re.match(r'(WITH|SELECT|INSERT|UPDATE|DELETE)\b', text) or ';' in text:
            continue
        for placeholder, name in (('{READING_ITEM}', 'READING_ITEM'), ('{}', 'COLUMNS')):
            if placeholder in text:
                text = text.replace(placeholder, constants[name])
        # Both delete_bot and prune generate one statement for each table.
        # Only prune's processes variant excludes still-running commands.
        tables = ('artifacts', 'processes', 'tools') if '{table}' in text else (None,)
        for table in tables:
            variant = text
            if table is not None:
                variant = variant.replace('{table}', table).replace('{key}', 'turn')
                variant = variant.replace('{finished}', "AND status!='running'" if table == 'processes' else '')
            yield re.sub(r'\s+', ' ', variant).strip()


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
    conn = sqlite3.connect(sys.argv[1])
    conn.execute('PRAGMA foreign_keys=ON')  # Include the daemon's constraint-check plans.
    seen, scans, structural = set(), [], []
    for statement in statements(source):
        if statement in seen:
            continue
        seen.add(statement)
        numbered = [int(n) for n in re.findall(r'\?(\d+)', statement)]
        count = max(numbered) if numbered else statement.count('?')
        try:
            rows = conn.execute('EXPLAIN QUERY PLAN ' + statement, tuple([1] * count)).fetchall()
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
          f'(SQLite {sqlite3.sqlite_version}, foreign_keys=ON)')
    return 1 if scans else 0


if __name__ == '__main__':
    sys.exit(main())
