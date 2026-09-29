"""A schedule's fire against a real daemon: what launchd runs, sent to the bot it was made for."""
import json
import os
import shutil
import subprocess
import sys
import threading
import tempfile
import time
import unittest
from pathlib import Path
from xml.sax.saxutils import escape as xml_escape

from bench.targets import clean_env
from bench.socket_client import Connection
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

    def store_identity(self):
        # What the daemon announces in `ready`, and `add` keeps.
        if not hasattr(self, '_store_id'):
            self._store_id = json.loads(self.agent('start', '--store', str(self.store)).stdout)['store']['identity']
        return self._store_id

    def fire(self, name, bot, bot_id, message, at=None, app=APP, socket=None, env=None, store_id=None, not_before=None):
        # What a schedule's plist has launchd run.
        store_id = store_id or self.store_identity()
        args = [str(app), '--schedule-fire', '--name', name, '--bot', bot, '--bot-id', str(bot_id),
                '--generation', str(time.time_ns()), '--when', 'every 30m', *(['--at', str(at)] if at else []), '--store', str(self.store),
                *(['--socket', str(socket)] if socket else []),
                '--store-id', store_id, *(['--not-before', str(not_before)] if not_before else []), '--', message]
        env = {**clean_env(), 'HOME': str(self.home), **(env or {})}
        # A fire records and ends only the schedule its plist still holds.
        plist = self.home / 'Library/LaunchAgents' / f'me.lydakis.agent.schedule.{name}.plist'
        plist.parent.mkdir(parents=True, exist_ok=True)
        strings = ''.join(f'<string>{xml_escape(a)}</string>' for a in args)
        plist.write_text(f'<plist><dict><key>ProgramArguments</key><array>{strings}</array></dict></plist>')
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
        # The actual schedule submit preserves its automated origin in the
        # approver's input, the durable event, and the displayed history.
        connection = Connection(str(self.store) + '.sock')
        try:
            turn = sent['last']['turn']
            prompts = connection.request('prompts', bot='p.task', turn=turn)['result']['prompts']
            self.assertEqual(prompts[0]['origin'], 'schedule')
            events = connection.request('events', bot='p.task', after=0, limit=256)['result']['events']
            accepted = next(e['data'] for e in events if e['event'] == 'accepted' and e['turn'] == turn)
            self.assertEqual(accepted['origin'], 'schedule')
            item = connection.request('history_items', bot='p.task', nodes=[accepted['node']])['result']['items'][0]
            self.assertEqual(item['origin'], 'schedule')
        finally:
            connection.close()
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

    def test_a_recurring_fire_waits_for_its_first_interval(self):
        self.agent('run', *self.common, '--new', '--bot', 'p.task', 'hello')
        self.assertIsNone(self.fire('p.task', 'p.task', self.bot_id('p.task'), 'x',
                                    not_before=int(time.time()) + 1800))
        self.assertEqual(len(self.turns('p.task')), 1)

    def test_a_stale_shell_cannot_schedule_a_replacement_bot(self):
        self.agent('run', *self.common, '--new', '--bot', 'p.task', 'hello')
        old = self.bot_id('p.task')
        self.agent('rm', '--store', str(self.store), '--bot', 'p.task')
        self.agent('run', *self.common, '--new', '--bot', 'p.task', 'again')
        result = subprocess.run([str(APP), '--schedule', 'add', '--every', '30m', '--', 'x'],
            env={**clean_env(), 'HOME': str(self.home), 'AGENT_STORE': str(self.store),
                 'AGENT_BOT': 'p.task', 'AGENT_BOT_ID': str(old)},
            capture_output=True, text=True, timeout=30)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('bot_not_found', result.stderr)
        self.assertEqual(list((self.home / 'Library/LaunchAgents').glob('*.plist')), [])

    def test_a_one_off_waits_for_a_working_bot_instead_of_skipping(self):
        self.agent('run', *self.common, '--new', '--bot', 'p.task', 'hello')
        bot_id = self.bot_id('p.task')
        self.model.release_headers = threading.Event()
        self.model.all_streaming = self.model.release_headers
        self.addCleanup(self.model.release_headers.set)
        self.agent('run', '--store', str(self.store), '--bot', 'p.task', '--detach', 'gate')
        time.sleep(0.5)
        fired = self.fire('p.task', 'p.task', bot_id, 'Look again.', at=int(time.time()))
        if sys.platform == 'darwin':
            # Delivered, it leaves no row behind.
            self.assertIsNone(fired)
        else:
            # No launchd to unload it: it stays, with what it did.
            self.assertEqual(fired['last']['outcome'], 'sent', fired)
        self.model.release_headers.set()
        self.settle('p.task')
        self.assertEqual([t['prompt_preview'] for t in self.turns('p.task')][1:], ['gate', 'Look again.'])

    def test_a_fire_starts_its_stopped_daemon_on_the_socket_it_was_made_with(self):
        self.agent('run', *self.common, '--new', '--bot', 'p.task', 'hello')
        bot_id = self.bot_id('p.task')
        self.store_identity()
        self.agent('shutdown', '--store', str(self.store))
        # The app starts a daemon with the `agent` it ships beside it.
        bundle = self.path / 'bundle'
        bundle.mkdir()
        app = bundle / 'agent-app'
        try:
            os.link(APP, app)
        except OSError:
            shutil.copy2(APP, app)
        (bundle / 'agent').symlink_to(self.binary)
        # A deep checkout's path would pass macOS's 104-byte limit for a socket's.
        short = Path(tempfile.mkdtemp(prefix='ag', dir='/tmp'))
        self.addCleanup(shutil.rmtree, short, True)
        socket = short / 'own.sock'
        self.addCleanup(lambda: subprocess.run([str(self.binary), 'shutdown', '--store', str(self.store),
                                                '--socket', str(socket)], env=clean_env(), capture_output=True, timeout=35))
        sent = self.fire('p.task', 'p.task', bot_id, 'Morning check.', app=app, socket=socket,
                         env={'AGENT_PROVIDER': f'openai=responses,{self.url}', 'SHELL': '/bin/sh'})
        self.assertEqual(sent['last']['outcome'], 'sent', sent)
        self.assertTrue(socket.exists())
        turns = json.loads(self.agent('turns', '--store', str(self.store), '--socket', str(socket), '--bot', 'p.task').stdout)
        self.assertEqual(turns[-1]['prompt_preview'], 'Morning check.')

    def test_a_fire_never_reaches_another_stores_daemon(self):
        self.agent('run', *self.common, '--new', '--bot', 'p.task', 'hello')
        other = self.fire('p.task', 'p.task', self.bot_id('p.task'), 'x', store_id='0' * 32)
        self.assertEqual(other['last']['outcome'], 'failed', other)
        self.assertIn('store_mismatch', other['last']['detail'])
        self.assertEqual(len(self.turns('p.task')), 1)

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
        early = self.fire('p.task', 'p.task', self.bot_id('p.task'), 'x', at=int(time.time()) + 10 * 86400)
        self.assertIsNone(early)
        self.assertEqual(len(self.turns('p.task')), 1)

    def test_a_one_off_months_late_is_its_next_year_and_not_sent(self):
        self.agent('run', *self.common, '--new', '--bot', 'p.task', 'hello')
        late = self.fire('p.task', 'p.task', self.bot_id('p.task'), 'x', at=int(time.time()) - 200 * 86400)
        self.assertEqual(late['last']['outcome'], 'missed', late)
        self.assertEqual(len(self.turns('p.task')), 1)

    def test_a_fire_records_nothing_for_a_schedule_replaced_or_removed_meanwhile(self):
        self.agent('run', *self.common, '--new', '--bot', 'p.task', 'hello')
        bot_id = self.bot_id('p.task')
        first = self.fire('p.task', 'p.task', bot_id, 'first')
        # The plist now holds another generation of the same message: the old job's fire leaves its result alone.
        plist = self.home / 'Library/LaunchAgents/me.lydakis.agent.schedule.p.task.plist'
        plist.write_text(plist.read_text().replace(first['generation'], first['generation'] + '-replacement'))
        last = self.home / '.agent/schedules/p.task.json'
        last.unlink()
        args = [str(APP), '--schedule-fire', '--name', 'p.task', '--bot', 'p.task', '--bot-id', str(bot_id),
                '--generation', first['generation'], '--when', 'every 30m', '--store', str(self.store), '--store-id', self.store_identity(), '--', 'first']
        result = subprocess.run(args, env={**clean_env(), 'HOME': str(self.home)}, capture_output=True, text=True,
                                timeout=60)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(last.exists())

    def test_add_from_an_agents_shell_needs_launchd(self):
        # Here there is no launchd: the schedule is refused and nothing is left behind.
        if os.uname().sysname == 'Darwin':
            self.skipTest('would add a real LaunchAgent on macOS')
        self.agent('run', *self.common, '--new', '--bot', 'p.task', 'hello')
        out = self.path / 'added.json'
        self.agent('run', '--store', str(self.store), '--bot', 'p.task',
                   f"shell:HOME='{self.home}' '{APP}' --schedule add --every 30m -- check > '{out}' 2>&1")
        text = out.read_text()
        self.assertIn('schedules_unsupported', text)
        self.assertEqual(list((self.home / 'Library/LaunchAgents').glob('*.plist')), [])

    @unittest.skipUnless(sys.platform == 'darwin' and os.environ.get('AGENT_TEST_LAUNCHD') == '1',
                         'real launchd: macOS with AGENT_TEST_LAUNCHD=1')
    def test_real_launchd_fires_replaces_and_removes(self):
        # The plists live under this test's HOME, so nothing loads at the next login.
        name = f'ztest-{os.getpid()}'
        label = f'me.lydakis.agent.schedule.{name}'
        target = f'gui/{os.getuid()}/{label}'
        plist = self.home / f'Library/LaunchAgents/{label}.plist'
        self.addCleanup(lambda: subprocess.run(['/bin/launchctl', 'bootout', target], capture_output=True))
        self.agent('run', *self.common, '--new', '--bot', 'p.task', 'hello')
        env = {**clean_env(), 'HOME': str(self.home), 'AGENT_STORE': str(self.store)}

        def schedule(*args, ok=True):
            result = subprocess.run([str(APP), '--schedule', *args], env=env, capture_output=True, text=True,
                                    timeout=60)
            if ok:
                self.assertEqual(result.returncode, 0, result.stderr)
            return result

        def loaded():
            return subprocess.run(['/bin/launchctl', 'print', target], capture_output=True).returncode == 0

        def until(check, seconds):
            deadline = time.monotonic() + seconds
            while time.monotonic() < deadline and not check():
                time.sleep(1)
            return check()

        # A one-off: launchd runs it at its minute, the bot gets the message, and it ends itself.
        schedule('add', '--bot', 'p.task', '--name', name, '--in', '1m', '--', 'launchd says hi')
        self.assertTrue(loaded())
        self.assertTrue(until(lambda: len(self.turns('p.task')) == 2, 150), 'launchd did not fire it')
        self.assertTrue(until(lambda: not loaded() and not plist.exists(), 30), 'it did not end itself')
        self.assertEqual(json.loads(schedule('ls').stdout)['schedules'], [])
        # A repeating one, replaced: one job, the new one.
        schedule('add', '--bot', 'p.task', '--name', name, '--every', '30m', '--', 'a')
        schedule('add', '--bot', 'p.task', '--name', name, '--every', '1h', '--', 'b')
        self.assertTrue(loaded())
        rows = json.loads(schedule('ls').stdout)['schedules']
        self.assertEqual([(r['name'], r['when'], r['message']) for r in rows], [(name, 'every 1h', 'b')])
        # A plist launchd no longer has, as after a failed reload: rm still removes it.
        subprocess.run(['/bin/launchctl', 'bootout', target], check=True, capture_output=True)
        schedule('rm', name)
        self.assertFalse(plist.exists())
        # A job loaded without its plist, as after an end cut short: rm reaches it by name.
        schedule('add', '--bot', 'p.task', '--name', name, '--every', '30m', '--', 'c')
        plist.unlink()
        schedule('rm', name)
        self.assertFalse(loaded())
        # Nothing left: bootout's not-loaded answer reads as that.
        self.assertIn('schedule_not_found', schedule('rm', name, ok=False).stderr)


if __name__ == '__main__':
    unittest.main()
