import json
import tempfile
import unittest
from pathlib import Path

from bench.perf_gate import compare


def footprint(cpu, jobs=3, next_ready=1):
    return {'idle_rss_mib': 18.0, 'per_idle_bot_kib': 1.0, 'turns_rss_mib': 27.0, 'cpu_ms_per_turn': cpu,
            'operations': {'begin': {'count': jobs}, 'commit': {'count': 9}, 'next_ready': {'count': next_ready}}}


class PerfGateTests(unittest.TestCase):
    def screen(self, base, head):
        root = Path(tempfile.mkdtemp())
        for side, runs in (('base', base), ('head', head)):
            folder = root / f'footprint-{side}-0'
            folder.mkdir()
            rows = [{'warmup': True, **footprint(99)}] + [{'warmup': False, **run} for run in runs]
            (folder / 'result.json').write_text(json.dumps({'runs': rows}))
        return compare(root)

    def test_more_store_work_fails_and_timing_only_warns_when_runs_do_not_overlap(self):
        # Timed counts and overlapping runs say nothing.
        _, failed, warned = self.screen([footprint(1.0), footprint(1.5)],
                                        [footprint(1.4, next_ready=7), footprint(1.1)])
        self.assertEqual((failed, warned), ([], []))
        # Every head run slower than every base run, by more than 15%.
        _, failed, warned = self.screen([footprint(1.0), footprint(1.1)], [footprint(1.4), footprint(1.5)])
        self.assertEqual(failed, [])
        self.assertEqual(len(warned), 1)
        self.assertIn('CPU per turn', warned[0])
        # One store job more a turn fails, whatever the timings.
        _, failed, _ = self.screen([footprint(1.0)], [footprint(1.0, jobs=4)])
        self.assertEqual(failed, ['footprint: begin rose from 3 to 4'])

    def test_a_turn_no_note_told_fails_where_the_base_told_every_one(self):
        def sonny(untold, p95):
            return {'lookout': {'10x100': {'untold': untold, 'note_delay_ms_p95': p95,
                                           'cpu_ms_per_1k_deltas': 10.0}}}
        root = Path(tempfile.mkdtemp())
        for side, run in (('base', sonny(0, 3000)), ('head', sonny(4, 1000))):
            folder = root / f'sonny-{side}-0'
            folder.mkdir()
            (folder / 'result.json').write_text(json.dumps({'runs': [run]}))
        _, failed, _ = compare(root)
        # However much faster the head's notes are.
        self.assertEqual(failed, ['sonny: lookout at 10x100 left 4 ended turns untold, '
                                  'where the base told every one'])


if __name__ == '__main__':
    unittest.main()
