"""Companions: commands the daemon keeps running for as long as it runs."""
import json
import os
import subprocess
import time
import unittest

from bench.targets import clean_env
from tests.test_runtime import ModelFixture


def alive(pid):
    try:
        os.kill(pid, 0)
        return True
    except OSError:
        return False


@unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'set AGENT_TEST_RUNTIME=1 after a Rust release build')
class CompanionTests(ModelFixture):
    def setUp(self):
        super().setUp()
        self.store = self.path / 'state.sqlite'
        self.daemon = ['--store', str(self.store), '--provider', f'openai=responses,{self.url}']
        self.addCleanup(lambda: subprocess.run([str(self.binary), 'shutdown', '--store', str(self.store)],
                                               env=clean_env(), capture_output=True, timeout=35))

    def agent(self, *args, check=True):
        result = subprocess.run([str(self.binary), *args], env=clean_env(), capture_output=True, text=True,
                                timeout=30, cwd=self.path)
        if check:
            self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
        return result

    def listed(self):
        return {c['name']: c for c in json.loads(self.agent('companion', *self.daemon, 'ls').stdout)['companions']}

    def until(self, what, timeout=15):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            value = what()
            if value:
                return value
            time.sleep(0.1)
        self.fail('timed out')

    def test_a_companion_lives_and_dies_with_the_daemon_and_comes_back_with_the_next(self):
        marks = self.path / 'marks'
        script = f'echo "$AGENT_COMPANION $AGENT_SOCKET $GREETING" >> {marks}; exec sleep 600'
        added = json.loads(self.agent('companion', *self.daemon, 'add', 'helper', '--env', 'GREETING=hi',
                                      '--cwd', str(self.path), '--', 'sh', '-c', script).stdout)
        self.assertTrue(added['changed'])
        self.assertEqual((added['companion']['command'], added['companion']['args']), ('sh', ['-c', script]))
        pid = self.until(lambda: self.listed()['helper']['pid'])
        socket = json.loads(self.agent('start', *self.daemon).stdout)['socket']
        self.until(lambda: marks.exists() and marks.read_text())
        self.assertEqual(marks.read_text().split(), ['helper', socket, 'hi'])
        # The same spec again keeps the run it has.
        again = json.loads(self.agent('companion', *self.daemon, 'add', 'helper', '--env', 'GREETING=hi',
                                      '--cwd', str(self.path), '--', 'sh', '-c', script).stdout)
        self.assertFalse(again['changed'])
        self.assertEqual(self.listed()['helper']['pid'], pid)
        self.assertEqual(json.loads(self.agent('stats', *self.daemon).stdout)['companions'][0]['pid'], pid)
        # It stops with the daemon, and the next daemon starts it again.
        self.agent('shutdown', '--store', str(self.store))
        self.assertFalse(alive(pid))
        self.agent('start', *self.daemon)
        second = self.until(lambda: self.listed()['helper']['pid'])
        self.assertNotEqual(second, pid)
        self.assertTrue(alive(second))
        removed = json.loads(self.agent('companion', *self.daemon, 'rm', 'helper').stdout)
        self.assertEqual(removed, {'name': 'helper', 'removed': True})
        self.until(lambda: not alive(second))
        self.assertEqual(self.listed(), {})
        self.assertFalse(json.loads(self.agent('companion', *self.daemon, 'rm', 'helper').stdout)['removed'])

    def test_a_companion_that_exits_starts_again(self):
        self.agent('companion', *self.daemon, 'add', 'flaky', '--', 'sh', '-c', 'exit 7')
        shown = self.until(lambda: (c := self.listed()['flaky'])['restarts'] >= 1 and c)
        self.assertEqual(shown['last_exit']['code'], 7)

    def test_a_companion_s_session_keeps_no_daemon_from_going_idle(self):
        daemon = json.loads(self.agent('start', *self.daemon, '--idle-exit', '2s').stdout)['pid']
        # It holds a session open for as long as it runs.
        self.agent('companion', '--store', str(self.store), 'add', 'watcher', '--',
                   str(self.binary), 'follow', '--all', '--store', str(self.store))
        pid = self.until(lambda: self.listed()['watcher']['pid'])
        # Asked nothing more, the daemon goes, and its companion with it.
        self.until(lambda: not alive(daemon), timeout=20)
        self.until(lambda: not alive(pid))

    def test_a_bad_companion_is_refused_by_name(self):
        refused = self.agent('companion', *self.daemon, 'add', '.x', '--', 'true', check=False)
        self.assertNotEqual(refused.returncode, 0)
        self.assertEqual(json.loads(refused.stderr.strip().splitlines()[-1])['error'], 'invalid_companion')
        self.assertNotEqual(self.agent('companion', *self.daemon, 'add', 'x', check=False).returncode, 0)


if __name__ == '__main__':
    unittest.main()
