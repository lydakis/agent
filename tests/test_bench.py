import unittest
from copy import deepcopy
from unittest.mock import patch

import psutil

from bench.events import Events
from bench.processes import Process, Tree
from bench.report import compare
from bench.profiles import profile


class ProcessTests(unittest.TestCase):
    def test_denied_counters_are_unavailable_instead_of_zero(self):
        row = Process(10, 1, 10, None, None, "a")
        with patch('bench.processes.counters', side_effect=psutil.AccessDenied(10)):
            result = Tree(10).sample([row])
        self.assertIsNone(result['rss_bytes'])
        self.assertIsNone(result['threads'])
        self.assertEqual(result['unreadable_processes'], 1)

    def test_kernel_pid_never_becomes_a_tree_root_via_getpgid_zero(self):
        # getpgid(0) means the caller's group, not the kernel process's group.
        rows = [Process(0, 0, 10, 0, 0, "kernel"),
                Process(1, 0, 1, 100, 10, "init"),
                Process(10, 1, 10, 200, 1, "target"),
                Process(20, 1, 20, 500, 5, "unrelated")]
        self.assertEqual(Tree(10).sample(rows)["processes"], 1)

    def test_process_tree_includes_reparented_group_and_descendants_only(self):
        tree = Tree(10)
        rows = [Process(10, 1, 10, 100, 0.2, "a", 2),
                Process(11, 10, 11, 200, 0.4, "b", 3),
                Process(12, 1, 10, 300, 0.1, "c", 1),
                Process(99, 1, 99, 900, 9, "d", 9)]
        result = tree.sample(rows)
        self.assertEqual(result["processes"], 3)
        self.assertEqual(result["rss_bytes"], 600)
        self.assertEqual(result['threads'], 6)
        self.assertAlmostEqual(result["observed_cpu_seconds"], 0.7)
        # Keep observed descendants after reparenting, but do not follow PID reuse.
        result = tree.sample([Process(11, 1, 11, 250, 0.6, "b"),
                              Process(10, 1, 99, 1000, 9, "new")])
        self.assertEqual(result["rss_bytes"], 250)
        self.assertIsNone(result['threads'])
        self.assertAlmostEqual(result["observed_cpu_seconds"], 0.9)

    def test_reused_child_pid_is_counted_as_new_process_when_still_owned(self):
        tree = Tree(10)
        root = Process(10, 1, 10, 100, 0.2, "a")
        tree.sample([root, Process(11, 10, 10, 200, 0.4, "b")])
        result = tree.sample([root, Process(11, 10, 10, 200, 0.1, "c")])
        self.assertAlmostEqual(result["observed_cpu_seconds"], 0.7)


class EventTests(unittest.TestCase):
    def test_only_static_diagnostics_are_retained_and_cannot_be_success(self):
        self.events.add({'event': 'diagnostic', 'stage': 'benchmark',
                         'code': 'provider_connection_os_49', 'ignored': 'DO_NOT_RETAIN'}, .4)
        self.assertEqual(self.events.diagnostic, {'stage': 'benchmark', 'code': 'provider_connection_os_49'})
        self.assertNotIn('DO_NOT_RETAIN', str(self.events.diagnostic))
        with self.assertRaises(ValueError):
            self.events.finish()
        with self.assertRaises(ValueError):
            Events({}).add({'event': 'diagnostic', 'stage': 'benchmark', 'code': 'secret message'}, 0)

    def setUp(self):
        self.events = Events({"concurrency": 1, "turns": 1, "chunks": 2,
                              "chunk_bytes": 4})
        self.events.add({"event": "ready"}, 0.1)
        self.events.add({"event": "turn_start", "agent": "bob", "turn": "0"}, 0.2)

    def test_complete_stream_and_observer_latencies(self):
        for seq, when in [(0, 0.3), (1, 0.4)]:
            self.events.add({"event": "chunk", "agent": "bob", "turn": "0",
                             "seq": seq, "bytes": 4}, when)
        self.events.add({"event": "turn_end", "agent": "bob", "turn": "0"}, 0.5)
        result = self.events.finish()
        self.assertEqual(result["completed_turns"], 1)
        self.assertAlmostEqual(result["observed_first_chunk_ms"]["p50"], 100)
        self.assertAlmostEqual(result["observed_turn_ms"]["p99"], 300)

    def test_incomplete_or_out_of_order_stream_is_not_a_success(self):
        with self.assertRaises(ValueError):
            self.events.add({"event": "chunk", "agent": "bob", "turn": "0",
                             "seq": 1, "bytes": 4}, 0.3)
        with self.assertRaises(ValueError):
            self.events.finish()

    def test_parallel_turns_on_one_agent_are_rejected(self):
        with self.assertRaises(ValueError):
            self.events.add({"event": "turn_start", "agent": "bob", "turn": "1"}, 0.3)


