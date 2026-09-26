"""Attribute a stopped daemon's SQLite store without printing its contents.

Run against snapshots at successive workload boundaries to separate retained
working sets from accumulating history. No writes, VACUUM, or checkpointing.
"""
import argparse
import json
import sqlite3
from pathlib import Path


def profile(path):
    path = Path(path).resolve()
    with sqlite3.connect(path.as_uri() + '?mode=ro', uri=True) as db:
        db.execute('BEGIN')
        page_size = db.execute('PRAGMA page_size').fetchone()[0]
        result = {
            'schema': 'storage_profile_v1',
            'sqlite_version': sqlite3.sqlite_version,
            'schema_version': db.execute('PRAGMA user_version').fetchone()[0],
            'database_bytes': db.execute('PRAGMA page_count').fetchone()[0] * page_size,
            'free_bytes': db.execute('PRAGMA freelist_count').fetchone()[0] * page_size,
            'wal_file_bytes': (path.with_name(path.name + '-wal').stat().st_size
                               if path.with_name(path.name + '-wal').exists() else 0),
            'rows': {},
        }
        for table in ('nodes', 'turns', 'retained_turns', 'artifacts', 'events', 'compactions'):
            result['rows'][table] = db.execute(f'SELECT count(*) FROM {table}').fetchone()[0]
        result['node_payloads'] = [dict(kind=kind, rows=count, bytes=size) for kind, count, size in db.execute(
            "SELECT coalesce(json_extract(CAST(item AS TEXT),'$.type'),"
            "json_extract(CAST(item AS TEXT),'$.role'),'other'),count(*),sum(length(item)) "
            'FROM nodes GROUP BY 1')]
        result['stored_artifact_bytes'] = db.execute('SELECT coalesce(sum(length(data)),0) FROM artifacts').fetchone()[0]
        result['payload_bytes'] = dict(zip(('nodes', 'turn_prompts', 'artifacts'), db.execute(
            'SELECT (SELECT coalesce(sum(length(item)),0) FROM nodes),'
            '(SELECT coalesce(sum(length(CAST(prompt AS BLOB))),0) FROM turns),'
            '(SELECT coalesce(sum(CASE WHEN raw_bytes>0 THEN raw_bytes ELSE length(data) END),0) FROM artifacts)').fetchone()))
        # Count only exact duplicate text, not all turns or guessed JSON overhead.
        result['duplicate_prompt_bytes'] = db.execute(
            "SELECT coalesce(sum(length(CAST(t.prompt AS BLOB))),0) FROM turns t "
            "WHERE EXISTS (SELECT 1 FROM nodes n WHERE n.turn=t.id "
            "AND json_extract(CAST(n.item AS TEXT),'$.role')='user' AND t.prompt=coalesce("
            "json_extract(CAST(n.item AS TEXT),'$.content[0].text'),"
            "json_extract(CAST(n.item AS TEXT),'$.content')))").fetchone()[0]
        try:
            result['objects'] = [dict(name=name, bytes=size, payload_bytes=payload, unused_bytes=unused)
                                 for name, size, payload, unused in db.execute(
                                     'SELECT name,sum(pgsize),sum(payload),sum(unused) FROM dbstat '
                                     'GROUP BY name ORDER BY sum(pgsize) DESC')]
        except sqlite3.OperationalError as error:
            if 'no such table: dbstat' not in str(error):
                raise
            result['objects'] = None
        return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--store', type=Path, required=True)
    parser.add_argument('--out', type=Path, required=True)
    args = parser.parse_args()
    result = profile(args.store)
    args.out.parent.mkdir(parents=True, exist_ok=True)
    with args.out.open('x') as output:
        output.write(json.dumps(result, indent=2) + '\n')
    print(json.dumps({key: result[key] for key in ('database_bytes', 'free_bytes', 'payload_bytes',
                                                'duplicate_prompt_bytes')}))


if __name__ == '__main__':
    main()
