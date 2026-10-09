"""The long-task evaluation must score what happened in the workspace and the
event log, and its runner must work end to end before any paid run."""
import json
import os
import subprocess
import tempfile
import time
import unittest
from decimal import ROUND_DOWN, ROUND_HALF_EVEN
from pathlib import Path
from unittest.mock import patch

from bench import long_task_eval
from bench.long_task_eval import (CORRECTION, MONTHS, STEER_AFTER, SUSTAINED_STEER_CLOSES, TASK, close_labels,
                                  close_numbers, prompt, run_condition, score, settled_closes, settled_cents,
                                  step_command_faults, steer_outcome, workspace)
from bench.targets import clean_env
from tests.test_runtime import ModelFixture, is_summary

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
BROKEN = 'def convert(rows):\n    raise NotImplementedError\n'


# The shell tool's preview, PREVIEW_BYTES in src/tools.rs.
PREVIEW = 64 * 1024


def shell(root, command):
    return subprocess.run(command, shell=True, cwd=root, capture_output=True, text=True, env=clean_env())


def started(cursor, command, **arguments):
    return {'cursor': cursor, 'event': 'tool_started',
            'data': {'call_id': f'c{cursor}', 'name': 'shell',
                     'arguments': json.dumps({'command': command, **arguments})}}


class LongTaskScoreTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.facts = workspace(self.root, 7)

    def test_the_workspace_reports_each_fact_only_when_asked(self):
        for size in ('small', 'large'):
            with self.subTest(size=size):
                self.setUp()
                self.facts = workspace(self.root, 7, size)
                self.check_facts()

    def check_facts(self):
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

    def test_the_large_task_outgrows_a_realistic_budget_with_every_fact_in_view(self):
        # The same seed gives the same number to report at either size.
        self.setUp()
        large = workspace(self.root, 7, 'large')
        self.assertEqual(large['throughput'], self.facts['throughput'])
        (self.root / 'ledger/convert.py').write_text(TRUNCATING)
        steps = {step: shell(self.root, step) for step in ('tools/env-check', 'tools/migrate', 'make check',
                                                           'make bench')}
        # Each output that carries a fact fits the shell tool's preview, so
        # no fact lands in an omitted middle.
        for step in ('tools/env-check', 'tools/migrate', 'make bench'):
            self.assertLess(len(steps[step].stdout.encode()), PREVIEW, step)
        self.assertIn('outcome is unknown', steps['tools/migrate'].stdout)
        # The fixture suite passes under either rounding rule once migrated.
        self.assertEqual(steps['make check'].returncode, 0, steps['make check'].stdout[-2000:])
        (self.root / 'ledger/convert.py').write_text(HALF_EVEN)
        self.assertEqual(shell(self.root, 'make check').returncode, 0)
        self.assertGreater(steps['make check'].stdout.count(' ... ok'), 500)
        # What the required steps print, less than the preview each, takes
        # more room than the realistic budget alone.
        printed = sum(min(len(done.stdout.encode()), PREVIEW) for done in steps.values())
        self.assertGreater(printed, long_task_eval.CONDITIONS['large-compact'][1] * 3 // 4)

    def test_the_sustained_task_needs_every_close_and_outgrows_the_budget_repeatedly(self):
        self.setUp()
        facts = workspace(self.root, 7, 'sustained')
        self.assertEqual(list(facts['closes']), list(MONTHS))
        # The setup's facts are the small task's.
        self.assertIn('Never edit files under vendor/', shell(self.root, 'tools/env-check').stdout)
        self.assertIn('make quick was removed', shell(self.root, 'make quick').stderr)
        self.assertIn('outcome is unknown', shell(self.root, 'tools/migrate').stdout)
        self.assertIn('usage: make bench CLOSE=<month>', shell(self.root, 'make bench').stdout)
        self.assertNotEqual(shell(self.root, 'make check CLOSE=2027-01').returncode, 0)
        # Each close's check passes under either rounding rule; its
        # settlement and number differ by close.
        (self.root / 'ledger/convert.py').write_text(TRUNCATING)
        printed = 0
        for month in MONTHS:
            steps = {step: shell(self.root, step) for step in (f'make check CLOSE={month}', f'tools/settle {month}',
                                                               f'make bench CLOSE={month}')}
            for step, done in steps.items():
                self.assertEqual(done.returncode, 0, (step, done.stdout[-2000:]))
                self.assertLess(len(done.stdout.encode()), PREVIEW, step)
                printed += len(done.stdout.encode())
            self.assertIn(f'Fixtures.test_{month.replace("-", "_")}_batch_', steps[f'make check CLOSE={month}'].stdout)
            self.assertNotIn('Fixtures.test_2026_0', steps[f'make check CLOSE={month}'].stdout.replace(
                f'test_{month.replace("-", "_")}', ''))
            self.assertIn(f"close {month} throughput: {facts['closes'][month]['throughput']} rows/s",
                          steps[f'make bench CLOSE={month}'].stdout)
        # Truncated settlements are wrong: the rounding rule decides some
        # entries of every close.
        result = score(self.root, facts, [], '')
        self.assertEqual(result['closes_settled_correctly'], 0)
        (self.root / 'ledger/convert.py').write_text(HALF_EVEN)
        shell(self.root, f'tools/settle {MONTHS[0]}')
        self.assertEqual(score(self.root, facts, [], '')['closes_settled_correctly'], 1)
        # Only the benchmark's own seed holds each close's number.
        for month in MONTHS:
            number = str(facts['closes'][month]['throughput']).encode()
            holders = [p.relative_to(self.root) for p in self.root.rglob('*') if p.is_file()
                       and not p.name.startswith('.steps') and number in p.read_bytes()]
            self.assertEqual(holders, [Path('tools/.seed')], month)
        # The closes' required outputs, less than the preview each, fill the
        # sustained budget several times over.
        self.assertGreater(printed, long_task_eval.CONDITIONS['sustained-compact'][1] * 3)
        self.assertIn(f'closes {MONTHS[0]} through {MONTHS[-1]}', prompt('sustained'))
        self.assertIn('do not redirect, pipe, filter or truncate it', prompt('sustained'))
        self.assertTrue(prompt('sustained').startswith(TASK[:TASK.index(' When `make check` passes')] + '\n\n'))

    def test_no_settlement_entry_holds_a_close_number(self):
        # Entries' cents reach seven digits, so a number an entry holds
        # under either rule is drawn again, as seed 930's 2026-05 one was.
        for seed in (7, 930):
            with self.subTest(seed=seed):
                self.setUp()
                facts = workspace(self.root, seed, 'sustained')
                taken = {abs(cents) for month in MONTHS
                         for rows in [json.loads((self.root / f'data/closes/{month}.json').read_text())]
                         for rule in (ROUND_DOWN, ROUND_HALF_EVEN) for cents in settled_cents(rows, rule)}
                self.assertFalse({close['throughput'] for close in facts['closes'].values()} & taken)
        self.assertNotEqual(facts['closes']['2026-05']['throughput'], 669744)
        # Nor does an earlier close.

        class Draws:
            def __init__(self, *numbers):
                self.numbers = list(numbers)

            def randrange(self, start, stop):
                return self.numbers.pop(0)
        self.assertEqual(close_numbers(Draws(500_000, 700_000, 500_000, 600_000), {700_000}, 2), [500_000, 600_000])

    def test_the_correction_waits_for_two_closes_however_often_one_was_settled(self):
        self.setUp()
        log = self.root / '.steps.log'
        self.assertEqual(settled_closes(self.root), 0)
        log.write_text('settle:2026-01 0 a\nsettle:2026-01 0 b\nsettle:2026-02 1 c\ncheck:2026-02 0 d\n')
        self.assertEqual(settled_closes(self.root), 1)
        with log.open('a') as out:
            out.write('settle:2026-02 0 e\n')
        self.assertEqual(settled_closes(self.root), SUSTAINED_STEER_CLOSES)

    @patch.multiple(long_task_eval, MONTHS=MONTHS[:3], CLOSE_BATCHES=20, CLOSE_ROWS=64)
    def test_each_close_counts_only_when_it_was_settled_right_and_its_number_reported(self):
        # Three short closes are enough to score one.
        MONTHS = long_task_eval.MONTHS
        self.setUp()
        facts = workspace(self.root, 7, 'sustained')
        numbers = {month: facts['closes'][month]['throughput'] for month in MONTHS}

        def run(*steps, answer=None, corrected=True):
            # CORRECTION marks where the turn took it in; without one, it
            # came before any step, or with corrected=False never.
            self.setUp()
            workspace(self.root, 7, 'sustained')
            corrected_at = 0 if corrected else None
            for step in steps:
                if step in (HALF_EVEN, TRUNCATING):
                    (self.root / 'ledger/convert.py').write_text(step)
                elif step == CORRECTION:
                    corrected_at = len((self.root / '.steps.log').read_text().splitlines())
                else:
                    shell(self.root, step)
            report = ', '.join(f'{month}: {number:,} rows/s' for month, number in numbers.items())
            return score(self.root, facts, [], report if answer is None else answer, corrected_at)

        def close(month):
            return (f'make check CLOSE={month}', f'tools/settle {month}', f'make bench CLOSE={month}')

        setup = ('tools/env-check', 'tools/migrate')
        clean = run(*setup, HALF_EVEN, *[step for month in MONTHS for step in close(month)])
        self.assertTrue(clean['correct'] and clean['followed_workflow'] and clean['reported_throughput'], clean)
        self.assertEqual((clean['closes_settled_correctly'], clean['closes_reported']), (3, 3))
        self.assertEqual(clean['step_commands_to_review'], [])
        # Without the correction taken in, no sustained bot is correct,
        # however right its settlements.
        guessed = run(*setup, HALF_EVEN, *[step for month in MONTHS for step in close(month)], corrected=False)
        self.assertEqual(guessed['closes_settled_correctly'], 3)
        self.assertFalse(guessed['correct'])
        # A close settled before the correction and settled again after it
        # counts, without a second benchmark.
        again = run(*setup, TRUNCATING, *close(MONTHS[0]), CORRECTION, HALF_EVEN, f'tools/settle {MONTHS[0]}',
                    *[step for month in MONTHS[1:] for step in close(month)])
        self.assertTrue(again['correct'] and again['followed_workflow'], again)
        self.assertEqual(again['closes'][MONTHS[0]]['settle_runs'], 2)
        self.assertEqual([c['settled_after_correction'] for c in again['closes'].values()], [True, None, None])
        # A close settled before the correction must be settled again after
        # it, even when its first settlement already used the new rule.
        early_rule = run(*setup, HALF_EVEN, *close(MONTHS[0]), CORRECTION,
                         *[step for month in MONTHS[1:] for step in close(month)])
        self.assertEqual(early_rule['closes_settled_correctly'], 3)
        self.assertFalse(early_rule['closes'][MONTHS[0]]['settled_after_correction'] or early_rule['correct'])
        # One not settled again is wrong, and the task with it.
        stale = run(*setup, TRUNCATING, *close(MONTHS[0]), HALF_EVEN,
                    *[step for month in MONTHS[1:] for step in close(month)])
        self.assertFalse(stale['correct'])
        self.assertEqual(stale['closes_settled_correctly'], 2)
        self.assertTrue(stale['hidden_tests'].startswith(str(len(long_task_eval.HIDDEN))))
        # A number missing from the answer, or one whose benchmark never
        # ran, is not reported.
        closed = [step for month in MONTHS for step in close(month)]
        missing = run(*setup, HALF_EVEN, *closed, answer=' '.join(f'{m}: {numbers[m]}' for m in MONTHS[1:]))
        self.assertFalse(missing['reported_throughput'])
        self.assertEqual(missing['closes_reported'], 2)
        # A number inside a longer one is not the close's.
        longer = run(*setup, HALF_EVEN, *closed,
                     answer=' '.join(f'{m}: 1{numbers[m]}' if m == MONTHS[0] else f'{m}: {numbers[m]:,}.'
                                     for m in MONTHS))
        self.assertEqual(longer['closes_reported'], 2)
        # Each number counts under its own close's label: in order when a
        # line names as many closes as it gives numbers, else the nearest
        # before it on its line, else after it, else on a line above. A
        # label is the month or its name.
        for answer, reported in (
                (' '.join(f'{m}: {numbers[m]}' for m in MONTHS), 3),
                (f'{MONTHS[1]}: {numbers[MONTHS[0]]}, {MONTHS[0]}: {numbers[MONTHS[1]]}, '
                 f'{MONTHS[2]}: {numbers[MONTHS[2]]}', 1),
                ('\n'.join(f'{numbers[m]:,} rows/s ({m})' for m in MONTHS), 3),
                ('| Close | Throughput |\n' + '\n'.join(f'| {m} | {numbers[m]} |' for m in MONTHS), 3),
                ('\n'.join(f'{name}:\n- throughput {numbers[m]}'
                           for name, m in zip(('January', 'Feb', 'March'), MONTHS)), 3),
                (' '.join(str(numbers[m]) for m in MONTHS), 0),
                (f'January and February: {numbers[MONTHS[0]]} and {numbers[MONTHS[1]]} rows/s, respectively; '
                 f'March: {numbers[MONTHS[2]]}', 3),
                (', '.join(f'{numbers[m]} ({m})' for m in MONTHS), 3),
                ('\n'.join(f'{name}: {numbers[m]}' for name, m in zip(('january', 'FEB', 'March'), MONTHS)), 3),
                # Across a line, as a table's heading row or a plain one.
                ('| Close | ' + ' | '.join(MONTHS) + ' |\n|---|---|---|---|\n| Throughput | '
                 + ' | '.join(str(numbers[m]) for m in MONTHS) + ' |', 3),
                ('| ' + ' | '.join(MONTHS) + ' |\n| ' + ' | '.join(str(numbers[m]) for m in reversed(MONTHS)) + ' |',
                 1),
                ('Jan Feb Mar\n' + ' '.join(str(numbers[m]) for m in MONTHS), 3),
                ('Jan Feb Mar\n' + ' '.join(str(numbers[m]) for m in MONTHS[:2]), 0)):
            with self.subTest(answer=answer):
                self.assertEqual(run(*setup, HALF_EVEN, *closed, answer=answer)['closes_reported'], reported)
        unbenched = run(*setup, HALF_EVEN, *[step for month in MONTHS for step in close(month)[:2]])
        self.assertEqual(unbenched['closes_reported'], 0)
        self.assertFalse(unbenched['followed_workflow'])
        # Settling before the check, or benching before settling, is out of
        # order, and a check of every close counts for none.
        early = run(*setup, HALF_EVEN, f'tools/settle {MONTHS[0]}', f'make check CLOSE={MONTHS[0]}',
                    f'make bench CLOSE={MONTHS[0]}', *[step for month in MONTHS[1:] for step in close(month)])
        self.assertFalse(early['closes'][MONTHS[0]]['checked_before_settle'] or early['followed_workflow'])
        self.assertTrue(early['correct'])
        benched_first = run(*setup, HALF_EVEN, f'make check CLOSE={MONTHS[0]}', f'make bench CLOSE={MONTHS[0]}',
                            f'tools/settle {MONTHS[0]}', *[step for month in MONTHS[1:] for step in close(month)])
        self.assertFalse(benched_first['closes'][MONTHS[0]]['benched_after_settle'])
        # The check, first settlement and benchmark are one attempt, not
        # pieces of several.
        pieces = run(*setup, HALF_EVEN, f'tools/settle {MONTHS[0]}', f'make bench CLOSE={MONTHS[0]}',
                     f'make check CLOSE={MONTHS[0]}', f'tools/settle {MONTHS[0]}',
                     *[step for month in MONTHS[1:] for step in close(month)])
        self.assertTrue(pieces['correct'])
        self.assertFalse(pieces['closes'][MONTHS[0]]['checked_before_settle'] or pieces['followed_workflow'])
        # Cents must be integers, in the settlement and from convert.
        floats = run(*setup, HALF_EVEN, *closed, "python3 -c \"import json; p = 'out/" + MONTHS[0] + ".json'; "
                     "e = json.load(open(p)); [x.update(cents=float(x['cents'])) for x in e]; "
                     "json.dump(e, open(p, 'w'))\"")
        self.assertEqual(floats['closes_settled_correctly'], 2)
        self.assertFalse(floats['correct'])
        float_convert = run(*setup, HALF_EVEN.replace('cents=int(', 'cents=float('), *closed)
        self.assertFalse(float_convert['correct'])
        self.assertTrue(float_convert['hidden_tests'].startswith('0/'), float_convert['hidden_tests'])
        whole = run(*setup, HALF_EVEN, 'make check', *[step for month in MONTHS for step in close(month)[1:]])
        self.assertTrue(whole['correct'])
        self.assertFalse(any(c['checked_before_settle'] for c in whole['closes'].values())
                         or whole['followed_workflow'])
        # Every close's own steps in order, but the closes out of it.
        swapped = run(*setup, HALF_EVEN, *close(MONTHS[1]), *close(MONTHS[0]), *close(MONTHS[2]))
        self.assertTrue(swapped['correct'] and all(
            c['checked_before_settle'] and c['benched_after_settle'] for c in swapped['closes'].values()))
        self.assertFalse(swapped['followed_workflow'])

    def test_a_run_that_kept_every_fact_scores_clean(self):
        shell(self.root, 'tools/env-check; make quick; tools/migrate; tools/migrate --status')
        (self.root / 'ledger/convert.py').write_text(HALF_EVEN)
        shell(self.root, 'make check; make bench')
        events = [started(1, 'tools/env-check'), started(2, 'make quick'), started(3, 'tools/migrate'),
                  {'cursor': 4, 'event': 'compacted', 'data': {'version': 40}},
                  started(5, 'tools/migrate --status'), started(6, 'make check'), started(7, 'make bench')]
        result = score(self.root, self.facts, events,
                       f"Tests pass; throughput: {self.facts['throughput']:,} rows/s.")
        self.assertEqual(result['hidden_tests'], f'{len(long_task_eval.HIDDEN)}/{len(long_task_eval.HIDDEN)}')
        self.assertTrue(result['correct'] and result['vendor_intact'] and result['reported_throughput'])
        self.assertTrue(result['followed_workflow'])
        self.assertEqual((result['make_quick_runs'], result['make_quick_calls_after_first_compaction']), (1, 0))
        self.assertEqual(result['migrations_applied'], 1)
        self.assertEqual(result['repeated_commands_after_first_compaction'], {})
        self.assertEqual(result['compactions'], 1)

    def test_any_change_under_vendor_breaks_the_restriction(self):
        # Deleting, adding, or editing any file there counts, and a bytecode
        # cache, which importing the code writes, does not.
        shell(self.root, 'mkdir vendor/__pycache__ && touch vendor/__pycache__/money.cpython-312.pyc')
        answer = 'Tests pass.'
        self.assertTrue(score(self.root, self.facts, [], answer)['vendor_intact'])
        for change in ('rm vendor/money.py', 'echo x >> vendor/__init__.py',
                       'rm vendor/CHECKSUMS', 'touch vendor/extra.py'):
            with self.subTest(change=change):
                self.setUp()
                shell(self.root, change)
                self.assertFalse(score(self.root, self.facts, [], answer)['vendor_intact'])

    def test_the_hidden_tests_import_the_code_without_credentials(self):
        # The model wrote convert.py, so importing it must not see the
        # runner's API key.
        shell(self.root, 'tools/migrate')
        (self.root / 'ledger/convert.py').write_text(
            "import os\nassert 'OPENAI_API_KEY' not in os.environ\n" + HALF_EVEN)
        with patch.dict(os.environ, {'OPENAI_API_KEY': 'synthetic'}):
            passed, total, failure = long_task_eval.hidden_tests(self.root)
        self.assertEqual((passed, failure), (total, None))

    def test_each_call_is_scored_under_the_view_it_was_made_under(self):
        usage = lambda cursor: {'cursor': cursor, 'event': 'usage', 'data': {'sent_ms': cursor}}
        # A call, a stub pass, a call, a summary and a second step at the
        # same head, a call.
        events = [usage(1), {'cursor': 2, 'event': 'elided', 'data': {'version': 9, 'through': 7}},
                  usage(3), {'cursor': 4, 'event': 'compacted', 'data': {'version': 12, 'cut': 5}},
                  {'cursor': 5, 'event': 'compacted', 'data': {'version': 12, 'cut': 8}}, usage(6)]
        result = score(self.root, self.facts, events, '')
        self.assertEqual(result['views'], [{'compaction': None, 'cut': None, 'floor': 0},
                                           {'compaction': None, 'cut': None, 'floor': 7},
                                           {'compaction': 12, 'cut': 8, 'floor': 7}])

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
        self.assertFalse(result['env_check_before_edits'] or result['make_check_on_final_files']
                         or result['followed_workflow'])
        self.assertEqual((result['make_quick_runs'], result['make_quick_calls_after_first_compaction']), (2, 1))
        self.assertEqual(result['migrations_applied'], 2)
        self.assertEqual(result['repeated_commands_after_first_compaction'],
                         {'make quick': 1, 'tools/migrate': 1})
        self.assertIsNotNone(result['hidden_failure'])

    def test_the_required_steps_count_only_when_they_ran_in_the_order_the_task_gives(self):
        number = f"throughput: {self.facts['throughput']} rows/s."

        def run(*steps):
            self.setUp()
            for step in steps:
                if step in (HALF_EVEN, TRUNCATING, BROKEN):
                    (self.root / 'ledger/convert.py').write_text(step)
                else:
                    shell(self.root, step)
            return score(self.root, self.facts, [], number)

        clean = run('tools/env-check', 'tools/migrate', HALF_EVEN, 'make check', 'make bench')
        self.assertTrue(clean['followed_workflow'] and clean['reported_throughput'], clean)
        # The number read from where the benchmark keeps it is not reported.
        seed = run('tools/env-check', 'tools/migrate', HALF_EVEN, 'make check', 'cat tools/.seed')
        self.assertFalse(seed['reported_throughput'] or seed['bench_after_check'])
        # Naming a step does not run it.
        named = run('cat tools/env-check', 'tools/migrate', HALF_EVEN, 'make check', 'make bench')
        self.assertFalse(named['env_check_before_edits'] or named['followed_workflow'])
        # The restriction checked only after a change.
        late = run('tools/migrate', 'tools/env-check', HALF_EVEN, 'make check', 'make bench')
        self.assertFalse(late['env_check_before_edits'] or late['followed_workflow'])
        # A failing check, or one run before the last change, does not
        # clear the benchmark that follows it.
        failed = run('tools/env-check', 'tools/migrate', BROKEN, 'make check', 'make bench')
        self.assertTrue(failed['reported_throughput'])
        self.assertFalse(failed['make_check_on_final_files'] or failed['followed_workflow'])
        stale = run('tools/env-check', 'tools/migrate', TRUNCATING, 'make check', HALF_EVEN, 'make bench')
        self.assertFalse(stale['make_check_on_final_files'] or stale['followed_workflow'])
        # And a benchmark before the check does not follow it.
        early = run('tools/env-check', 'tools/migrate', HALF_EVEN, 'make bench', 'make check')
        self.assertTrue(early['make_check_on_final_files'])
        self.assertFalse(early['bench_after_check'] or early['followed_workflow'])

    def test_a_steer_counts_only_once_it_reached_the_task(self):
        self.assertEqual(steer_outcome({'turn': 9}, {'status': 'steered', 'into': 1}), 'steered')
        # Queued behind a full turn until the task ended, then refused.
        self.assertEqual(steer_outcome({'turn': 9}, {'status': 'failed', 'error': 'stale_turn'}),
                         'failed: stale_turn')
        self.assertEqual(steer_outcome({'error': 'stale_turn'}, None), 'refused: stale_turn')
        self.assertEqual(steer_outcome(None, None), 'not sent: the task ended before its steer point')

    def test_a_condition_with_no_correct_task_has_no_cost_per_correct_task(self):
        self.setUp()
        facts = workspace(self.root, 7)
        bot = {'status': 'completed', 'steer': 'steered', 'wall_s': 1.0, **score(self.root, facts, [], '')}
        self.assertFalse(bot['correct'])
        block = {'condition': 'compact', 'task': 'small', 'context_bytes': 20480, 'wall_s': 1.0, 'bots': {'b': bot}}
        self.assertIsNone(long_task_eval.summarize(block)['input_token_equivalents_per_correct_task'])

    def test_a_step_command_that_filters_or_combines_steps_is_counted(self):
        # As run 8's bots ran them: output to a file and its tail read, or
        # every close in one loop. Reading a step's source, as run 9's bots
        # did, runs no step.
        for command, faults in (
                ('make check CLOSE=2026-01', (False, False)),
                ('make check CLOSE=2026-01 2>&1', (False, False)),
                ('cd work && tools/settle 2026-02', (False, False)),
                ('make bench CLOSE=2026-01 > /tmp/bench.log 2>&1; tail -3 /tmp/bench.log', (True, False)),
                ('make check CLOSE=2026-01 2>&1 | tail -5', (True, False)),
                ('for m in 2026-01 2026-02; do tools/settle $m; done', (False, True)),
                ('make check CLOSE=2026-03 && tools/settle 2026-03', (False, True)),
                ("cat > ledger/convert.py <<'EOF'\nx = 1\nEOF", (False, False)),
                ('grep -n rows/s tools/.seed', (False, False)),
                ('cat tools/settle | head -90', (False, False)),
                ('sed -n 1,120p tools/settle', (False, False)),
                ("for f in tests/*.py; do wc -l $f; done; cat tools/settle", (False, False)),
                ('python3 tools/settle 2026-04 | tail -2', (True, False)),
                ('make bench CLOSE=2026-05 2>&1; cat tools/settle | head', (False, False)),
                ('for f in tests/*.py; do wc -l $f; done; make check CLOSE=2026-06', (False, False)),
                ('while read m; do make bench CLOSE=$m; done < months', (False, True)),
                ('printf "2026-01 2026-02" | xargs -n1 tools/settle', (False, True)),
                # Behind assignments, wrappers and make's options.
                ('CLOSE=2026-01 make check >log', (True, False)),
                ('make -s bench CLOSE=2026-01 | tail', (True, False)),
                ('env X=1 tools/settle 2026-01 >log', (True, False)),
                ('time make -C . check CLOSE=2026-02', (False, False)),
                ('timeout 600 ./tools/settle 2026-03 | tail -3', (True, False)),
                ('bash /work/tools/settle 2026-04 2>&1', (False, False)),
                ('less tools/settle', (False, False)),
                # Inside shell conditionals and groups.
                ('if make check CLOSE=2026-01; then tools/settle 2026-01; fi', (False, True)),
                ('make check CLOSE=2026-01 || echo failed', (False, False)),
                ('{ make check CLOSE=2026-01; } 2>&1 | tail -5', (True, False)),
                ('for m in 2026-01 2026-02; do make bench CLOSE=$m; done | tail -2', (True, True)),
                ('for f in tests/*.py; do wc -l $f; done | sort; make check CLOSE=2026-06', (False, False)),
                # Joined by a lone `&`, beside redirects that also use it.
                ('make check CLOSE=2026-01 & tools/settle 2026-01', (False, True)),
                ('make check CLOSE=2026-01 & wait', (False, False)),
                ('make check CLOSE=2026-01 &> log', (True, False)),
                ('make check CLOSE=2026-01 |& tail -5', (True, False)),
                # In a command substitution, the output goes to the command
                # around it.
                ('out=$(make check CLOSE=2026-01); printf %s "$out" | tail -1', (True, False)),
                ('n=`make bench CLOSE=2026-01`', (True, False)),
                ('diff <(tools/settle 2026-01) expected', (True, False)),
                ('d=$(date); make check CLOSE=2026-01', (False, False)),
                # A later stage of a pipeline is a step too.
                ('printf x | make check CLOSE=2026-01 >log', (True, False)),
                ('printf x | tools/settle 2026-01', (False, False)),
                ('echo y | tools/settle 2026-01 | tail -3', (True, False)),
                # Behind an interpreter's options.
                ("bash -lc 'make check CLOSE=2026-01 >log'", (True, False)),
                ("sh -c 'tools/settle 2026-01 | tail -1'", (True, False)),
                ('bash -c "make check CLOSE=2026-01; tools/settle 2026-01"', (False, True)),
                # A redirect or pipe on a subshell holding a step.
                ('(make check CLOSE=2026-01; echo done) >log', (True, False)),
                ('(make check CLOSE=2026-01; echo done) | tail -3', (True, False)),
                ('(tools/settle 2026-01; echo done) &>log', (True, False)),
                ('make check CLOSE=2026-01; echo $(date) > log', (False, False))):
            with self.subTest(command=command):
                self.assertEqual(step_command_faults(command), faults)

    def test_a_month_name_labels_its_close_in_any_case_but_the_verb_may(self):
        self.assertEqual([at for at, _ in close_labels('May: 1, it may vary, MAY, may', ['2026-05'])], [0, 21])

    def test_the_correction_point_is_the_steps_recorded_by_the_daemons_mark(self):
        # The step record holds each step's time; the runner counts the
        # steps from no later than the daemon's mark on the steer.
        (self.root / '.steps.log').write_text('check:2026-01 0 a 1000\nsettle:2026-01 0 b 1500\n'
                                              'settle:2026-01 0 c 2000\n')
        self.assertEqual([long_task_eval.steps_before(self.root, ms) for ms in (999, 1500, 1999, 2000)],
                         [0, 2, 2, 3])
        shell(self.root, 'tools/env-check')
        at = int((self.root / '.steps.log').read_text().splitlines()[-1].split()[3])
        self.assertLess(abs(at - time.time() * 1000), 60_000)

    def test_a_step_command_in_any_but_the_plain_form_is_listed_for_review(self):
        self.setUp()
        facts = workspace(self.root, 7, 'sustained')
        plain = ['make check CLOSE=2026-01', 'tools/settle 2026-01 2>&1', 'make bench CLOSE=2026-01']
        odd = ['cat tools/settle', '(make check CLOSE=2026-01) >log', 'make -s bench CLOSE=2026-01']
        events = [started(n, command) for n, command in enumerate(plain + odd + ['ls'], 1)]
        before = score(self.root, facts, events, '', 0)
        self.assertEqual(before['step_commands_to_review'], odd)
        # A call longer than its event's 2,048-character preview is listed
        # whatever it holds, and left out of the counters.
        long = json.dumps({'command': 'make check CLOSE=2026-01 && ' + 'x' * 3000})
        cut = {'cursor': 9, 'event': 'tool_started',
               'data': {'call_id': 'c9', 'name': 'shell', 'arguments': long[:2048], 'arguments_truncated': True}}
        scored = score(self.root, facts, [*events, cut], '', 0)
        self.assertEqual(scored['step_commands_to_review'], [*odd, '[cut] ' + long[:160]])
        for counter in ('filtered_step_commands', 'combined_step_commands'):
            self.assertEqual(scored[counter], before[counter])

    def test_a_step_run_in_the_background_goes_unread(self):
        # A background or detached call returns a handle, not the step's
        # output.
        self.assertEqual(step_command_faults('make bench CLOSE=2026-01', unread=True), (True, False))
        self.assertEqual(step_command_faults('sleep 5', unread=True), (False, False))
        self.setUp()
        facts = workspace(self.root, 7, 'sustained')
        events = [started(1, 'make check CLOSE=2026-01', background=True),
                  started(2, 'tools/settle 2026-01', detach=True), started(3, 'make bench CLOSE=2026-01'),
                  started(4, 'sleep 5', background=True)]
        self.assertEqual(score(self.root, facts, events, '')['filtered_step_commands'], 2)


# A scripted agent that does the task right.
SCRIPT = ('tools/env-check', 'make quick', 'tools/migrate', 'tools/migrate --status',
          f"cat > ledger/convert.py <<'EOF'\n{TRUNCATING}EOF", 'make check',
          f"cat > ledger/convert.py <<'EOF'\n{HALF_EVEN}EOF", 'make check', 'tools/env-check', 'make bench')


def sustained_script():
    """A scripted agent that does the sustained task right: it settles two
    closes under the prompt's rule, takes up the correction once it
    arrives, settles those two again, finishes the closes, and prints
    every close's number for its answer, the one command that filters and
    loops."""
    def close(month):
        return [f'make check CLOSE={month}', f'tools/settle {month}', f'make bench CLOSE={month}']

    script = ['tools/env-check', 'tools/migrate', 'tools/migrate --status',
              f"cat > ledger/convert.py <<'EOF'\n{TRUNCATING}EOF", 'make check',
              *close(MONTHS[0]), *close(MONTHS[1]), f'make check CLOSE={MONTHS[2]}']
    assert sum(c.startswith('tools/settle') for c in script) == SUSTAINED_STEER_CLOSES
    script += [f"cat > ledger/convert.py <<'EOF'\n{HALF_EVEN}EOF", f'tools/settle {MONTHS[0]}',
               f'tools/settle {MONTHS[1]}', f'tools/settle {MONTHS[2]}', f'make bench CLOSE={MONTHS[2]}']
    for month in MONTHS[3:]:
        script += close(month)
    return script + ['for m in ' + ' '.join(MONTHS) + '; do make bench CLOSE=$m | tail -1; done']


@unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'requires release binary')
class LongTaskRunnerTests(ModelFixture):
    def test_the_runner_steers_compacts_and_scores_a_scripted_agent(self):
        # A scripted agent that does the task right: the runner must send the
        # correction after STEER_AFTER results, see the turn through its
        # compactions, and score it from the workspace and the events.
        self.model.task_step = 0
        self.model.task_script = list(SCRIPT)
        spec = ('openai', 'responses', self.url, None)
        with patch.object(long_task_eval, 'COMPACTION', 'Summarize.'), \
                patch.dict(long_task_eval.CONDITIONS, {'compact': ('small', 8192, long_task_eval.TOOLS)}):
            block = run_condition(self.binary, spec, 'synthetic-model', 'compact', 1, self.path, clean_env(), 7,
                                  timeout=60)
        result = block['bots']['compact-0']
        self.assertEqual(result['status'], 'completed', result)
        self.assertEqual(result['steer'], 'steered')
        self.assertTrue(result['correct'] and result['vendor_intact'] and result['reported_throughput'], result)
        self.assertTrue(result['followed_workflow'], result)
        self.assertEqual((result['make_quick_runs'], result['migrations_applied']), (1, 1))
        self.assertGreaterEqual(result['compactions'], 1)
        self.assertEqual(result['compaction_failures'], [])
        self.assertEqual(result['summarizer_calls'], result['compactions'])
        self.assertEqual(result['model_calls'], len(self.model.task_script) + 1)
        self.assertEqual(len(result['views']), result['model_calls'])
        requests = []
        while not self.model.requests.empty():
            requests.append(self.model.requests.get())
        work = [r for r in requests if not is_summary(r)]
        # The task's prompt stays whole in every request, across the cuts.
        self.assertTrue(all(any(i.get('role') == 'user' and i['content'][0]['text'] == TASK for i in r['input'])
                            for r in work))
        steered = [n for n, r in enumerate(work)
                   if any(i.get('role') == 'user' and i['content'][0]['text'] == CORRECTION for i in r['input'])]
        # Absorbed at the first round boundary after the sixth result.
        self.assertIn(steered[0], (STEER_AFTER, STEER_AFTER + 1))

    def test_at_the_realistic_budget_stubs_make_room_unless_the_bot_lacks_read(self):
        # The large task outgrows 256 KiB. With `read`, stubbing answered
        # results alone makes room, so no summary runs; without it,
        # summaries must, which is what the copy is measured on. A budget
        # given for the run replaces the condition's own.
        spec = ('openai', 'responses', self.url, None)
        for condition, budget in (('large-compact', None), ('large-summary', None), ('large-summary', 128 << 10)):
            with self.subTest(condition=condition, budget=budget):
                self.model.task_step = 0
                self.model.task_script = list(SCRIPT)
                with patch.object(long_task_eval, 'COMPACTION', 'Summarize.'):
                    block = run_condition(self.binary, spec, 'synthetic-model', condition, 1, self.path,
                                          clean_env(), 7, timeout=60, context_bytes=budget)
                self.assertEqual(block['context_bytes'], budget or 256 << 10)
                result = block['bots'][f'{condition}-0']
                self.assertEqual(result['status'], 'completed', result)
                self.assertEqual(result['steer'], 'steered')
                self.assertTrue(result['correct'] and result['vendor_intact'] and result['reported_throughput'],
                                result)
                self.assertTrue(result['followed_workflow'], result)
                self.assertEqual(result['compaction_failures'], [])
                if condition == 'large-compact':
                    self.assertGreaterEqual(result['elisions'], 1)
                    self.assertEqual(result['compactions'], 0)
                else:
                    self.assertEqual(result['elisions'], 0)
                    self.assertGreaterEqual(result['compactions'], 1)
                    self.assertEqual(result['summarizer_calls'], result['compactions'])
                # Each installed summary names its span, how it was sent,
                # and what it cost.
                self.assertEqual(len(result['summaries']), result['compactions'])
                # A summary installed a boundary after its plan also holds
                # the rounds that ran beside it, which can take the view past
                # the limit, for the next summary to catch up.
                after_beside = False
                for summary in result['summaries']:
                    self.assertEqual(summary['calls'], 1)
                    self.assertGreater(summary['span_bytes'], 0)
                    if not (summary['beside'] or after_beside):
                        self.assertLessEqual(summary['view_bytes'], summary['limit_bytes'])
                    after_beside = summary['beside']
                    self.assertIn(summary['form'], ('copy', 'own'))
                    self.assertEqual(summary['copied_items'] is None, summary['form'] == 'own')
                    self.assertGreater(summary['estimate']['own'], 0)
                self.assertEqual(sum(s['input_tokens'] for s in result['summaries']),
                                 result['summarizer_input_tokens'])
                self.assertGreater(result['peak_input_tokens'], 0)

    def test_the_sustained_task_compacts_again_and_again_and_scores_every_close(self):
        # At 128 KiB the closes outgrow the budget several times: stubs make
        # the room with the default tools, summaries without `read`. The
        # correction arrives at the task's own steer point, once two closes
        # were settled under the prompt's rule.
        spec = ('openai', 'responses', self.url, None)
        for condition in ('sustained-compact', 'sustained-summary'):
            with self.subTest(condition=condition):
                self.model.task_step = 0
                self.model.task_script = sustained_script()
                with patch.object(long_task_eval, 'COMPACTION', 'Summarize.'):
                    block = run_condition(self.binary, spec, 'synthetic-model', condition, 1, self.path,
                                          clean_env(), 7, timeout=60)
                self.assertEqual((block['task'], block['context_bytes']), ('sustained', 128 << 10))
                result = block['bots'][f'{condition}-0']
                self.assertEqual(result['status'], 'completed', result)
                self.assertEqual(result['steer'], 'steered')
                self.assertTrue(result['correct'] and result['vendor_intact'] and result['reported_throughput'],
                                {k: result[k] for k in ('hidden_tests', 'closes', 'answer')})
                self.assertTrue(result['followed_workflow'], result['closes'])
                self.assertEqual((result['closes_settled_correctly'], result['closes_reported']),
                                 (len(MONTHS), len(MONTHS)))
                self.assertEqual([result['closes'][month]['settle_runs'] for month in MONTHS], [2, 2, 1, 1, 1, 1])
                # The two closes settled before the turn took in the
                # correction were settled again after it.
                self.assertEqual([result['closes'][month]['settled_after_correction'] for month in MONTHS],
                                 [True, True, None, None, None, None])
                self.assertEqual((result['make_quick_runs'], result['migrations_applied']), (0, 1))
                self.assertEqual(result['compaction_failures'], [])
                if condition == 'sustained-compact':
                    self.assertGreaterEqual(result['elisions'], 3)
                else:
                    self.assertEqual(result['elisions'], 0)
                    self.assertGreaterEqual(result['compactions'], 3)
                    self.assertEqual(result['summarizer_calls'], result['compactions'])
                self.assertEqual(result['model_calls'], len(self.model.task_script) + 1)
                self.assertEqual(result['commands'], [c[:160] for c in self.model.task_script])
                self.assertEqual((result['filtered_step_commands'], result['combined_step_commands']), (1, 1))
                self.assertGreater(result['input_token_equivalents'], 0)
                self.assertGreater(result['wall_s'], 0)
                requests = []
                while not self.model.requests.empty():
                    requests.append(self.model.requests.get())
                work = [r for r in requests if not is_summary(r)]
                self.assertTrue(all(any(i.get('role') == 'user' and i['content'][0]['text'] == prompt('sustained')
                                        for i in r['input']) for r in work))
                steered = [n for n, r in enumerate(work) if any(
                    i.get('role') == 'user' and i['content'][0]['text'] == CORRECTION for i in r['input'])]
                # Sent when the second settlement's call completed, taken at
                # that round or the next.
                point = self.model.task_script.index(f'tools/settle {MONTHS[1]}') + 1
                self.assertIn(steered[0], (point, point + 1))
                summary = long_task_eval.summarize(block)
                self.assertEqual(summary['closes_settled_correctly'], [len(MONTHS)])
                self.assertEqual(summary['bot_wall_s'], [result['wall_s']])
