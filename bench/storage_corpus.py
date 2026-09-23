"""Create an isolated payload corpus for the native storage prototype.

Synthetic profiles distinguish repetition, varied code/diagnostics, and
incompressible bytes. --store exports bytes from a stopped synthetic soak
store without changing it. Corpus files can contain source transcript data:
keep them in the ignored local directory. Reports contain only aggregates.
"""
import argparse
import hashlib
import json
import random
import sqlite3
from pathlib import Path


def synthetic(profile, count):
    rng = random.Random(7)
    for n in range(count):
        size = (256, 4096, 65536, 1048576)[n % 4]
        if profile == 'repeated':
            data = b'n' * size
        elif profile == 'entropy':
            data = rng.randbytes(size)
        else:
            parts = []
            remaining = size
            while remaining > 0:
                line = (f'error[E{rng.randrange(10000):04d}]: mismatched type at src/module_{n}.rs:'
                        f'{rng.randrange(10000)}\n'
                        f'fn operation_{rng.randrange(100000)}(value: usize) -> usize {{ '
                        f'value.wrapping_add({rng.randrange(1000000)}) }}\n').encode()
                parts.append(line[:remaining])
                remaining -= len(line)
            data = b''.join(parts)
        yield f'{profile}:{size}', data


def export(store):
    with sqlite3.connect(Path(store).resolve().as_uri() + '?mode=ro', uri=True) as db:
        db.execute('BEGIN')
        columns = {row[1] for row in db.execute('PRAGMA table_info(artifacts)')}
        if 'raw_bytes' in columns and db.execute('SELECT EXISTS(SELECT 1 FROM artifacts WHERE raw_bytes>0)').fetchone()[0]:
            raise ValueError('corpus export requires raw artifacts; export decoded bytes through the artifact protocol first')
        for kind, query in (
                ('node', 'SELECT item FROM nodes ORDER BY id'),
                ('prompt', 'SELECT CAST(prompt AS BLOB) FROM turns ORDER BY id'),
                ('artifact', 'SELECT data FROM artifacts ORDER BY turn,call_id,stream')):
            for (data,) in db.execute(query):
                yield kind, bytes(data)


def create(path, records):
    # Refuse to replace an existing corpus, including the input database.
    path = Path(path)
    with path.open('xb'):
        pass
    digest, count, size = hashlib.sha256(), 0, 0
    with sqlite3.connect(path) as db:
        db.execute('CREATE TABLE payloads(id INTEGER PRIMARY KEY,kind TEXT NOT NULL,data BLOB NOT NULL)')
        for count, (kind, data) in enumerate(records, 1):
            kind_bytes = kind.encode()
            digest.update(len(kind_bytes).to_bytes(8, 'little'))
            digest.update(kind_bytes)
            digest.update(len(data).to_bytes(8, 'little'))
            digest.update(data)
            size += len(data)
            db.execute('INSERT INTO payloads VALUES (?,?,?)', (count, kind, data))
    return {'records': count, 'payload_bytes': size, 'sha256': digest.hexdigest()}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    source = parser.add_mutually_exclusive_group(required=True)
    source.add_argument('--store', type=Path)
    source.add_argument('--profile', choices=('repeated', 'varied', 'entropy'))
    parser.add_argument('--count', type=int, default=128)
    parser.add_argument('--out', type=Path, required=True)
    args = parser.parse_args()
    if args.count < 1:
        parser.error('--count must be positive')
    report = args.out.with_name(args.out.name + '.json')
    for path in (args.out, report):
        if path.exists() or path.is_symlink():
            parser.error(f'refusing to overwrite {path}')
    args.out.parent.mkdir(parents=True, exist_ok=True)
    result = create(args.out, export(args.store) if args.store else synthetic(args.profile, args.count))
    with report.open('x') as output:
        output.write(json.dumps(result, indent=2) + '\n')
    print(json.dumps(result))


if __name__ == '__main__':
    main()
