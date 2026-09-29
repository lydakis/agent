"""A schedule's fire against a real daemon: what launchd runs, sent to the bot it was made for."""
import json
import os
import subprocess
import threading
import time
import unittest
from pathlib import Path

from bench.targets import clean_env
from tests.test_runtime import ModelFixture

ROOT = Path(__file__).resolve().parent.parent
APP = next((p for p in (ROOT / '.local/target/debug/agent-app', ROOT / '.local/target/release/agent-app') if p.exists()), None)


@unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'set AGENT_TEST_RUNTIME=1 after a Rust release build')
@unittest.skipUnless(APP, 'build the app first: cargo build -p agent-app')
class ScheduleFireTests(ModelFixture):
    def setUp(self):
        super().setUp()
        self.store = self.path / 'state.sqlite'
        self.home = self.path / 'home'
        self.home.mkdir()
        self.common = ['--store', str(self.store), '--provider', f'openai=responses,{self.url}',
                       '--model', 'openai/synthetic-model', '--tools', 'echo,shell']
        self.addCleanup(lambda: subprocess.run([str(self.binary), 'shutdown', '--store', str(self.store)],
                                               env=clean_env(), capture_output=True, timeout=35))

    def agent(self, *args):
        result = subprocess.run([str(self.binary), *args], env=clean_env(), capture_output=True, text=True,
                                timeout=30, cwd=self.path)
        self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
        return result

    def turns(self, bot):
        return json.loads(self.agent('turns', '--store', str(self.store), '--bot', bot).stdout)

    def bot_id(self, bot):
        return next(b['id'] for b in json.loads(self.agent('ls', '--store', str(self.store)).stdout) if b['name'] == bot)

    def fire(self, name, bot, bot_id, message, at=None, app=APP, socket=None, env=None):
        # What a schedule's plist has launchd run.
        args = [str(app), '--schedule-fire', '--name', name, '--bot', bot, '--bot-id', str(bot_id),
                '--when', 'every 30m', *(['--at', str(at)] if at else []), '--store', str(self.store),
                *(['--socket', str(socket)] if socket else []), '--', message]
        env = {**clean_env(), 'HOME': str(self.home), **(env or {})}
        result = subprocess.run(args, env=env, capture_output=True, text=True, timeout=60)
        self.assertEqual(result.returncode, 0, result.stderr)
        last = self.home / '.agent/schedules' / f'{name}.json'
        return json.loads(last.read_text()) if last.exists() else None

    def settle(self, bot):
        self.agent('wait', '--store', str(self.store), f"turn:{bot}/{self.turns(bot)[-1]['turn']}")

    def test_a_fire_wakes_its_resting_bot_and_skips_a_working_one(self):
        self.agent('run', *self.common, '--new', '--bot', 'p.task', 'hello')
        bot_id = self.bot_id('p.task')
        sent = self.fire('p.task', 'p.task', bot_id, 'Check the PR again.')
        self.assertEqual(sent['last']['outcome'], 'sent', sent)
        self.settle('p.task')
        self.assertEqual(self.turns('p.task')[-1]['prompt_preview'], 'Check the PR again.')
        # A working bot is not interrupted or queued behind: that time is skipped.
        self.model.release_headers = threading.Event()
        self.model.all_streaming = self.model.release_headers
        self.addCleanup(self.model.release_headers.set)
        self.agent('run', '--store', str(self.store), '--bot', 'p.task', '--detach', 'gate')
        time.sleep(0.5)
        skipped = self.fire('p.task', 'p.task', bot_id, 'Check the PR again.')
        self.assertEqual(skipped['last']['outcome'], 'skipped', skipped)
        self.model.release_headers.set()
        self.settle('p.task')
        self.assertEqual(len(self.turns('p.task')), 3)

    def test_a_one_off_waits_for_a_working_bot_instead_of_skipping(self):
        self.agent('run', *self.common, '--new', '--bot', 'p.task', 'hello')
        bot_id = self.bot_id('p.task')
        self.model.release_headers = threading.Event()
        self.model.all_streaming = self.model.release_headers
        self.addCleanup(self.model.release_headers.set)
        self.agent('run', '--store', str(self.store), '--bot', 'p.task', '--detach', 'gate')
        time.sleep(0.5)
        # Delivered, it leaves no row behind.
        self.assertIsNone(self.fire('p.task', 'p.task', bot_id, 'Look again.', at=int(time.time())))
        self.model.release_headers.set()
        self.settle('p.task')
        self.assertEqual([t['prompt_preview'] for t in self.turns('p.task')][1:], ['gate', 'Look again.'])

    def test_a_fire_starts_its_stopped_daemon_on_the_socket_it_was_made_with(self):
        self.agent('run', *self.common, '--new', '--bot', 'p.task', 'hello')
        bot_id = self.bot_id('p.task')
        self.agent('shutdown', '--store', str(self.store))
        # The app starts a daemon with the `agent` it ships beside it.
        bundle = self.path / 'bundle'
        bundle.mkdir()
        app = bundle / 'agent-app'
        try:
            os.link(APP, app)
        except OSError:
            import shutil
            shutil.copy2(APP, app)
        (bundle / 'agent').symlink_to(self.binary)
        socket = self.path / 'own.sock'
        self.addCleanup(lambda: subprocess.run([str(self.binary), 'shutdown', '--store', str(self.store),
                                                '--socket', str(socket)], env=clean_env(), capture_output=True, timeout=35))
        sent = self.fire('p.task', 'p.task', bot_id, 'Morning check.', app=app, socket=socket,
                         env={'AGENT_PROVIDER': f'openai=responses,{self.url}', 'SHELL': '/bin/sh'})
        self.assertEqual(sent['last']['outcome'], 'sent', sent)
        self.assertTrue(socket.exists())
        turns = json.loads(self.agent('turns', '--store', str(self.store), '--socket', str(socket), '--bot', 'p.task').stdout)
        self.assertEqual(turns[-1]['prompt_preview'], 'Morning check.')

    def test_a_fire_never_reaches_a_bot_made_again_under_the_name(self):
        self.agent('run', *self.common, '--new', '--bot', 'p.task', 'hello')
        old = self.bot_id('p.task')
        self.agent('rm', '--store', str(self.store), '--bot', 'p.task')
        self.agent('run', *self.common, '--new', '--bot', 'p.task', 'hello again')
        gone = self.fire('p.task', 'p.task', old, 'Check the PR again.')
        self.assertEqual(gone['last']['outcome'], 'gone', gone)
        # The schedule ended on its own, and says why until it is removed.
        self.assertEqual(gone['message'], 'Check the PR again.')
        self.assertEqual(len(self.turns('p.task')), 1)

    def test_a_one_off_ignores_its_date_a_year_early(self):
        self.agent('run', *self.common, '--new', '--bot', 'p.task', 'hello')
        early = self.fire('p.task', 'p.task', self.bot_id('p.task'), 'x', at=int(time.time()) + 3600)
        self.assertIsNone(early)
        self.assertEqual(len(self.turns('p.task')), 1)

    def test_add_from_an_agents_shell_needs_launchd(self):
        # Here there is no launchd: the schedule is refused and nothing is left behind.
        self.agent('run', *self.common, '--new', '--bot', 'p.task', 'hello')
        out = self.path / 'added.json'
        self.agent('run', '--store', str(self.store), '--bot', 'p.task',
                   f"shell:HOME='{self.home}' '{APP}' --schedule add --every 30m -- check > '{out}' 2>&1")
        text = out.read_text()
        if os.uname().sysname == 'Darwin':
            self.skipTest('adds a real LaunchAgent on macOS')
        self.assertIn('schedules_unsupported', text)
        self.assertEqual(list((self.home / 'Library/LaunchAgents').glob('*.plist')), [])


if __name__ == '__main__':
    unittest.main()
