"""A trigger's fire against a real daemon: what launchd runs, sent to the bot it was made for."""
import json
import os
import re
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
# What a fire puts before the message: `[trigger NAME · YYYY-MM-DD HH:MM · why]`.
LINE = r'\[trigger {} · \d{{4}}-\d\d-\d\d \d\d:\d\d · {}\]\n'


@unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'set AGENT_TEST_RUNTIME=1 after a Rust release build')
@unittest.skipUnless(APP, 'build the app first: cargo build -p agent-app')
class TriggerFireTests(ModelFixture):
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

    def bundle(self):
        # The app starts a daemon, and a `--start` agent, with the `agent` it ships beside it.
        bundle = self.path / 'bundle'
        if not bundle.exists():
            bundle.mkdir()
            try:
                os.link(APP, bundle / 'agent-app')
            except OSError:
                shutil.copy2(APP, bundle / 'agent-app')
            (bundle / 'agent').symlink_to(self.binary)
        return bundle / 'agent-app'

    def fire(self, name, message, target=None, when='every 30m', extra=(), at=None, app=APP, socket=None,
             env=None, store_id=None, not_before=None, generation=None, wait=True):
        # What a trigger's plist has launchd run.
        target = target or ['--bot', name, '--bot-id', str(self.bot_id(name))]
        args = [str(app), '--trigger-fire', '--name', name, *target,
                '--generation', generation or str(time.time_ns()), '--when', when, *(['--at', str(at)] if at else []),
                *extra, '--store', str(self.store), *(['--socket', str(socket)] if socket else []),
                '--store-id', store_id or self.store_identity(),
                *(['--not-before', str(not_before)] if not_before else []), '--', message]
        env = {**clean_env(), 'HOME': str(self.home), **(env or {})}
        # A fire records and ends only the trigger its plist still holds.
        plist = self.home / 'Library/LaunchAgents' / f'me.lydakis.agent.trigger.{name}.plist'
        plist.parent.mkdir(parents=True, exist_ok=True)
        strings = ''.join(f'<string>{xml_escape(a)}</string>' for a in args)
        plist.write_text(f'<plist><dict><key>ProgramArguments</key><array>{strings}</array></dict></plist>')
        if not wait:
            return subprocess.Popen(args, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        result = subprocess.run(args, env=env, capture_output=True, text=True, timeout=60)
        self.assertEqual(result.returncode, 0, result.stderr)
        last = self.home / '.agent/triggers' / f'{name}.json'
        return json.loads(last.read_text()) if last.exists() else None

    def settle(self, bot):
        self.agent('wait', '--store', str(self.store), f"turn:{bot}/{self.turns(bot)[-1]['turn']}")

    def test_a_fire_wakes_its_resting_bot_and_skips_a_working_one(self):
        self.agent('run', *self.common, '--new', '--bot', 'p.task', 'hello')
        sent = self.fire('p.task', 'Check the PR again.')
        self.assertEqual(sent['last']['outcome'], 'sent', sent)
        self.assertEqual(sent['sent'], 1)
        self.settle('p.task')
        # The message, after one line with the local fire time and what fired it.
        self.assertRegex(self.turns('p.task')[-1]['prompt_preview'],
                         '^' + LINE.format('p.task', 'every 30m') + 'Check the PR again.$')
        # The submit keeps its automated origin in the approver's input, the
        # durable event, and the displayed history.
        connection = Connection(str(self.store) + '.sock')
        try:
            turn = sent['last']['turn']
            prompts = connection.request('prompts', bot='p.task', turn=turn)['result']['prompts']
            self.assertEqual(prompts[0]['origin'], 'trigger')
            events = connection.request('events', bot='p.task', after=0, limit=256)['result']['events']
            accepted = next(e['data'] for e in events if e['event'] == 'accepted' and e['turn'] == turn)
            self.assertEqual(accepted['origin'], 'trigger')
            item = connection.request('history_items', bot='p.task', nodes=[accepted['node']])['result']['items'][0]
            self.assertEqual(item['origin'], 'trigger')
        finally:
            connection.close()
        # A working bot is not interrupted or queued behind: that time is skipped.
        self.model.release_headers = threading.Event()
        self.model.all_streaming = self.model.release_headers
        self.addCleanup(self.model.release_headers.set)
        self.agent('run', '--store', str(self.store), '--bot', 'p.task', '--detach', 'gate')
        time.sleep(0.5)
        skipped = self.fire('p.task', 'Check the PR again.')
        self.assertEqual(skipped['last']['outcome'], 'skipped', skipped)
        self.model.release_headers.set()
        self.settle('p.task')
        self.assertEqual(len(self.turns('p.task')), 3)

    def test_a_recurring_fire_waits_for_its_first_interval(self):
        self.agent('run', *self.common, '--new', '--bot', 'p.task', 'hello')
        self.assertIsNone(self.fire('p.task', 'x', not_before=int(time.time()) + 1800))
        self.assertEqual(len(self.turns('p.task')), 1)

    def test_a_stale_shell_cannot_add_a_trigger_for_a_replacement_bot(self):
        self.agent('run', *self.common, '--new', '--bot', 'p.task', 'hello')
        old = self.bot_id('p.task')
        self.agent('rm', '--store', str(self.store), '--bot', 'p.task')
        self.agent('run', *self.common, '--new', '--bot', 'p.task', 'again')
        result = subprocess.run([str(APP), '--trigger', 'add', '--every', '30m', '--', 'x'],
            env={**clean_env(), 'HOME': str(self.home), 'AGENT_STORE': str(self.store),
                 'AGENT_BOT': 'p.task', 'AGENT_BOT_ID': str(old)},
            capture_output=True, text=True, timeout=30)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(json.loads(result.stderr)['error'], 'bot_not_found')
        self.assertEqual(list((self.home / 'Library/LaunchAgents').glob('*.plist')), [])

    def test_a_one_off_waits_for_a_working_bot_instead_of_skipping(self):
        self.agent('run', *self.common, '--new', '--bot', 'p.task', 'hello')
        self.model.release_headers = threading.Event()
        self.model.all_streaming = self.model.release_headers
        self.addCleanup(self.model.release_headers.set)
        self.agent('run', '--store', str(self.store), '--bot', 'p.task', '--detach', 'gate')
        time.sleep(0.5)
        fired = self.fire('p.task', 'Look again.', at=int(time.time()))
        if sys.platform == 'darwin':
            # Delivered, it leaves no row behind.
            self.assertIsNone(fired)
        else:
            # No launchd to unload it: it stays, with what it did.
            self.assertEqual(fired['last']['outcome'], 'sent', fired)
        self.model.release_headers.set()
        self.settle('p.task')
        previews = [t['prompt_preview'] for t in self.turns('p.task')][1:]
        self.assertEqual(previews[0], 'gate')
        self.assertTrue(previews[1].endswith(']\nLook again.'), previews)

    def test_a_fire_starts_its_stopped_daemon_on_the_socket_it_was_made_with(self):
        self.agent('run', *self.common, '--new', '--bot', 'p.task', 'hello')
        target = ['--bot', 'p.task', '--bot-id', str(self.bot_id('p.task'))]
        self.store_identity()
        self.agent('shutdown', '--store', str(self.store))
        # A deep checkout's path would pass macOS's 104-byte limit for a socket's.
        short = Path(tempfile.mkdtemp(prefix='ag', dir='/tmp'))
        self.addCleanup(shutil.rmtree, short, True)
        socket = short / 'own.sock'
        self.addCleanup(lambda: subprocess.run([str(self.binary), 'shutdown', '--store', str(self.store),
                                                '--socket', str(socket)], env=clean_env(), capture_output=True, timeout=35))
        sent = self.fire('p.task', 'Morning check.', target=target, app=self.bundle(), socket=socket,
                         env={'AGENT_PROVIDER': f'openai=responses,{self.url}', 'SHELL': '/bin/sh'})
        self.assertEqual(sent['last']['outcome'], 'sent', sent)
        self.assertTrue(socket.exists())
        turns = json.loads(self.agent('turns', '--store', str(self.store), '--socket', str(socket), '--bot', 'p.task').stdout)
        self.assertTrue(turns[-1]['prompt_preview'].endswith(']\nMorning check.'))

    def test_a_fire_never_reaches_another_stores_daemon(self):
        self.agent('run', *self.common, '--new', '--bot', 'p.task', 'hello')
        other = self.fire('p.task', 'x', store_id='0' * 32)
        self.assertEqual(other['last']['outcome'], 'failed', other)
        self.assertIn('store_mismatch', other['last']['detail'])
        self.assertEqual(len(self.turns('p.task')), 1)

    def test_a_fire_never_reaches_a_bot_made_again_under_the_name(self):
        self.agent('run', *self.common, '--new', '--bot', 'p.task', 'hello')
        old = self.bot_id('p.task')
        self.agent('rm', '--store', str(self.store), '--bot', 'p.task')
        self.agent('run', *self.common, '--new', '--bot', 'p.task', 'hello again')
        gone = self.fire('p.task', 'Check the PR again.', target=['--bot', 'p.task', '--bot-id', str(old)])
        self.assertEqual(gone['last']['outcome'], 'gone', gone)
        # The trigger ended on its own, and says why until it is removed.
        self.assertEqual(gone['message'], 'Check the PR again.')
        self.assertEqual(len(self.turns('p.task')), 1)

    def test_a_one_off_ignores_its_date_a_year_early(self):
        self.agent('run', *self.common, '--new', '--bot', 'p.task', 'hello')
        early = self.fire('p.task', 'x', at=int(time.time()) + 10 * 86400)
        self.assertIsNone(early)
        self.assertEqual(len(self.turns('p.task')), 1)

    def test_a_one_off_months_late_is_its_next_year_and_not_sent(self):
        self.agent('run', *self.common, '--new', '--bot', 'p.task', 'hello')
        late = self.fire('p.task', 'x', at=int(time.time()) - 200 * 86400)
        self.assertEqual(late['last']['outcome'], 'missed', late)
        self.assertEqual(len(self.turns('p.task')), 1)

    def test_a_fire_records_nothing_for_a_trigger_replaced_or_removed_meanwhile(self):
        self.agent('run', *self.common, '--new', '--bot', 'p.task', 'hello')
        first = self.fire('p.task', 'first', generation='g1')
        # The plist now holds another generation of the same message: the old job's fire leaves its result alone.
        plist = self.home / 'Library/LaunchAgents/me.lydakis.agent.trigger.p.task.plist'
        args = re.findall(r'<string>(.*?)</string>', plist.read_text())
        plist.write_text(plist.read_text().replace('<string>g1</string>', '<string>g2</string>'))
        last = self.home / '.agent/triggers/p.task.json'
        last.unlink()
        result = subprocess.run(args, env={**clean_env(), 'HOME': str(self.home)}, capture_output=True, text=True,
                                timeout=60)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(first['generation'], 'g1')
        self.assertFalse(last.exists())

    def test_a_start_trigger_makes_its_agent_once_and_messages_it_after(self):
        self.agent('run', *self.common, '--new', '--bot', 'p.lead', 'hello')
        lead = self.bot_id('p.lead')
        work = self.path / 'work'
        work.mkdir()
        (work / 'AGENTS.md').write_text('Synthetic project rule.\n')
        target = ['--start', 'p.review', '--model', 'openai/synthetic-model', '--by', 'p.lead', '--by-id', str(lead)]
        extra = ['--dir', str(work)]
        env = {'AGENT_PROVIDER': f'openai=responses,{self.url}'}
        first = self.fire('p.review', 'Review the newest commit.', target=target, extra=extra, app=self.bundle(),
                          env=env, generation='g1')
        self.assertEqual(first['last']['outcome'], 'sent', first)
        self.assertTrue(first['last']['started'])
        review = self.bot_id('p.review')
        self.assertEqual((first['started_id'], first['bot_id']), (review, review))
        record = next(b for b in json.loads(self.agent('ls', '--store', str(self.store)).stdout) if b['name'] == 'p.review')
        # Made in the folder `add` ran in, under the agent that added it.
        self.assertEqual((record['workspace'], record['created_by']), (str(work), 'p.lead'))
        self.settle('p.review')
        again = self.fire('p.review', 'Review the newest commit.', target=target, extra=extra, app=self.bundle(),
                          env=env, generation='g1')
        self.assertEqual((again['last']['outcome'], again['sent']), ('sent', 2), again)
        self.assertNotIn('started', again['last'])
        self.settle('p.review')
        self.assertEqual(len(self.turns('p.review')), 2)
        # A fire of a new trigger for that name finds an agent it did not start.
        taken = self.fire('p.review', 'x', target=target, extra=extra, app=self.bundle(), env=env, generation='g2')
        self.assertEqual(taken['last']['outcome'], 'failed', taken)
        self.assertEqual(len(self.turns('p.review')), 2)

    def test_a_start_trigger_cut_short_while_it_waits_messages_the_agent_it_made(self):
        self.agent('run', *self.common, '--new', '--bot', 'p.lead', 'hello')
        lead = self.bot_id('p.lead')
        target = ['--start', 'p.review', '--model', 'openai/synthetic-model', '--by', 'p.lead', '--by-id', str(lead)]
        extra = ['--dir', str(self.path), '--reply-to', 'p.lead', '--reply-to-id', str(lead)]
        env = {'AGENT_PROVIDER': f'openai=responses,{self.url}'}
        # Its turn holds, so the fire waits to pass its answer on, and is stopped there.
        self.model.release_headers = threading.Event()
        self.model.all_streaming = self.model.release_headers
        self.addCleanup(self.model.release_headers.set)
        running = self.fire('p.review', 'Review.', target=target, extra=extra, app=self.bundle(), env=env,
                            generation='g1', wait=False)
        last = self.home / '.agent/triggers/p.review.json'
        deadline = time.time() + 20
        while time.time() < deadline and not (last.exists() and json.loads(last.read_text()).get('started_id')):
            time.sleep(0.05)
        running.kill()
        running.wait()
        made = self.bot_id('p.review')
        self.assertEqual(json.loads(last.read_text())['started_id'], made)
        self.model.release_headers.set()
        self.settle('p.review')
        again = self.fire('p.review', 'Review.', target=target, extra=extra, app=self.bundle(), env=env,
                          generation='g1')
        self.assertEqual(again['last']['outcome'], 'sent', again)
        self.assertEqual(again['bot_id'], made)

    def test_an_answer_goes_to_the_reply_agent(self):
        self.agent('run', *self.common, '--new', '--bot', 'p.lead', 'hello')
        self.agent('run', *self.common, '--new', '--bot', 'p.task', 'hello')
        lead = self.bot_id('p.lead')
        sent = self.fire('p.task', 'Check the PR.', extra=['--reply-to', 'p.lead', '--reply-to-id', str(lead)])
        self.assertEqual(sent['last']['reply']['outcome'], 'sent', sent)
        self.settle('p.lead')
        task_turn = sent['last']['turn']
        answer = self.turns('p.lead')[-1]
        self.assertRegex(answer['prompt_preview'], '^' + LINE.format('p.task', f'p.task turn {task_turn} completed'))
        # It is the task's answer, and says so in the protocol, not only in its first line.
        connection = Connection(str(self.store) + '.sock')
        try:
            events = connection.request('events', bot='p.lead', after=0, limit=256)['result']['events']
            accepted = next(e['data'] for e in events if e['event'] == 'accepted' and e['turn'] == answer['turn'])
            self.assertEqual(accepted['from'], {'bot': 'p.task', 'turn': task_turn, 'id': self.bot_id('p.task')})
            self.assertNotIn('origin', accepted)
            # The task's own turn names where its answer goes.
            self.assertEqual(self.turns('p.task')[-1]['request_id'].rsplit('-to-', 1)[1], str(lead))
        finally:
            connection.close()

    def test_a_one_off_whose_gate_says_no_ends_saying_so(self):
        self.agent('run', *self.common, '--new', '--bot', 'p.task', 'hello')
        no = self.fire('p.task', 'x', at=int(time.time()), extra=['--if', 'exit 1', '--dir', str(self.path)])
        self.assertEqual(len(self.turns('p.task')), 1)
        if sys.platform == 'darwin':
            self.assertEqual(no['last']['outcome'], 'declined', no)
            self.assertFalse((self.home / 'Library/LaunchAgents/me.lydakis.agent.trigger.p.task.plist').exists())
        else:
            # No launchd to unload it: it stays, with why.
            self.assertEqual(no['last']['outcome'], 'declined', no)

    def test_a_gate_that_says_no_costs_no_turn(self):
        self.agent('run', *self.common, '--new', '--bot', 'p.task', 'hello')
        self.assertIsNone(self.fire('p.task', 'x', extra=['--if', 'exit 1', '--dir', str(self.path)]))
        self.assertEqual(len(self.turns('p.task')), 1)
        yes = self.fire('p.task', 'x', extra=['--if', 'test -d .', '--dir', str(self.path)])
        self.assertEqual(yes['last']['outcome'], 'sent', yes)

    def test_a_commit_trigger_sends_only_for_a_new_commit(self):
        self.agent('run', *self.common, '--new', '--bot', 'p.task', 'hello')
        repo = self.path / 'repo'
        repo.mkdir()
        git = ['git', '-C', str(repo), '-c', 'user.name=t', '-c', 'user.email=t@example.com']
        subprocess.run([*git, 'init', '-q'], check=True)
        subprocess.run([*git, 'commit', '-q', '--allow-empty', '-m', 'one'], check=True)
        extra = ['--commit', str(repo)]
        when = f'commit {repo}'
        first = self.fire('p.task', 'Look at it.', when=when, extra=extra, generation='g')
        self.assertEqual(first['last']['outcome'], 'sent', first)
        self.settle('p.task')
        sha = subprocess.run([*git, 'rev-parse', 'HEAD'], check=True, capture_output=True, text=True).stdout.strip()
        self.assertRegex(self.turns('p.task')[-1]['prompt_preview'], '^' + LINE.format('p.task', re.escape(f'{when} at {sha[:12]}')))
        # The same HEAD again (a checkout, a reflog write) is not news.
        again = self.fire('p.task', 'Look at it.', when=when, extra=extra, generation='g')
        self.assertEqual(again['sent'], 1)
        subprocess.run([*git, 'commit', '-q', '--allow-empty', '-m', 'two'], check=True)
        news = self.fire('p.task', 'Look at it.', when=when, extra=extra, generation='g')
        self.assertEqual(news['sent'], 2)
        self.settle('p.task')
        # Run now while the repository is away sends, and keeps the commit last seen.
        moved = self.path / 'away'
        repo.rename(moved)
        (self.home / '.agent/triggers/p.task.fire').write_text(str(int(time.time())))
        asked = self.fire('p.task', 'Look at it.', when=when, extra=extra, generation='g')
        self.assertEqual(asked['last']['outcome'], 'sent', asked)
        self.assertEqual(asked['head'], news['head'])
        self.settle('p.task')
        moved.rename(repo)
        count = len(self.turns('p.task'))
        self.fire('p.task', 'Look at it.', when=when, extra=extra, generation='g')
        self.assertEqual(len(self.turns('p.task')), count, 'its return is no new commit')

    def test_add_from_an_agents_shell_needs_launchd(self):
        # Here there is no launchd: the trigger is refused and nothing is left behind.
        if os.uname().sysname == 'Darwin':
            self.skipTest('would add a real LaunchAgent on macOS')
        self.agent('run', *self.common, '--new', '--bot', 'p.task', 'hello')
        out = self.path / 'added.json'
        self.agent('run', '--store', str(self.store), '--bot', 'p.task',
                   f"shell:HOME='{self.home}' '{APP}' --trigger add --every 30m -- check > '{out}' 2>&1")
        self.assertEqual(json.loads(out.read_text())['error'], 'triggers_unsupported')
        self.assertEqual(list((self.home / 'Library/LaunchAgents').glob('*.plist')), [])

    @unittest.skipUnless(sys.platform == 'darwin' and os.environ.get('AGENT_TEST_LAUNCHD') == '1',
                         'real launchd: macOS with AGENT_TEST_LAUNCHD=1')
    def test_real_launchd_fires_watches_and_removes(self):
        # The plists live under this test's HOME, so nothing loads at the next login.
        name = f'ztest-{os.getpid()}'
        label = f'me.lydakis.agent.trigger.{name}'
        target = f'gui/{os.getuid()}/{label}'
        plist = self.home / f'Library/LaunchAgents/{label}.plist'
        self.addCleanup(lambda: subprocess.run(['/bin/launchctl', 'bootout', target], capture_output=True))
        self.agent('run', *self.common, '--new', '--bot', 'p.task', 'hello')
        env = {**clean_env(), 'HOME': str(self.home), 'AGENT_STORE': str(self.store)}

        def trigger(*args, ok=True):
            result = subprocess.run([str(APP), '--trigger', *args], env=env, capture_output=True, text=True,
                                    timeout=60, cwd=self.path)
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
        trigger('add', '--bot', 'p.task', '--name', name, '--in', '1m', '--', 'launchd says hi')
        self.assertTrue(loaded())
        self.assertTrue(until(lambda: len(self.turns('p.task')) == 2, 150), 'launchd did not fire it')
        self.assertTrue(until(lambda: not loaded() and not plist.exists(), 30), 'it did not end itself')
        self.assertEqual(json.loads(trigger('ls').stdout)['triggers'], [])
        # A file trigger: a write to the file fires it; the same add again is that trigger.
        watched = self.path / 'notes.md'
        trigger('add', '--bot', 'p.task', '--name', name, '--file', str(watched), '--', 'notes changed')
        self.assertTrue(json.loads(trigger('add', '--bot', 'p.task', '--name', name, '--file', str(watched),
                                           '--', 'notes changed').stdout)['duplicate'])
        other = trigger('add', '--bot', 'p.task', '--name', name, '--every', '1h', '--', 'b', ok=False)
        self.assertEqual(json.loads(other.stderr)['field'], 'when')
        watched.write_text('synthetic\n')
        self.assertTrue(until(lambda: len(self.turns('p.task')) == 3, 60), 'launchd did not fire on the write')
        self.settle('p.task')
        # Fire by name: launchd runs it now.
        self.assertEqual(json.loads(trigger('fire', name).stdout), {'name': name, 'fired': True})
        self.assertTrue(until(lambda: len(self.turns('p.task')) == 4, 60), 'fire did not run it')
        self.assertTrue(self.turns('p.task')[-1]['prompt_preview'].startswith(f'[trigger {name} · '))
        # A plist launchd no longer has, as after a failed reload: rm still removes it.
        subprocess.run(['/bin/launchctl', 'bootout', target], check=True, capture_output=True)
        trigger('rm', name)
        self.assertFalse(plist.exists())
        # A job loaded without its plist, as after an end cut short: rm reaches it by name.
        trigger('add', '--bot', 'p.task', '--name', name, '--every', '30m', '--', 'c')
        plist.unlink()
        trigger('rm', name)
        self.assertFalse(loaded())
        # Nothing left: bootout's not-loaded answer reads as that.
        self.assertEqual(json.loads(trigger('rm', name, ok=False).stderr)['error'], 'trigger_not_found')


if __name__ == '__main__':
    unittest.main()
