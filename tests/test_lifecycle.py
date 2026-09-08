import os
from pathlib import Path
import tempfile
import unittest
from bench.lifecycle import run_once


@unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'requires Rust release and local process counters')
class LifecycleTests(unittest.TestCase):
    def test_measurement_checks_tools_recovery_and_forks_with_separate_provider(self):
        root = Path(__file__).resolve().parent.parent
        with tempfile.TemporaryDirectory(dir=root/'.local') as path:
            config = dict(version=1, concurrency=2, turns=2, history_bytes=4096,
                          chunks=4, chunk_bytes=256, chunk_delay_ms=25)
            result = run_once(root/'.local/target/release/agent', Path(path), config, 'shell', 'echo,shell')
        self.assertEqual(result['status'], 'ok', result)
        self.assertEqual(result['completed_turns'], 4)
        self.assertEqual(result['provider']['completed_requests'], 8)
        self.assertEqual(result['provider']['tool_results'], 4)
        self.assertEqual(result['provider']['peak_active_requests'], 2)
        self.assertGreater(result['target_peak_processes'], 1)
        self.assertGreater(result['provider_peak_rss_bytes'], 0)
        self.assertIn('forked_idle', result['phase_peak_rss_bytes'])
