import json
from pathlib import Path
import sys
import tempfile
from types import SimpleNamespace
import unittest

import psutil

from bench.config import workload
from bench.runner import run_once


class IntegrationTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.path = Path(self.temp.name)
        self.config = workload('bench/workloads/smoke.json')
        (self.path / 'workload.json').write_text(json.dumps(self.config))
        self.options = SimpleNamespace(timeout=5, interval=.1, discovery_interval=.1, rss_limit_mib=256,
                                       process_limit=16)

    def test_real_streams_separate_provider_and_validate_work(self):
        result = run_once([sys.executable, '-m', 'bench.fixture'], self.config,
                          self.options, self.path, 0)
        self.assertEqual(result['status'], 'ok', result)
        self.assertEqual(result['provider']['connections_used'], 4)
        self.assertEqual(result['provider']['completed_requests'], 12)
        self.assertEqual(result['provider']['peak_active_requests'], 4)
        self.assertEqual(result['events']['completed_turns'], 12)
        self.assertEqual(result['target']['processes'], 1)
        self.assertEqual(result['provider_resources']['processes'], 1)
        self.assertGreater(result['target']['rss_bytes'], 0)
        self.assertGreater(result['observer']['self_cpu_seconds'], 0)

    def test_timeout_reclaims_observed_child_in_another_process_group(self):
        pid_file = self.path / 'child.pid'
        script = (
            "import subprocess, sys, time; "
            "p = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(30)'], "
            "start_new_session=True); "
            "open(sys.argv[1], 'w').write(str(p.pid)); "
            "print('{\"event\":\"ready\"}', flush=True); time.sleep(30)"
        )
        self.options.timeout = .7
        result = run_once([sys.executable, '-c', script, str(pid_file)], self.config,
                          self.options, self.path, 0)
        self.assertEqual(result['status'], 'timeout')
        self.assertGreaterEqual(result['target']['processes'], 2)
        pid = int(pid_file.read_text())
        # The orphan may briefly remain a zombie while its new parent reaps it.
        try:
            process = psutil.Process(pid)
            self.assertEqual(process.status(), psutil.STATUS_ZOMBIE)
        except psutil.NoSuchProcess:
            pass

    def test_bad_target_output_is_failure_and_is_not_captured(self):
        marker = 'SYNTHETIC_DO_NOT_STORE_THIS_OUTPUT'
        result = run_once([sys.executable, '-c', f'print({marker!r})'], self.config,
                          self.options, self.path, 0)
        self.assertEqual(result['status'], 'protocol_error')
        for path in self.path.iterdir():
            self.assertNotIn(marker, path.read_text())


if __name__ == '__main__':
    unittest.main()
