import hashlib
import json
import sqlite3
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

from bench.storage_corpus import create, export, synthetic
from bench.storage_profile import profile


class StorageProfileTests(unittest.TestCase):
    def corpus_cli(self, path):
        return subprocess.run(
            [sys.executable, '-m', 'bench.storage_corpus', '--profile', 'varied',
             '--count', '4', '--out', str(path)],
            cwd=Path(__file__).resolve().parent.parent, capture_output=True, text=True)

    def test_corpus_and_report_have_distinct_paths_for_any_suffix(self):
        with tempfile.TemporaryDirectory() as directory:
            for name in ('corpus.json', 'corpus.sqlite'):
                with self.subTest(name=name):
                    path = Path(directory) / name
                    result = self.corpus_cli(path)
                    self.assertEqual(result.returncode, 0, result.stderr)
                    with sqlite3.connect(path) as db:
                        self.assertEqual(db.execute('SELECT count(*) FROM payloads').fetchone()[0], 4)
                    report = json.loads(path.with_name(path.name + '.json').read_text())
                    self.assertEqual(report, json.loads(result.stdout))

    def test_corpus_cli_refuses_collisions_before_creating_either_output(self):
        for existing in ('corpus', 'report'):
            with self.subTest(existing=existing), tempfile.TemporaryDirectory() as directory:
                corpus = Path(directory) / 'corpus.sqlite'
                report = corpus.with_name(corpus.name + '.json')
                occupied, other = (corpus, report) if existing == 'corpus' else (report, corpus)
                occupied.write_bytes(b'keep these bytes')
                result = self.corpus_cli(corpus)
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(occupied.read_bytes(), b'keep these bytes')
                self.assertFalse(other.exists())

    def test_profile_and_export_preserve_exact_source_bytes(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'state.sqlite'
            item = json.dumps({'role': 'user', 'content': [{'text': 'é\0hello'}]}).encode()
            with sqlite3.connect(path) as db:
                db.executescript('''CREATE TABLE nodes(id INTEGER,turn INTEGER,item BLOB);
                    CREATE TABLE turns(id INTEGER,prompt TEXT);
                    CREATE TABLE retained_turns(turn INTEGER);
                    CREATE TABLE artifacts(turn INTEGER,call_id TEXT,stream TEXT,data BLOB);
                    CREATE TABLE events(id INTEGER);
                    CREATE TABLE compactions(node INTEGER);''')
                db.execute('INSERT INTO nodes VALUES (1,1,?)', (item,))
                db.execute('INSERT INTO turns VALUES (1,?)', ('é\0hello',))
                db.execute('INSERT INTO artifacts VALUES (1,\'x\',\'stdout\',?)', (b'\xff\0output',))
            before = hashlib.sha256(path.read_bytes()).hexdigest()
            result = profile(path)
            self.assertEqual(result['duplicate_prompt_bytes'], len('é\0hello'.encode()))
            self.assertEqual(result['payload_bytes'], {'nodes': len(item), 'turn_prompts': 8, 'artifacts': 8})
            self.assertEqual(list(export(path)), [('node', item), ('prompt', 'é\0hello'.encode()),
                                                 ('artifact', b'\xff\0output')])
            self.assertEqual(before, hashlib.sha256(path.read_bytes()).hexdigest())

            # Count each turn's redundant prompt once even if later messages echo it.
            with sqlite3.connect(path) as db:
                db.execute('INSERT INTO nodes VALUES (2,1,?)', (item,))
                db.execute('INSERT INTO turns VALUES (2,?)', ('assistant-only',))
                db.execute('INSERT INTO nodes VALUES (3,2,?)', (json.dumps({
                    'role': 'assistant', 'content': 'assistant-only'}).encode(),))
            self.assertEqual(profile(path)['duplicate_prompt_bytes'], 8)

    def test_compressed_artifact_profile_distinguishes_physical_and_logical_bytes(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'state.sqlite'
            with sqlite3.connect(path) as db:
                db.executescript("""CREATE TABLE nodes(id INTEGER,turn INTEGER,item BLOB);
                    CREATE TABLE turns(id INTEGER,prompt TEXT);
                    CREATE TABLE retained_turns(turn INTEGER);
                    CREATE TABLE artifacts(turn INTEGER,call_id TEXT,stream TEXT,data BLOB,raw_bytes INTEGER);
                    CREATE TABLE events(id INTEGER); CREATE TABLE compactions(node INTEGER);
                    INSERT INTO artifacts VALUES (1,'call','stdout',x'0102',100000);""")
            result = profile(path)
            self.assertEqual(result['stored_artifact_bytes'], 2)
            self.assertEqual(result['payload_bytes']['artifacts'], 100000)
            with self.assertRaisesRegex(ValueError, 'raw artifacts'):
                list(export(path))

    def test_corpus_is_deterministic_and_refuses_overwrite(self):
        for kind in ('repeated', 'varied', 'entropy'):
            with self.subTest(kind=kind), tempfile.TemporaryDirectory() as directory:
                path = Path(directory) / 'corpus.sqlite'
                first = create(path, synthetic(kind, 4))
                second = create(Path(directory) / 'copy.sqlite', synthetic(kind, 4))
                self.assertEqual(first, second)
                self.assertEqual(first['records'], 4)
                with self.assertRaises(FileExistsError):
                    create(path, [])
