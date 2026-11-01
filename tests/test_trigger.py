"""A trigger's fire against a real daemon: what launchd or systemd runs, sent to the bot it was made for."""
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

from bench.targets import clean_env
from bench.socket_client import Connection
from tests.test_runtime import ModelFixture

ROOT = Path(__file__).resolve().parent.parent
SONNY = next((p for p in (ROOT / '.local/target/debug/sonny', ROOT / '.local/target/release/sonny') if p.exists()), None)
# What a fire puts before the message: `[trigger NAME · YYYY-MM-DD HH:MM · why]`.
LINE = r'\[trigger {} · \d{{4}}-\d\d-\d\d \d\d:\d\d · {}\]\n'
SCHEDULER = 'me.lydakis.agent.scheduler.'


def home_tag(home):
    """HOME's tag in its scheduler job's label, as Sonny computes it: FNV-1a 64 of the path, folded to 32 bits."""
    data = str(home).encode().rstrip(b'/') or b'/'
    value = 0xcbf29ce484222325
    for byte in data:
        value = ((value ^ byte) * 0x100000001b3) & 0xffffffffffffffff
    return f'{(value ^ (value >> 32)) & 0xffffffff:08x}'


def job(home, name):
    """Where a trigger's definition is under `home`: one file Sonny's scheduler reads."""
    return home / '.agent/triggers/jobs' / f'{name}.json'


def definition(args):
    """A definition running `args`, as Sonny writes one: all a fire reads of it."""
    return json.dumps({'program': args})


def program(text):
    """The arguments a definition runs."""
    return json.loads(text)['program']


def jobs(home):
    """Every trigger definition under `home`."""
    return sorted(job(home, 'x').parent.glob('*.json'))


@unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'set AGENT_TEST_RUNTIME=1 after a Rust release build')
@unittest.skipUnless(SONNY, 'build Sonny first: cargo build -p sonny')
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
        return next(b['bot_id'] for b in json.loads(self.agent('ls', '--store', str(self.store)).stdout) if b['name'] == bot)

    def store_identity(self):
        # The lineage the daemon announces in `ready`, which `add` keeps.
        if not hasattr(self, '_store_id'):
            self._store_id = json.loads(self.agent('start', '--store', str(self.store)).stdout)['store']['lineage']
        return self._store_id

    def bundle(self):
        # Sonny starts a daemon with the `agent` installed beside it, as Homebrew lays them out.
        bundle = self.path / 'bundle'
        if not bundle.exists():
            (bundle / 'bin').mkdir(parents=True)
            (bundle / 'libexec').mkdir()
            try:
                os.link(SONNY, bundle / 'bin/sonny')
            except OSError:
                shutil.copy2(SONNY, bundle / 'bin/sonny')
            (bundle / 'libexec/agent').symlink_to(self.binary)
        return bundle / 'bin/sonny'

    def fire(self, name, message, target=None, when='every 30m', extra=(), at=None, app=SONNY, socket=None,
             env=None, store_id=None, not_before=None, generation=None, wait=True):
        args, env = self.write_trigger(name, message, target, when, extra, at, app, socket, env, store_id,
                                       not_before, generation)
        if not wait:
            return subprocess.Popen(args, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        result = subprocess.run(args, env=env, capture_output=True, text=True, timeout=60)
        self.assertEqual(result.returncode, 0, result.stderr)
        last = self.home / '.agent/triggers' / f'{name}.json'
        return json.loads(last.read_text()) if last.exists() else None

    def write_trigger(self, name, message, target=None, when='every 30m', extra=(), at=None, app=SONNY, socket=None,
                      env=None, store_id=None, not_before=None, generation=None):
        # What a trigger's plist has launchd run.
        target = target or ['--bot', name, '--bot-id', str(self.bot_id(name))]
        args = [str(app), '--trigger-fire', '--name', name, *target,
                '--generation', generation or str(time.time_ns()), '--when', when, *(['--at', str(at)] if at else []),
                *extra, '--store', str(self.store), *(['--socket', str(socket)] if socket else []),
                '--store-id', store_id or self.store_identity(),
                *(['--not-before', str(not_before)] if not_before else []), '--', message]
        env = {**clean_env(), 'HOME': str(self.home), **(env or {})}
        # A fire records and ends only the trigger its definition still holds.
        path = job(self.home, name)
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(definition(args))
        return args, env

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
        # A working bot is not interrupted: the fire waits behind its work,
        # as anacron runs a missed job once, and one fire of the trigger at
        # a time waits there.
        self.model.release_headers = threading.Event()
        self.model.all_streaming = self.model.release_headers
        self.addCleanup(self.model.release_headers.set)
        self.agent('run', '--store', str(self.store), '--bot', 'p.task', '--detach', 'gate')
        time.sleep(0.5)
        late = self.fire('p.task', 'Check the PR again.', generation='7')
        self.assertEqual((late['last']['outcome'], late['last'].get('late')), ('sent', True), late)
        skipped = self.fire('p.task', 'Check the PR again.', generation='7')
        self.assertEqual(skipped['last']['outcome'], 'skipped', skipped)
        # The waiting fire is found behind the notes the skipped ones left.
        again = self.fire('p.task', 'Check the PR again.', generation='7')
        self.assertEqual(again['last']['outcome'], 'skipped', again)
        self.model.release_headers.set()
        busy = next(t['turn'] for t in self.turns('p.task') if t['prompt_preview'] == 'gate')
        self.agent('wait', '--store', str(self.store), f"turn:p.task/{late['last']['turn']}")
        turns = self.turns('p.task')
        waited = next(t for t in turns if t['turn'] == late['last']['turn'])
        self.assertGreater(waited['turn'], busy)
        self.assertEqual(waited['status'], 'completed')
        # The skipped fires start no turn: the bot is told in a model call.
        for told in turns[-2:]:
            self.assertIn(told['status'], ('attached', 'steered'))
            self.assertRegex(told['prompt_preview'],
                             r'^\[trigger p\.task · .* · every 30m not sent to p\.task: bot_busy')
        self.assertEqual(len(turns), 6)

    def test_a_recurring_fire_waits_for_its_first_interval(self):
        self.agent('run', *self.common, '--new', '--bot', 'p.task', 'hello')
        self.assertIsNone(self.fire('p.task', 'x', not_before=int(time.time()) + 1800))
        self.assertEqual(len(self.turns('p.task')), 1)

    def test_a_stale_shell_cannot_add_a_trigger_for_a_replacement_bot(self):
        self.agent('run', *self.common, '--new', '--bot', 'p.task', 'hello')
        old = self.bot_id('p.task')
        self.agent('rm', '--store', str(self.store), '--bot', 'p.task')
        self.agent('run', *self.common, '--new', '--bot', 'p.task', 'again')
        result = subprocess.run([str(SONNY), 'trigger', 'add', '--every', '30m', '--', 'x'],
            env={**clean_env(), 'HOME': str(self.home), 'AGENT_STORE': str(self.store),
                 'AGENT_BOT': 'p.task', 'AGENT_BOT_ID': str(old)},
            capture_output=True, text=True, timeout=30)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(json.loads(result.stderr)['error'], 'bot_not_found')
        self.assertEqual(jobs(self.home), [])

    def test_a_one_off_waits_for_a_working_bot_instead_of_skipping(self):
        self.agent('run', *self.common, '--new', '--bot', 'p.task', 'hello')
        self.model.release_headers = threading.Event()
        self.model.all_streaming = self.model.release_headers
        self.addCleanup(self.model.release_headers.set)
        self.agent('run', '--store', str(self.store), '--bot', 'p.task', '--detach', 'gate')
        time.sleep(0.5)
        fired = self.fire('p.task', 'Look again.', at=int(time.time()))
        # Delivered, it ends, keeping its row until its turn is known to
        # have ended well.
        self.assertEqual(fired['last']['outcome'], 'sent', fired)
        self.assertFalse(job(self.home, 'p.task').exists())
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
        # launchd gives no provider keys: the daemon gets them from the login shell's profile.
        shell = short / 'login-shell'
        shell.write_text(f"#!/bin/sh\nexport AGENT_PROVIDER='openai=responses,{self.url}'\nexec /bin/sh -c \"$4\"\n")
        shell.chmod(0o755)
        sent = self.fire('p.task', 'Morning check.', target=target, app=self.bundle(), socket=socket,
                         env={'SHELL': str(shell)})
        self.assertEqual(sent['last']['outcome'], 'sent', sent)
        self.assertTrue(socket.exists())
        turns = json.loads(self.agent('turns', '--store', str(self.store), '--socket', str(socket), '--bot', 'p.task').stdout)
        self.assertTrue(turns[-1]['prompt_preview'].endswith(']\nMorning check.'))

    def test_a_fire_never_reaches_another_stores_daemon(self):
        self.agent('run', *self.common, '--new', '--bot', 'p.task', 'hello')
        other = self.fire('p.task', 'x', store_id='0' * 16)
        self.assertEqual(other['last']['outcome'], 'failed', other)
        self.assertIn('store_mismatch', other['last']['detail'])
        self.assertEqual(len(self.turns('p.task')), 1)

    def test_a_fire_finds_its_store_after_the_file_was_replaced(self):
        self.agent('run', *self.common, '--new', '--bot', 'p.task', 'hello')
        target = ['--bot', 'p.task', '--bot-id', str(self.bot_id('p.task'))]
        self.store_identity()
        self.agent('shutdown', '--store', str(self.store))
        # A copy put back in its place: the same store in a new file.
        copy = self.path / 'copy.sqlite'
        shutil.copy2(self.store, copy)
        os.replace(copy, self.store)
        self.agent('start', *self.common[:4])
        sent = self.fire('p.task', 'Still here.', target=target)
        self.assertEqual(sent['last']['outcome'], 'sent', sent)

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
        # The definition now holds another generation of the same message: the old job's fire leaves its result alone.
        path = job(self.home, 'p.task')
        args = program(path.read_text())
        path.write_text(definition(['g2' if a == 'g1' else a for a in args]))
        last = self.home / '.agent/triggers/p.task.json'
        last.unlink()
        result = subprocess.run(args, env={**clean_env(), 'HOME': str(self.home)}, capture_output=True, text=True,
                                timeout=60)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(first['generation'], 'g1')
        self.assertFalse(last.exists())

    def test_an_ask_sends_the_request_it_sent_before_also_for_the_trigger_that_replaced_it(self):
        self.agent('run', *self.common, '--new', '--bot', 'p.lead', 'hello')
        self.agent('run', *self.common, '--new', '--bot', 'p.task', 'hello')
        lead = self.bot_id('p.lead')
        taking = self.home / '.agent/triggers/p.task.taking'
        taking.mkdir(parents=True)
        (taking / '100.1.1').write_text('')
        sent = self.fire('p.task', 'first', generation='g1', extra=['--reply-to', 'p.lead', '--reply-to-id', str(lead)])
        self.assertEqual((sent['last']['outcome'], sent['last']['reply']['outcome']), ('sent', 'sent'), sent)
        self.settle('p.lead')
        turn = self.turns('p.task')[-1]
        answers = len(self.turns('p.lead'))
        connection = Connection(str(self.store) + '.sock')
        try:
            prompt = connection.request('prompts', bot='p.task', turn=turn['turn'], bytes=65536)['result']['prompts'][0]['text']
        finally:
            connection.close()
        # The fire was cut short once its turn and its answer went, and
        # another definition took the name: the ask it left sends what went,
        # and its answer where it went, under the ids they went under, which
        # the daemon takes once; not the new message under the new generation.
        submit = {'bot': 'p.task', 'bot_id': self.bot_id('p.task'), 'request_id': turn['request_id'],
                  'prompt': prompt, 'delivery': 'queue', 'origin': 'trigger', 'budget_tokens': None}
        kept = {'store_id': self.store_identity(), 'generation': 'g1', 'reply_to': ['p.lead', lead], 'submit': submit}
        (taking / '100.1.1').write_text(json.dumps(kept))
        # Cut short before it settled: the row has no result of it.
        row = self.home / '.agent/triggers/p.task.json'
        row.write_text(json.dumps({**json.loads(row.read_text()), 'last': None, 'sent': 0}))
        # Its `--if` said go once already: one that would say no now is not asked.
        again = self.fire('p.task', 'second', generation='g2', extra=['--if', 'exit 1', '--dir', str(self.path)])
        self.assertEqual((again['last']['outcome'], again['last']['turn']), ('sent', turn['turn']), again)
        self.assertEqual(again['last']['reply']['outcome'], 'sent', again)
        self.assertEqual(len(self.turns('p.task')), 2)
        self.assertEqual(len(self.turns('p.lead')), answers)
        self.assertFalse((taking / '100.1.1').exists())
        # Cut short once it had settled: its send is not counted again.
        (taking / '100.1.1').write_text(json.dumps(kept))
        settled = self.fire('p.task', 'second', generation='g2')
        self.assertEqual((settled['sent'], settled['last']['turn']), (1, turn['turn']), settled)
        self.assertFalse((taking / '100.1.1').exists())
        # What went to another store is that store's: this one sends its own.
        (taking / '100.1.2').write_text(json.dumps({**kept, 'store_id': 'another-store'}))
        own = self.fire('p.task', 'third', generation='g3')
        self.assertEqual(own['last']['outcome'], 'sent', own)
        self.settle('p.task')
        self.assertEqual(len(self.turns('p.task')), 3)
        self.assertRegex(self.turns('p.task')[-1]['prompt_preview'], LINE.format('p.task', 'fired') + 'third$')
        self.assertEqual(len(self.turns('p.lead')), answers)

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
        # With Sonny's tools, as an agent Sonny opens has them.
        connection = Connection(str(self.store) + '.sock')
        declared = lambda: [t['name'] for t in connection.request('inspect', bot='p.review')['result'].get('declared', [])]
        try:
            self.assertIn('memory', declared())
            # Gated and sandboxed as `sonny --new` makes one (#404).
            made = connection.request('inspect', bot='p.review')['result']
            self.assertEqual(made['settings']['sandbox'], 'workspace-write', made['settings'])
            self.assertEqual([g['tag'] for g in made['gates']], ['auto'], made['gates'])
            self.settle('p.review')
            # One a trigger made before it had them takes them at its next fire.
            connection.request('configure', bot='p.review', bot_id=review, declare=[])
            self.assertEqual(declared(), [])
            again = self.fire('p.review', 'Review the newest commit.', target=target, extra=extra,
                              app=self.bundle(), env=env, generation='g1')
            self.assertIn('memory', declared())
        finally:
            connection.close()
        self.assertEqual((again['last']['outcome'], again['sent']), ('sent', 2), again)
        self.assertNotIn('started', again['last'])
        self.settle('p.review')
        self.assertEqual(len(self.turns('p.review')), 2)
        # A fire cut short after the daemon made the agent, before its id was
        # kept, asks again under the trigger's request id and gets that agent.
        last = self.home / '.agent/triggers/p.review.json'
        kept = json.loads(last.read_text())
        del kept['started_id']
        last.write_text(json.dumps(kept))
        made = self.fire('p.review', 'Review the newest commit.', target=target, extra=extra, app=self.bundle(),
                         env=env, generation='g1')
        self.assertEqual((made['last']['outcome'], made['started_id']), ('sent', review), made)
        self.settle('p.review')
        self.assertEqual(len(self.turns('p.review')), 3)
        # A fire of a new trigger for that name finds an agent it did not start.
        taken = self.fire('p.review', 'x', target=target, extra=extra, app=self.bundle(), env=env, generation='g2')
        self.assertEqual(taken['last']['outcome'], 'failed', taken)
        self.assertIn('bot_exists', taken['last']['detail'])
        self.assertEqual(len(self.turns('p.review')), 3)

    def test_an_agent_made_in_full_access_starts_ungated(self):
        # `[sonny] default_permissions = "full"`: no sandbox and no one
        # approving, so the agent is made with no gate, which `create`
        # takes as no approval fields (it refuses an empty `approve`).
        (self.home / '.agent').mkdir(exist_ok=True)
        (self.home / '.agent/config.toml').write_text('[sonny]\ndefault_permissions = "full"\n')
        self.agent('run', *self.common, '--new', '--bot', 'p.lead', 'hello')
        lead = self.bot_id('p.lead')
        work = self.path / 'work'
        work.mkdir()
        target = ['--start', 'p.free', '--model', 'openai/synthetic-model', '--by', 'p.lead', '--by-id', str(lead)]
        env = {'AGENT_PROVIDER': f'openai=responses,{self.url}'}
        fired = self.fire('p.free', 'Look around.', target=target, extra=['--dir', str(work)], app=self.bundle(),
                          env=env, generation='g1')
        self.assertEqual(fired['last']['outcome'], 'sent', fired)
        connection = Connection(str(self.store) + '.sock')
        try:
            made = connection.request('inspect', bot='p.free')['result']
        finally:
            connection.close()
        self.assertEqual(made['settings']['sandbox'], 'danger-full-access', made['settings'])
        self.assertEqual(made['gates'], [])
        self.settle('p.free')

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
            self.assertEqual(accepted['from'], {'bot': 'p.task', 'turn': task_turn, 'bot_id': self.bot_id('p.task')})
            self.assertNotIn('origin', accepted)
            # The task's own turn names where its answer goes.
            self.assertEqual(self.turns('p.task')[-1]['request_id'].rsplit('.to.', 1)[1], str(lead))
        finally:
            connection.close()

    def test_a_trigger_at_its_run_limit_sends_nothing_more(self):
        self.agent('run', *self.common, '--new', '--bot', 'p.task', 'hello')
        first = self.fire('p.task', 'once', extra=['--runs', '1'], generation='g1')
        self.settle('p.task')
        self.assertEqual(len(self.turns('p.task')), 2)
        # Delivered to its limit, it ends, its row kept until its turn is
        # known to have ended well.
        self.assertEqual(first['last']['outcome'], 'sent', first)
        self.assertFalse(job(self.home, 'p.task').exists())

    def test_a_fire_caps_the_turn_it_starts(self):
        self.agent('run', *self.common, '--new', '--bot', 'p.task', 'hello')
        self.fire('p.task', 'check', extra=['--turn-budget-tokens', '500'])
        self.settle('p.task')
        self.assertEqual([t['budget_tokens'] for t in self.turns('p.task')], [None, 500])

    def test_a_one_off_whose_gate_says_no_ends_saying_so(self):
        self.agent('run', *self.common, '--new', '--bot', 'p.task', 'hello')
        no = self.fire('p.task', 'x', at=int(time.time()), extra=['--if', 'exit 1', '--dir', str(self.path)])
        self.assertEqual(len(self.turns('p.task')), 1)
        self.assertEqual(no['last']['outcome'], 'declined', no)
        self.assertFalse(job(self.home, 'p.task').exists())

    def test_a_gate_that_cannot_run_is_a_failed_fire(self):
        self.agent('run', *self.common, '--new', '--bot', 'p.task', 'hello')
        gone = self.path / 'gone'
        failed = self.fire('p.task', 'x', extra=['--if', 'true', '--dir', str(gone)])
        self.assertEqual(failed['last']['outcome'], 'failed', failed)
        self.assertIn('--if', failed['last']['detail'])
        self.assertEqual(len(self.turns('p.task')), 1)

    def test_a_gate_that_says_no_costs_no_turn(self):
        self.agent('run', *self.common, '--new', '--bot', 'p.task', 'hello')
        # Its row says when it looked and why it said no, with no result yet.
        no = self.fire('p.task', 'x', extra=['--if', 'exit 1', '--dir', str(self.path)])
        self.assertEqual((no['last'], no['checked']['gate'], no['checked']['detail']),
                         (None, 'no', '--if: exit status: 1'), no)
        self.assertEqual(len(self.turns('p.task')), 1)
        yes = self.fire('p.task', 'x', extra=['--if', 'test -d .', '--dir', str(self.path)])
        self.assertEqual((yes['last']['outcome'], yes['checked']['gate']), ('sent', 'go'), yes)

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
        # A move between commits sends nothing, and the next look starts past it.
        # git's dates are whole seconds; a commit the second before a look is older than it.
        time.sleep(1.1)
        subprocess.run([*git, 'checkout', '-q', 'HEAD~1'], check=True)
        back = self.fire('p.task', 'Look at it.', when=when, extra=extra, generation='g')
        self.assertEqual(back['last'], news['last'])
        self.assertEqual(back['head'], sha)
        self.assertIsNotNone(back['seen_at'])
        time.sleep(1.1)
        subprocess.run([*git, 'checkout', '-q', '-'], check=True)
        news = self.fire('p.task', 'Look at it.', when=when, extra=extra, generation='g')
        self.assertEqual(news['last'], back['last'])
        self.assertEqual(len(self.turns('p.task')), 3)
        # Run now while the repository is away sends, and keeps the commit last seen.
        moved = self.path / 'away'
        repo.rename(moved)
        (self.home / '.agent/triggers/p.task.asks').mkdir(parents=True, exist_ok=True)
        (self.home / '.agent/triggers/p.task.asks/1').write_text('')
        asked = self.fire('p.task', 'Look at it.', when=when, extra=extra, generation='g')
        self.assertEqual(asked['last']['outcome'], 'sent', asked)
        self.assertEqual(asked['head'], news['head'])
        self.settle('p.task')
        moved.rename(repo)
        count = len(self.turns('p.task'))
        self.fire('p.task', 'Look at it.', when=when, extra=extra, generation='g')
        self.assertEqual(len(self.turns('p.task')), count, 'its return is no new commit')

    def test_a_commit_its_gate_said_no_to_is_not_news_again(self):
        self.agent('run', *self.common, '--new', '--bot', 'p.task', 'hello')
        repo = self.path / 'repo'
        repo.mkdir()
        git = ['git', '-C', str(repo), '-c', 'user.name=t', '-c', 'user.email=t@example.com']
        subprocess.run([*git, 'init', '-q'], check=True)
        subprocess.run([*git, 'commit', '-q', '--allow-empty', '-m', 'one'], check=True)
        go = self.path / 'go'
        extra = ['--commit', str(repo), '--if', f"test -f '{go}'", '--dir', str(self.path)]
        when = f'commit {repo}'
        self.fire('p.task', 'x', when=when, extra=extra, generation='g')
        subprocess.run([*git, 'commit', '-q', '--allow-empty', '-m', 'two'], check=True)
        declined = self.fire('p.task', 'x', when=when, extra=extra, generation='g')
        sha = subprocess.run([*git, 'rev-parse', 'HEAD'], check=True, capture_output=True, text=True).stdout.strip()
        self.assertEqual(declined['head'], sha)
        # A checkout later wakes it with the gate open: the declined commit is not news.
        go.write_text('')
        time.sleep(1.1)
        subprocess.run([*git, 'checkout', '-q', 'HEAD~1'], check=True)
        self.fire('p.task', 'x', when=when, extra=extra, generation='g')
        self.assertEqual(len(self.turns('p.task')), 1)

    def test_a_file_trigger_whose_path_became_its_own_state_ends_saying_why(self):
        self.agent('run', *self.common, '--new', '--bot', 'p.task', 'hello')
        state = self.home / '.agent/triggers'
        state.mkdir(parents=True)
        # Made for a folder a link pointed elsewhere; the link now points at the triggers' own.
        link = self.path / 'watched'
        link.symlink_to(state)
        ended = self.fire('p.task', 'x', when=f'file {link}', extra=['--file', str(link / 'p.task.json')])
        self.assertEqual(ended['last']['outcome'], 'failed', ended)
        self.assertTrue(ended['last']['detail'].startswith('invalid_file:'), ended)
        self.assertEqual(len(self.turns('p.task')), 1)

    def test_add_from_an_agents_shell_needs_systemd_running(self):
        # No user manager runs for this runtime folder: the trigger is refused and nothing is left behind.
        if sys.platform == 'darwin':
            self.skipTest('would add a real LaunchAgent on macOS')
        self.agent('run', *self.common, '--new', '--bot', 'p.task', 'hello')
        out = self.path / 'added.json'
        runtime = self.path / 'runtime'
        runtime.mkdir()
        self.agent('run', '--store', str(self.store), '--bot', 'p.task',
                   f"shell:HOME='{self.home}' XDG_RUNTIME_DIR='{runtime}' '{SONNY}' trigger add --every 30m -- check"
                   f" > '{out}' 2>&1")
        self.assertEqual(json.loads(out.read_text())['error'], 'triggers_unsupported')
        self.assertEqual(jobs(self.home), [])

    def scheduler(self, env):
        """`sonny scheduler` for this test's HOME, run here as the system's job would run it."""
        process = subprocess.Popen([str(env.get('SONNY', SONNY)), 'scheduler'], env=env, cwd=self.path,
                                   stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)

        def stop():
            if process.poll() is None:
                process.terminate()
            process.communicate(timeout=10)
        self.addCleanup(stop)
        return process

    def until(self, check, seconds):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline and not check():
            time.sleep(0.5)
        return check()

    def test_the_scheduler_fires_and_removes(self):
        # The user manager or launchd gives no provider keys: a daemon a fire starts gets them from the login shell.
        shell = self.path / 'login-shell'
        shell.write_text(f"#!/bin/sh\nexport AGENT_PROVIDER='openai=responses,{self.url}'\nexec /bin/sh -c \"$4\"\n")
        shell.chmod(0o755)
        name = 'ztest'
        self.agent('run', *self.common, '--new', '--bot', 'p.task', 'hello')
        # Sonny with its agent beside it, so a fire can start the daemon it stopped.
        sonny = self.bundle()
        env = {**clean_env(), 'HOME': str(self.home), 'AGENT_STORE': str(self.store), 'SHELL': str(shell),
               'SONNY': str(sonny)}
        # One scheduler for a HOME: of two, one leaves it to the other.
        both = [self.scheduler(env), self.scheduler(env)]
        self.assertTrue(self.until(lambda: any(p.poll() is not None for p in both), 30), 'two schedulers run')
        (left,), (running,) = [p for p in both if p.poll() is not None], [p for p in both if p.poll() is None]
        self.assertEqual(left.returncode, 0)

        def trigger(*args, ok=True):
            result = subprocess.run([str(sonny), 'trigger', *args], env=env, capture_output=True, text=True,
                                    timeout=120, cwd=self.path)
            if ok:
                self.assertEqual(result.returncode, 0, result.stderr)
            return result

        # A one-off: the scheduler fires it at its minute, the bot gets the message, and it ends itself.
        trigger('add', '--bot', 'p.task', '--name', name, '--in', '1m', '--', 'the scheduler says hi')
        self.assertTrue(job(self.home, name).exists())
        self.assertTrue(self.until(lambda: len(self.turns('p.task')) == 2, 150), 'the scheduler did not fire it')
        self.assertTrue(self.until(lambda: not job(self.home, name).exists(), 30), 'it did not end itself')
        ended = json.loads(trigger('ls').stdout)['triggers']
        self.assertEqual([(r['name'], r['ended'], r['last']['outcome']) for r in ended], [(name, True, 'sent')])
        # A file trigger: a write to the file fires it; the same add again is that trigger.
        watched = self.path / 'notes.md'
        trigger('add', '--bot', 'p.task', '--name', name, '--file', str(watched), '--', 'notes changed')
        self.assertTrue(json.loads(trigger('add', '--bot', 'p.task', '--name', name, '--file', str(watched),
                                           '--', 'notes changed').stdout)['duplicate'])
        other = trigger('add', '--bot', 'p.task', '--name', name, '--every', '1h', '--', 'b', ok=False)
        self.assertEqual(json.loads(other.stderr)['field'], 'when')
        watched.write_text('synthetic\n')
        self.assertTrue(self.until(lambda: len(self.turns('p.task')) == 3, 60), 'it did not fire on the write')
        self.settle('p.task')
        # Fire by name, with the daemon stopped: the fire starts it, and it outlives the fire.
        self.agent('shutdown', '--store', str(self.store))
        fired = json.loads(trigger('fire', name).stdout)
        self.assertEqual((fired['name'], fired['fired'], fired['outcome']), (name, True, 'sent'), fired)
        self.assertTrue(self.until(lambda: len(self.turns('p.task')) == 4, 60), 'fire did not run it')
        self.assertEqual(fired['handle'], f"turn:p.task/{self.turns('p.task')[-1]['turn']}")
        self.assertTrue(self.turns('p.task')[-1]['prompt_preview'].startswith(f'[trigger {name} · '))
        # A trigger that recurs leaves the scheduler as it was: nothing for the system to tell anyone about.
        trigger('add', '--bot', 'p.task', '--name', 'p.next', '--every', '30m', '--', 'look')
        trigger('add', '--bot', 'p.task', '--name', 'p.next', '--every', '30m', '--replace', '--', 'look again')
        self.assertIn('look again', job(self.home, 'p.next').read_text())
        trigger('rm', name)
        trigger('rm', 'p.next')
        self.assertEqual(jobs(self.home), [])
        self.assertEqual(json.loads(trigger('rm', name, ok=False).stderr)['error'], 'trigger_not_found')
        self.assertIsNone(running.poll())

    def test_a_scheduler_whose_home_is_gone_lets_its_job_go(self):
        # One user manager for every HOME (#453): a test's HOME deleted with its scheduler still running.
        gone = self.path / 'gone-home'
        gone.mkdir()
        config = self.path / 'config'
        env = {**clean_env(), 'HOME': str(gone), 'AGENT_STORE': str(self.store), 'XDG_CONFIG_HOME': str(config)}
        label = f'{SCHEDULER}{home_tag(gone)}'
        self.assertNotEqual(label, f'{SCHEDULER}{home_tag(self.home)}')
        # Its job, as the system would have it; nothing here loads it.
        job_file = (gone / f'Library/LaunchAgents/{label}.plist' if sys.platform == 'darwin'
                    else config / f'systemd/user/{label}.service')
        job_file.parent.mkdir(parents=True)
        job_file.write_text('# a scheduler job\n')
        self.agent('run', *self.common, '--new', '--bot', 'p.task', 'hello')
        running = self.scheduler(env)
        added = subprocess.run([str(SONNY), 'trigger', 'add', '--bot', 'p.task', '--name', 'ztest-gone',
                                '--every', '30m', '--', 'nobody home'], env=env, capture_output=True, text=True,
                               timeout=120)
        self.assertEqual(added.returncode, 0, added.stderr)
        turns = len(self.turns('p.task'))
        shutil.rmtree(gone)
        running.wait(timeout=90)
        self.assertEqual(running.returncode, 0)
        self.assertFalse(job_file.exists())
        self.assertEqual(len(self.turns('p.task')), turns)

    @unittest.skipUnless((sys.platform == 'darwin' and os.environ.get('AGENT_TEST_LAUNCHD') == '1')
                         or (sys.platform.startswith('linux') and os.environ.get('AGENT_TEST_SYSTEMD') == '1'),
                         'the real system: macOS with AGENT_TEST_LAUNCHD=1, or Linux with a user manager and '
                         'AGENT_TEST_SYSTEMD=1')
    def test_the_system_runs_one_scheduler_job_written_once(self):
        mac = sys.platform == 'darwin'
        label = f'{SCHEDULER}{home_tag(self.home)}'
        real = Path(os.environ.get('XDG_CONFIG_HOME') or Path.home() / '.config')
        runtime = os.environ.get('XDG_RUNTIME_DIR') or f'/run/user/{os.getuid()}'
        # The plist lives under this test's HOME, so nothing loads at the next login; units where the manager reads them.
        job_file = (self.home / f'Library/LaunchAgents/{label}.plist' if mac
                    else real / f'systemd/user/{label}.service')
        env = {**clean_env(), 'HOME': str(self.home), 'AGENT_STORE': str(self.store),
               **({} if mac else {'XDG_CONFIG_HOME': str(real), 'XDG_RUNTIME_DIR': runtime})}

        def running():
            if mac:
                return subprocess.run(['/bin/launchctl', 'print', f'gui/{os.getuid()}/{label}'],
                                      capture_output=True).returncode == 0
            return subprocess.run(['systemctl', '--user', 'is-active', f'{label}.service'], env=env,
                                  capture_output=True, text=True).stdout.strip() == 'active'

        def cleanup():
            if mac:
                subprocess.run(['/bin/launchctl', 'bootout', f'gui/{os.getuid()}/{label}'], capture_output=True)
            else:
                subprocess.run(['systemctl', '--user', 'disable', '--now', f'{label}.service'], env=env,
                               capture_output=True)
            job_file.unlink(missing_ok=True)
            if not mac:
                subprocess.run(['systemctl', '--user', 'daemon-reload'], env=env, capture_output=True)
        self.addCleanup(cleanup)
        self.agent('run', *self.common, '--new', '--bot', 'p.task', 'hello')

        def trigger(*args):
            result = subprocess.run([str(SONNY), 'trigger', *args], env=env, capture_output=True, text=True,
                                    timeout=120, cwd=self.path)
            self.assertEqual(result.returncode, 0, result.stderr)
            return result

        # The first trigger sets the job up; the system runs the scheduler, which fires it.
        trigger('add', '--bot', 'p.task', '--name', 'ztest', '--in', '1m', '--', 'the system says hi')
        self.assertTrue(job_file.exists())
        written = job_file.stat().st_mtime_ns
        self.assertTrue(self.until(running, 30), 'the system did not run the scheduler')
        # Every trigger after, and every replace, leaves the job as it was.
        for message in ('look', 'look again', 'and again'):
            trigger('add', '--bot', 'p.task', '--name', 'p.next', '--every', '30m', '--replace', '--', message)
        self.assertEqual(job_file.stat().st_mtime_ns, written)
        self.assertTrue(self.until(lambda: len(self.turns('p.task')) == 2, 150), 'the scheduler did not fire it')
        trigger('rm', 'p.next')
        self.assertEqual(job_file.stat().st_mtime_ns, written)

if __name__ == '__main__':
    unittest.main()
