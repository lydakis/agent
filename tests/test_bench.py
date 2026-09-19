import unittest
import os
from copy import deepcopy
from unittest.mock import patch

import psutil

from bench.events import Events
from bench.processes import Process, Tree
from bench.report import compare
from bench.profiles import profile
from bench.targets import clean_env
from tests.test_runtime import ModelFixture


class ProcessTests(unittest.TestCase):
    def test_detailed_memory_is_opt_in_and_never_substitutes_missing_values(self):
        rows = [Process(10, 1, 10, 100, .2, 'a'), Process(11, 10, 10, 200, .1, 'b')]
        with patch('bench.processes.detailed_memory', side_effect=[(60, 30), (80, 40)]) as measure:
            self.assertIsNone(Tree(10).sample(rows)['pss_bytes'])
            measure.assert_not_called()
            result = Tree(10).sample(rows, memory_detail=True)
            self.assertEqual(result['pss_bytes'], 140)
            self.assertEqual(result['private_bytes'], 70)
        with patch('bench.processes.detailed_memory', side_effect=[(None, 30), (None, 40)]):
            result = Tree(10).sample(rows, memory_detail=True)
            self.assertIsNone(result['pss_bytes'])
            self.assertEqual(result['private_bytes'], 70)
        for failure in (psutil.AccessDenied(11), psutil.NoSuchProcess(11)):
            with patch('bench.processes.detailed_memory', side_effect=[(60, 30), failure]):
                result = Tree(10).sample(rows, memory_detail=True)
                self.assertIsNone(result['pss_bytes'])
                self.assertIsNone(result['private_bytes'])
                self.assertEqual(result['rss_bytes'], 300)

    def test_denied_counters_are_unavailable_instead_of_zero(self):
        row = Process(10, 1, 10, None, None, "a")
        with patch('bench.processes.counters', side_effect=psutil.AccessDenied(10)):
            result = Tree(10).sample([row])
        self.assertIsNone(result['rss_bytes'])
        self.assertIsNone(result['root_rss_bytes'])
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
        self.assertEqual(result["root_rss_bytes"], 100)
        self.assertEqual(result['threads'], 6)
        self.assertAlmostEqual(result["observed_cpu_seconds"], 0.7)
        # Keep observed descendants after reparenting, but do not follow PID reuse.
        result = tree.sample([Process(11, 1, 11, 250, 0.6, "b"),
                              Process(10, 1, 99, 1000, 9, "new")])
        self.assertEqual(result["rss_bytes"], 250)
        self.assertEqual(result["root_rss_bytes"], 0)
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
        changed['target_metadata']['comparison_profile']['contract']['durability'] = 'ephemeral'
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




class DaemonRunnerTests(unittest.TestCase):
    @unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1',
                         'set AGENT_TEST_RUNTIME=1 after a Rust release build')
    def test_large_daemon_deltas_are_normalized_after_framing(self):
        import json
        from pathlib import Path
        from tempfile import TemporaryDirectory
        from types import SimpleNamespace
        from bench.runner import run_once
        binary = Path('.local/target/release/agent').resolve()
        options = SimpleNamespace(protocol='responses', driver='daemon', timeout=15,
                                  interval=.05, discovery_interval=.2,
                                  rss_limit_mib=512, process_limit=16)
        # One reaches the old complete-line bound; the other also exceeds it
        # while the newline is still in a later pipe read.
        for chunk_bytes in (65536, 131072):
            with self.subTest(chunk_bytes=chunk_bytes), TemporaryDirectory() as tmp:
                directory = Path(tmp)
                config = dict(version=1, concurrency=1, turns=2, chunks=3,
                              chunk_bytes=chunk_bytes, chunk_delay_ms=100, history_bytes=32)
                (directory / 'workload.json').write_text(json.dumps(config))
                result = run_once([str(binary), 'serve', '--tools', 'echo'],
                                  config, options, directory, 0)
                self.assertEqual(result['status'], 'ok', result)
                self.assertEqual(result['events']['stream_payload_bytes'], 6 * chunk_bytes)


@unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1',
                     'set AGENT_TEST_RUNTIME=1 after a Rust release build')
class LiveFleetTests(ModelFixture):
    def test_selected_model_reaches_every_bot_without_environment_dependence(self):
        import subprocess
        from bench import live_fleet

        original_run = subprocess.run

        def local_run(command, **kwargs):
            # Redirect only provider transport. All CLI parsing, submission,
            # model selection, tool execution and reporting remain real.
            command = list(command)
            if '--provider' in command:
                command[command.index('--provider') + 1] = f'openai=responses,{self.url}'
            return original_run(command, **kwargs)

        for label, defaults in (('unset', {}), ('conflicting', {'AGENT_MODEL': 'unregistered/wrong-model'})):
            with self.subTest(environment=label):
                out = self.path / label
                with patch.dict(os.environ, dict(clean_env(), **defaults), clear=True), \
                     patch.object(live_fleet.subprocess, 'run', side_effect=local_run), \
                     patch.object(live_fleet, 'PROMPT', 'shell:wc -l notes.txt'):
                    try:
                        result = live_fleet.run(self.binary, out, 2, 'openai/synthetic-model', None, 'low')
                    finally:
                        original_run([str(self.binary), 'shutdown', '--store', str(out / 'state.sqlite')],
                                     env=clean_env(), capture_output=True, timeout=5)
                self.assertEqual(result['submitted'], 2)
                self.assertEqual(result['submit_failures'], [])
                self.assertEqual(result['statuses'], {'completed': 2})
                self.assertEqual(result['errors'], {})
                self.assertEqual(result['model'], 'openai/synthetic-model')
                # One warmup plus a shell call and final answer for each bot.
                requests = [self.model.requests.get(timeout=3) for _ in range(5)]
                self.assertEqual({r['model'] for r in requests}, {'synthetic-model'})
                self.assertTrue(self.model.requests.empty())


if __name__ == "__main__":
    unittest.main()
