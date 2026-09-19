"""The offline SQL audit must reject missing indexes on growing tables."""
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile
import unittest

from bench.query_plans import statements


class QueryPlanTests(unittest.TestCase):
    def test_deletion_variants_preserve_running_processes_only_when_pruning(self):
        root = Path(__file__).resolve().parent.parent
        source = (root / 'src/store/db.rs').read_text()
        queries = set(statements(source))
        for table in ('artifacts', 'processes', 'tools'):
            with self.subTest(table=table):
                self.assertIn(f'DELETE FROM {table} WHERE turn IN (SELECT id FROM turns WHERE bot=?)', queries)
                prune = (f'DELETE FROM {table} WHERE turn IN '
                         '(SELECT turn FROM retained_turns WHERE bot=?1 AND turn<?2 AND turn IS NOT ?3)')
                self.assertIn(prune + (" AND status!='running'" if table == 'processes' else ''), queries)
        self.assertNotIn('DELETE FROM processes WHERE turn IN '
                         '(SELECT turn FROM retained_turns WHERE bot=?1 AND turn<?2 AND turn IS NOT ?3)', queries)

    def test_current_schema_passes_and_missing_indexes_fail(self):
        root = Path(__file__).resolve().parent.parent
        source = (root / 'src/store/db.rs').read_text()
        ddl = source.split('tx.execute_batch("', 1)[1].split('")?;', 1)[0]
        # Use the current runtime's schema and statements, not simplified copies.
        for index, table in [(None, None), ('checkpoints_head', 'checkpoints'),
                             ('processes_turn', 'processes'),
                             ('turns_running', 'turns'), ('turns_waiting', 'turns'),
                             ('processes_running', 'processes')]:
            with self.subTest(index=index), tempfile.TemporaryDirectory() as directory:
                store = Path(directory) / 'store.sqlite'
                with sqlite3.connect(store) as conn:
                    conn.executescript(ddl)
                    if index:
                        conn.execute(f'DROP INDEX {index}')
                result = subprocess.run(
                    [sys.executable, '-m', 'bench.query_plans', str(store)],
                    cwd=root, capture_output=True, text=True, timeout=10)
                self.assertEqual(result.returncode, 1 if index else 0,
                                 result.stdout + result.stderr)
                if index:
                    self.assertIn(f'SCAN {table}', result.stdout)
                else:
                    self.assertIn('0 table scans', result.stdout)


if __name__ == '__main__':
    unittest.main()