class ReportTests(unittest.TestCase):
    def result(self, engine='rust'):
        return {'schema': 1, 'compatibility': {'workload': 'a'},
                'target_metadata': {'engine': engine, 'comparison_profile': profile(engine)},
                'workload': {'concurrency': 1}, 'runs': [{
                    'status': 'ok', 'wall_seconds': 1,
                    'target': {'rss_bytes': 100, 'observed_cpu_seconds': .1},
                    'events': {'observed_turn_ms': {'p99': 1}, 'observed_first_chunk_ms': {'p99': 1}},
                    'provider': {'peak_active_requests': 1, 'request_body_bytes': 10,
                                 'response_body_bytes': 10, 'connections_used': 1}}]}

    def test_only_matched_regressions_get_percentage_claims(self):
        base = self.result()
        self.assertEqual(compare(base, deepcopy(base))['classification'], 'matched_configuration_regression')
        with self.assertRaisesRegex(ValueError, 'exploratory'):
            compare(base, self.result('pi'))
        report = compare(base, self.result('pi'), exploratory=True)
        self.assertEqual(report['classification'], 'exploratory_unmatched_footprints')
        self.assertNotIn('change_percent', report['metrics']['peak_target_rss_bytes'])
        self.assertTrue(report['feature_gaps'])

    def test_durability_mismatch_missing_profile_and_underload_cannot_be_ranked(self):
        base = self.result()
        changed = deepcopy(base)
        changed['target_metadata']['comparison_profile']['contract']['durability'] = 'sqlite_full'
        for candidate in (changed, {**base, 'target_metadata': {}}):
            with self.assertRaises(ValueError):
                compare(base, candidate)
            report = compare(base, candidate, exploratory=True)
            self.assertNotIn('change_percent', report['metrics']['peak_target_rss_bytes'])
        changed = deepcopy(base)
        changed['runs'][0]['provider']['peak_active_requests'] = 0
        with self.assertRaisesRegex(ValueError, 'concurrency'):
            compare(base, changed)

    def test_incompatible_or_failed_results_are_not_compared(self):
        base = {"schema": 1, "compatibility": {"workload": "a"},
                "runs": [{"status": "ok", "wall_seconds": 1}]}
        changed = {**base, "compatibility": {"workload": "b"}}
        with self.assertRaises(ValueError):
            compare(base, changed)
        changed = {**base, "runs": [{"status": "timeout"}]}
        with self.assertRaises(ValueError):
            compare(base, changed)

    def test_only_explicit_model_protocol_difference_can_be_exploratory(self):
        base, fx = self.result(), self.result('fx')
        base['compatibility']['provider_protocol'] = 'responses'
        fx['compatibility']['provider_protocol'] = 'gateway'
        with self.assertRaises(ValueError):
            compare(base, fx)
        report = compare(base, fx, exploratory=True)
        self.assertTrue(any('provider_protocol' in gap for gap in report['feature_gaps']))
        self.assertNotIn('change_percent', report['metrics']['peak_target_rss_bytes'])
        for key, value in (('provider_protocol', 'binary'), ('host_id', 'other'),
                           ('observer_sha256', 'other'), ('workload', 'other')):
            changed = deepcopy(fx)
            changed['compatibility'][key] = value
            with self.assertRaises(ValueError):
                compare(base, changed, exploratory=True)


if __name__ == "__main__":
    unittest.main()
