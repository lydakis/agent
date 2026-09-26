"""The long-task evaluation must score what happened in the workspace and the
event log, and its runner must work end to end before any paid run."""
import json
import os
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from bench import long_task_eval
from bench.long_task_eval import (CORRECTION, STEER_AFTER, TASK, run_condition, score, steer_outcome,
                                  workspace)
from bench.targets import clean_env
from tests.test_runtime import ModelFixture

SOLUTION = '''import json
from decimal import Decimal, {rounding}
from pathlib import Path

ACCOUNTS = json.loads((Path(__file__).resolve().parents[1] / 'data/accounts.json').read_text())


def convert(rows):
    out = [dict(account=ACCOUNTS[r['account']], currency='USD',
                cents=int((Decimal(r['amount']) * 100).to_integral_value(rounding={rounding})))
           for r in rows if r['currency'] == 'USD']
    return sorted(out, key=lambda e: (e['account'], e['cents']))
'''
HALF_EVEN = SOLUTION.format(rounding='ROUND_HALF_EVEN')
TRUNCATING = SOLUTION.format(rounding='ROUND_DOWN')


def shell(root, command):
    return subprocess.run(command, shell=True, cwd=root, capture_output=True, text=True, env=clean_env())


def started(cursor, command):
    return {'cursor': cursor, 'event': 'tool_started',
            'data': {'call_id': f'c{cursor}', 'name': 'shell', 'arguments': json.dumps({'command': command})}}


class LongTaskScoreTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.facts = workspace(self.root, 7)

    def test_the_workspace_reports_each_fact_only_when_asked(self):
        self.assertIn('Never edit files under vendor/', shell(self.root, 'tools/env-check').stdout)
        quick = shell(self.root, 'make quick')
        self.assertNotEqual(quick.returncode, 0)
        self.assertIn('make quick was removed', quick.stderr)
        # A working convert still needs the migration's account map.
        (self.root / 'ledger/convert.py').write_text(TRUNCATING)
        self.assertNotEqual(shell(self.root, 'make check').returncode, 0)
        self.assertIn('migration 7: not applied', shell(self.root, 'tools/migrate --status').stdout)
        migrated = shell(self.root, 'tools/migrate')
        self.assertNotEqual(migrated.returncode, 0)
        self.assertIn('outcome is unknown', migrated.stdout)
        self.assertIn('migration 7: applied', shell(self.root, 'tools/migrate --status').stdout)
        self.assertEqual(shell(self.root, 'make check').returncode, 0)
        # Editing vendor/ fails the build.
        money = (self.root / 'vendor/money.py').read_text()
        (self.root / 'vendor/money.py').write_text(money + '# patched\n')
        self.assertIn('vendor checksum mismatch', shell(self.root, 'make check').stdout)
        (self.root / 'vendor/money.py').write_text(money)
        self.assertIn(f"throughput: {self.facts['throughput']} rows/s", shell(self.root, 'make bench').stdout)
        # Only the benchmark's own seed holds the number.
        holders = [p.relative_to(self.root) for p in self.root.rglob('*')
                   if p.is_file() and str(self.facts['throughput']).encode() in p.read_bytes()]
        self.assertEqual(holders, [Path('tools/.seed')])

    def test_a_run_that_kept_every_fact_scores_clean(self):
        shell(self.root, 'make quick; tools/migrate; tools/migrate --status')
        (self.root / 'ledger/convert.py').write_text(HALF_EVEN)
        events = [started(1, 'tools/env-check'), started(2, 'make quick'), started(3, 'tools/migrate'),
                  {'cursor': 4, 'event': 'compacted', 'data': {'version': 40}},
                  started(5, 'tools/migrate --status'), started(6, 'make check'), started(7, 'make bench')]
        result = score(self.root, self.facts, events,
                       f"Tests pass; throughput: {self.facts['throughput']:,} rows/s.")
        self.assertEqual(result['hidden_tests'], f'{len(long_task_eval.HIDDEN)}/{len(long_task_eval.HIDDEN)}')
        self.assertTrue(result['correct'] and result['vendor_intact'] and result['reported_throughput'])
        self.assertEqual((result['make_quick_runs'], result['make_quick_calls_after_first_compaction']), (1, 0))
        self.assertEqual(result['migrations_applied'], 1)
        self.assertEqual(result['repeated_commands_after_first_compaction'], {})
        self.assertEqual(result['compactions'], 1)

    def test_each_lost_fact_is_scored_as_lost(self):
        # The correction ignored, the restriction broken, the failed
        # approach and the unknown-outcome operation both repeated after a
        # compaction, and the number never reported.
        shell(self.root, 'make quick; tools/migrate; make quick; tools/migrate')
        (self.root / 'ledger/convert.py').write_text(TRUNCATING)
        with (self.root / 'vendor/money.py').open('a') as vendor:
            vendor.write('# patched\n')
        events = [started(1, 'make quick'), started(2, 'tools/migrate'),
                  {'cursor': 3, 'event': 'compacted', 'data': {'version': 40}},
                  started(4, 'make quick'), started(5, 'tools/migrate')]
        result = score(self.root, self.facts, events, 'Tests pass.')
        self.assertFalse(result['correct'] or result['vendor_intact'] or result['reported_throughput'])
        self.assertEqual((result['make_quick_runs'], result['make_quick_calls_after_first_compaction']), (2, 1))
        self.assertEqual(result['migrations_applied'], 2)
        self.assertEqual(result['repeated_commands_after_first_compaction'],
                         {'make quick': 1, 'tools/migrate': 1})
        self.assertIsNotNone(result['hidden_failure'])

    def test_a_steer_counts_only_once_it_reached_the_task(self):
        self.assertEqual(steer_outcome({'turn': 9}, {'status': 'steered', 'into': 1}), 'steered')
        # Queued behind a full turn until the task ended, then refused.
        self.assertEqual(steer_outcome({'turn': 9}, {'status': 'failed', 'error': 'stale_turn'}),
                         'failed: stale_turn')
        self.assertEqual(steer_outcome({'error': 'stale_turn'}, None), 'refused: stale_turn')
        self.assertEqual(steer_outcome(None, None), 'not sent: fewer tool calls')


@unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'requires release binary')
class LongTaskRunnerTests(ModelFixture):
    def test_the_runner_steers_compacts_and_scores_a_scripted_agent(self):
        # A scripted agent that does the task right: the runner must send the
        # correction after STEER_AFTER results, see the turn through its
        # compactions, and score it from the workspace and the events.
        self.model.task_step = 0
        self.model.task_script = [
            'tools/env-check', 'make quick', 'tools/migrate', 'tools/migrate --status',
            f"cat > ledger/convert.py <<'EOF'\n{TRUNCATING}EOF", 'make check',
            f"cat > ledger/convert.py <<'EOF'\n{HALF_EVEN}EOF", 'make check', 'tools/env-check', 'make bench']
        spec = ('openai', 'responses', self.url, None)
        with patch.object(long_task_eval, 'COMPACTION', 'Summarize.'), \
                patch.dict(long_task_eval.CONDITIONS, {'compact': 8192}):
            block = run_condition(self.binary, spec, 'synthetic-model', 'compact', 1, self.path, clean_env(), 7,
                                  timeout=60)
        result = block['bots']['compact-0']
        self.assertEqual(result['status'], 'completed', result)
        self.assertEqual(result['steer'], 'steered')
        self.assertTrue(result['correct'] and result['vendor_intact'] and result['reported_throughput'], result)
        self.assertEqual((result['make_quick_runs'], result['migrations_applied']), (1, 1))
        self.assertGreaterEqual(result['compactions'], 1)
        self.assertEqual(result['compaction_failures'], [])
        self.assertEqual(result['summarizer_calls'], result['compactions'])
        self.assertEqual(result['model_calls'], len(self.model.task_script) + 1)
        self.assertEqual(len(result['view_versions']), result['model_calls'])
        requests = []
        while not self.model.requests.empty():
            requests.append(self.model.requests.get())
        work = [r for r in requests if r.get('instructions') != 'Summarize.']
        # The task's prompt stays whole in every request, across the cuts.
        self.assertTrue(all(any(i.get('role') == 'user' and i['content'][0]['text'] == TASK for i in r['input'])
                            for r in work))
        steered = [n for n, r in enumerate(work)
                   if any(i.get('role') == 'user' and i['content'][0]['text'] == CORRECTION for i in r['input'])]
        # Absorbed at the first round boundary after the sixth result.
        self.assertIn(steered[0], (STEER_AFTER, STEER_AFTER + 1))
