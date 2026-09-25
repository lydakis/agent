"""The agent command: daemon startup, run/follow/ls, peers, and socket rendezvous."""
import fcntl
import json
import re
import http.server
import concurrent.futures
import threading
import os
from pathlib import Path
import socket
import sqlite3
import subprocess
import tempfile
import time
import unittest

from bench.runtime_client import Client, serve_args
from bench.socket_client import Connection, SocketClient
from bench.targets import clean_env
from tests.test_runtime import ModelFixture


@unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'set AGENT_TEST_RUNTIME=1 after a Rust release build')
class SocketAndCliTests(ModelFixture):
    """The daemon over a Unix socket, driven through the agent command."""

    def setUp(self):
        super().setUp()
        self.store = self.path / 'state.sqlite'
        self.socket = self.path / 'state.sqlite.sock'
        self.base = [str(self.binary)]
        self.common = ['--store', str(self.store), '--provider', f'openai=responses,{self.url}',
                       '--model', 'openai/synthetic-model', '--tools', 'echo,shell']
        # Continuing a bot: what it runs and may call is its own, not a flag.
        self.again = self.common[:4]
        self.addCleanup(self.shutdown)

    def shutdown(self):
        if self.socket.exists():
            subprocess.run([*self.base, 'shutdown', '--store', str(self.store)], env=clean_env(),
                           capture_output=True, timeout=35)

    def agent(self, *args, check=True, timeout=30, stdin=None):
        result = subprocess.run([*self.base, *args], env=clean_env(), capture_output=True, text=True,
                                timeout=timeout, cwd=self.path, input=stdin)
        if check:
            self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
        return result

    def test_run_starts_a_daemon_streams_the_turn_and_resumes_the_bot(self):
        missing = self.agent('run', *self.again, '--bot', 'Bob', 'hello', check=False)
        self.assertEqual(missing.returncode, 1)
        self.assertIn('bot_not_found', missing.stderr)
        first = self.agent('run', *self.common, '--new', '--bot', 'Bob', '--pretty', 'hello')
        self.assertIn('reply:hello', first.stdout)
        self.assertIn('turn 1 (new bot) in ' + str(self.path.resolve()), first.stderr)
        self.assertIn('completed', first.stderr)
        self.assertTrue(self.socket.exists())
        second = self.agent('run', '--store', str(self.store), '--bot', 'Bob', 'tool:again')
        events = [json.loads(line) for line in second.stdout.splitlines()]
        kinds = [e.get('event') for e in events]
        self.assertEqual(kinds[0], 'accepted')
        self.assertIn('tool_started', kinds)
        self.assertIn('tool_completed', kinds)
        self.assertEqual(kinds[-1], 'turn_finished')
        self.assertTrue(all(e.get('turn') == 2 for e in events if e.get('turn') is not None))
        self.assertTrue(all('cursor' in e for e in events if e.get('durable') is not False))
        listing = self.agent('ls', '--store', str(self.store), '--pretty').stdout
        self.assertIn('Bob', listing)
        self.assertIn('openai/synthetic-model', listing)
        duplicate = self.agent('run', *self.common, '--new', '--bot', 'Bob', 'hello', check=False)
        self.assertEqual(duplicate.returncode, 1)
        self.assertIn('bot_exists', duplicate.stderr)
        # A turn runs where it is invoked, not where the bot was created.
        elsewhere = self.path / 'elsewhere'
        elsewhere.mkdir()
        moved = subprocess.run([*self.base, 'run', '--store', str(self.store), '--bot', 'Bob',
                                'shell:printf here > marker'], env=clean_env(), capture_output=True,
                               text=True, timeout=30, cwd=elsewhere)
        self.assertEqual(moved.returncode, 0, moved.stderr)
        self.assertEqual((elsewhere / 'marker').read_text(), 'here')
        self.assertFalse((self.path / 'marker').exists())
        accepted = next(json.loads(l) for l in moved.stdout.splitlines() if json.loads(l).get('event') == 'accepted')
        self.assertEqual(accepted['data']['workspace'], str(elsewhere.resolve()))
        self.assertEqual(accepted['data']['model'], 'openai/synthetic-model')
        failing = self.agent('run', '--store', str(self.store), '--bot', 'Bob', 'truncate', check=False)
        self.assertEqual(failing.returncode, 1)
        last = json.loads(failing.stdout.splitlines()[-1])
        self.assertEqual((last['event'], last['data']['error']), ('turn_finished', 'missing_completion'))
        pretty = self.agent('run', '--store', str(self.store), '--bot', 'Bob', '--pretty', 'truncate', check=False)
        self.assertIn('missing_completion', pretty.stderr)
        self.agent('shutdown', '--store', str(self.store))
        self.assertFalse(self.socket.exists())

    def test_shutdown_returns_once_the_daemon_has_exited(self):
        handle = json.loads(self.agent('run', *self.common, '--new', '--bot', 'Bob', '--detach', 'wait').stdout)
        self.model.requests.get(timeout=3)
        self.agent('shutdown', '--store', str(self.store))
        # The active turn's record is committed and the store is released:
        # a caller may copy or reopen it now.
        self.assertFalse(self.socket.exists())
        with open(f'{self.store}.owner-lock', 'r+') as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        with sqlite3.connect(self.store) as db:
            status, = db.execute('SELECT status FROM turns WHERE id=?', (handle['turn'],)).fetchone()
        self.assertNotIn(status, ('queued', 'ready', 'running'))

    def test_stats_and_wait_any_from_the_cli(self):
        self.agent('run', *self.common, '--new', '--bot', 'Bob', 'p0')
        stats = json.loads(self.agent('stats', '--store', str(self.store)).stdout)
        self.assertEqual(stats['active_turns'], 0)
        self.assertIn('store', stats)
        slow = json.loads(self.agent('run', '--store', str(self.store), '--model', 'openai/synthetic-model',
                                     '--new', '--bot', 'Slow', '--detach', 'wait').stdout)['handle']
        quick = json.loads(self.agent('run', '--store', str(self.store), '--model', 'openai/synthetic-model',
                                      '--new', '--bot', 'Quick', '--detach', 'hi').stdout)['handle']
        first = json.loads(self.agent('wait', '--store', str(self.store), '--any', '--timeout-ms', '5000', slow, quick).stdout)
        self.assertEqual(first['pending'], [slow])
        self.assertEqual(first['results'][quick]['text'], 'reply:hi')
        timed = self.agent('wait', '--store', str(self.store), '--any', '--timeout-ms', '0', slow, check=False)
        self.assertEqual(timed.returncode, 1)
        self.assertEqual(json.loads(timed.stdout), {'pending': [slow], 'results': {slow: {'pending': True}}})
        self.assertEqual(timed.stderr, '')
        done = self.agent('wait', '--store', str(self.store), '--timeout-ms=0', quick)
        self.assertEqual(json.loads(done.stdout)['results'][quick]['text'], 'reply:hi')
        mixed = self.agent('wait', '--store', str(self.store), '--any', '--timeout-ms=0', slow, quick)
        self.assertEqual(json.loads(mixed.stdout)['pending'], [slow])
        self.agent('interrupt', '--store', str(self.store), '--bot', 'Slow')
        failed = self.agent('wait', '--store', str(self.store), '--any', slow, check=False)
        self.assertEqual(failed.returncode, 1)

    def test_run_delivery_modes_from_the_cli(self):
        self.agent('run', *self.common, '--new', '--bot', 'Bob', 'p0')
        busy = json.loads(self.agent('run', '--store', str(self.store), '--bot', 'Bob', '--detach', 'slow').stdout)
        self.assertEqual(busy['status'], 'running')
        refused = self.agent('run', '--store', str(self.store), '--bot', 'Bob', '--detach', 'never', check=False)
        self.assertEqual(refused.returncode, 1)
        self.assertIn('bot_busy', refused.stderr + refused.stdout)
        # The refusal gives flags to copy, not a description of them, and the
        # fork it offers works while the turn runs.
        for flags in (f"--delivery steer --turn {busy['turn']}", '--delivery queue'):
            self.assertIn(flags, refused.stderr + refused.stdout)
        fork = re.search(r'fork (--source Bob --checkpoint \d+) --bot NEW', refused.stderr + refused.stdout)
        self.assertIsNotNone(fork, refused.stderr + refused.stdout)
        self.agent('fork', '--store', str(self.store), *fork.group(1).split(), '--bot', 'Side')
        side = self.agent('run', '--store', str(self.store), '--bot', 'Side', 'aside')
        self.assertEqual(side.returncode, 0)
        queued = json.loads(self.agent('run', '--store', str(self.store), '--bot', 'Bob', '--detach',
                                       '--delivery', 'queue', 'second').stdout)
        self.assertEqual(queued['status'], 'queued')
        steered = self.agent('run', '--store', str(self.store), '--bot', 'Bob', '--delivery=steer', '--pretty', 'late')
        self.assertRegex(steered.stderr, r'steered into turn|completed')
        done = json.loads(self.agent('wait', '--store', str(self.store), queued['handle']).stdout)
        self.assertEqual(done['results'][queued['handle']]['text'], 'reply:second')
        busy = json.loads(self.agent('run', '--store', str(self.store), '--bot', 'Bob', '--detach', 'slow').stdout)
        strict = json.loads(self.agent('run', '--store', str(self.store), '--bot', 'Bob', '--detach',
                                       '--delivery', 'steer', '--turn', str(busy['turn']), 'now').stdout)
        self.assertEqual(strict['status'], 'queued')
        stale = self.agent('run', '--store', str(self.store), '--bot', 'Bob', '--detach',
                           '--delivery', 'steer', '--turn', str(busy['turn'] + 50), 'never', check=False)
        self.assertEqual(stale.returncode, 1)
        self.assertIn('stale_turn', stale.stderr + stale.stdout)
        self.agent('wait', '--store', str(self.store), busy['handle'])
        usage = self.agent('run', '--store', str(self.store), '--bot', 'Bob', '--delivery', 'later', 'x', check=False)
        self.assertEqual(usage.returncode, 1)
        self.assertIn('invalid_delivery', usage.stderr + usage.stdout)
        # The environment supplies a per-user default; the flag still wins.
        env = dict(clean_env(), AGENT_DELIVERY='queue')
        slow = json.loads(subprocess.run([*self.base, 'run', '--store', str(self.store), '--bot', 'Bob', '--detach', 'slow'],
                                         env=env, capture_output=True, text=True, cwd=self.path).stdout)
        queued = json.loads(subprocess.run([*self.base, 'run', '--store', str(self.store), '--bot', 'Bob', '--detach', 'from-env'],
                                           env=env, capture_output=True, text=True, cwd=self.path).stdout)
        self.assertEqual((slow['status'], queued['status']), ('running', 'queued'))
        overridden = subprocess.run([*self.base, 'run', '--store', str(self.store), '--bot', 'Bob', '--detach',
                                     '--delivery', 'reject', 'flag-wins'], env=env, capture_output=True, text=True, cwd=self.path)
        self.assertEqual(overridden.returncode, 1)
        self.assertIn('bot_busy', overridden.stderr + overridden.stdout)
        self.agent('wait', '--store', str(self.store), queued['handle'])

    def test_tools_are_chosen_per_bot_and_enforced(self):
        self.agent('run', *self.common, '--new', '--bot', 'Bob', 'p0')
        # --tools names a new bot's tools; an existing bot keeps its own.
        kept = self.agent('run', '--store', str(self.store), '--bot', 'Bob', '--tools', 'echo', 'hi', check=False)
        self.assertEqual(kept.returncode, 2)
        self.assertIn('keeps its own', kept.stderr)
        only = json.loads(self.agent('run', '--store', str(self.store), '--model', 'openai/synthetic-model',
                                     '--tools', 'echo', '--new', '--bot', 'Only', '--detach', 'shell:true').stdout)
        self.agent('wait', '--store', str(self.store), only['handle'])
        listed = {b['name']: b['tools'] for b in json.loads(self.agent('ls', '--store', str(self.store)).stdout)}
        self.assertEqual((listed['Bob'], listed['Only']), (['echo', 'shell'], ['echo']))
        # The model asked for shell; the bot may not call it, and the result says so.
        bad = self.agent('run', '--store', str(self.store), '--model', 'openai/synthetic-model',
                         '--tools', 'nothing', '--new', '--bot', 'Odd', '--detach', 'hi', check=False)
        self.assertEqual(bad.returncode, 1)
        self.assertIn('unsupported_tool_set', bad.stderr + bad.stdout)

    def test_a_new_bot_needs_a_model_from_the_client(self):
        without = self.agent('run', '--store', str(self.store), '--provider', f'openai=responses,{self.url}',
                             '--tools', 'echo', '--new', '--bot', 'Nobody', 'hello', check=False)
        self.assertEqual(without.returncode, 2, without.stderr)
        self.assertIn('AGENT_MODEL', without.stderr)
        env = dict(clean_env(), AGENT_MODEL='openai/synthetic-model')
        with_env = subprocess.run([*self.base, 'run', '--store', str(self.store), '--provider',
                                   f'openai=responses,{self.url}', '--tools', 'echo', '--new', '--bot', 'Env',
                                   '--detach', 'hello'], env=env, capture_output=True, text=True, cwd=self.path)
        self.assertEqual(with_env.returncode, 0, with_env.stderr)
        listed = json.loads(self.agent('ls', '--store', str(self.store)).stdout)
        self.assertEqual([b['model'] for b in listed if b['name'] == 'Env'], ['synthetic-model'])

    def test_new_bots_get_the_cli_compaction_text_unless_declined(self):
        env = dict(clean_env(), AGENT_MODEL='openai/synthetic-model')
        for bot, extra in (('Default', ()), ('Declined', ('--no-compaction',)), ('Own', ('--compaction-instructions', 'Keep it short.'))):
            run = subprocess.run([*self.base, 'run', '--store', str(self.store), '--provider',
                                  f'openai=responses,{self.url}', '--tools', 'echo', '--new', '--bot', bot, *extra,
                                  '--detach', 'hello'], env=env, capture_output=True, text=True, cwd=self.path)
            self.assertEqual(run.returncode, 0, run.stderr)
        with sqlite3.connect(self.store) as db:
            rows = dict(db.execute('SELECT name, compaction_instructions FROM bots'))
        self.assertTrue(rows['Default'].startswith('You are summarizing'))
        self.assertIsNone(rows['Declined'])
        self.assertEqual(rows['Own'], 'Keep it short.')

    def test_run_refuses_a_stale_bot_identity(self):
        env = dict(clean_env(), AGENT_MODEL='openai/synthetic-model')
        created = subprocess.run([*self.base, 'run', '--store', str(self.store), '--provider',
                                  f'openai=responses,{self.url}', '--tools', 'echo', '--new', '--bot', 'Ident',
                                  '--detach', 'hello'], env=env, capture_output=True, text=True, cwd=self.path)
        self.assertEqual(created.returncode, 0, created.stderr)
        identity = json.loads(created.stdout)['bot_id']
        stale = self.agent('run', '--store', str(self.store), '--bot', 'Ident', '--bot-id', str(identity + 1),
                           '--detach', 'again', check=False)
        self.assertEqual(stale.returncode, 1, stale.stdout)
        self.assertIn('bot_not_found', stale.stderr + stale.stdout)
        exact = self.agent('run', '--store', str(self.store), '--bot', 'Ident', '--bot-id', str(identity),
                           '--detach', 'again')
        self.assertEqual(json.loads(exact.stdout)['bot_id'], identity)
        self.assertEqual(self.agent('run', '--bot-id', 'x', 'hello', check=False).returncode, 2)

    def test_peer_creation_inherits_a_model_but_continuation_keeps_its_own(self):
        self.agent('run', *self.common, '--provider', f'peer=responses,{self.url}',
                   '--new', '--bot', 'Bob', 'hello')
        self.agent('run', '--store', str(self.store), '--model', 'peer/synthetic-model',
                   '--new', '--bot', 'Alice', 'hello')
        for flags, bot, provider in (
            ('--new --bot Inherited', 'Inherited', 'peer'),
            ('--bot Bob', 'Bob', 'openai'),
            ('--bot Inherited --model openai/synthetic-model', 'Inherited', 'openai'),
        ):
            with self.subTest(flags=flags):
                self.agent('run', '--store', str(self.store), '--bot', 'Alice',
                           f'shell:"$AGENT_BIN" run --detach {flags} -- hello')
                turns = json.loads(self.agent('turns', '--store', str(self.store), '--bot', bot).stdout)
                turn = turns[-1]
                self.agent('wait', '--store', str(self.store), f'turn:{bot}/{turn["turn"]}')
                self.assertEqual(turn['model'], f'{provider}/synthetic-model')
        # A one-turn override does not replace the bot's durable choice.
        listed = json.loads(self.agent('ls', '--store', str(self.store)).stdout)
        providers = {b['name']: b['provider'] for b in listed}
        self.assertEqual((providers['Bob'], providers['Inherited']), ('openai', 'peer'))

    def test_attach_refuses_a_running_daemon_with_a_different_configuration(self):
        self.agent('run', *self.common, '--new', '--bot', 'Bob', 'p0')
        def attempt(*flags):
            result = self.agent('run', '--store', str(self.store), *flags, '--bot', 'Bob', '--detach', 'hi', check=False)
            if result.returncode == 0:
                # Let the detached turn finish, so the next attempt finds Bob idle
                # on a slow host too rather than answering bot_busy.
                self.agent('wait', '--store', str(self.store), json.loads(result.stdout)['handle'])
            return result
        # Nothing stated, or the same thing stated differently, attaches.
        self.assertEqual(attempt().returncode, 0)
        self.assertEqual(attempt('--provider', f'openai=responses,{self.url}').returncode, 0)
        # A stated value the daemon does not serve fails before any submission.
        for flags, named in ((('--provider', 'openai=responses,http://127.0.0.1:1/v1'), '--provider openai'),
                             (('--provider', 'other=responses,http://127.0.0.1:1/v1'), '--provider other: not registered'),
                             (('--provider', f'openai=responses-ws,{self.url}'), 'over websocket but daemon has'),
                             (('--max-processes', '3'), '--max-processes'),
                             (('--max-detached', '2'), '--max-detached'),
                             (('--max-pending', '5'), '--max-pending'),
                             (('--note-turns', '5'), '--note-turns'),
                             (('--retain-turns', '2'), '--retain-turns')):
            refused = attempt(*flags)
            self.assertEqual(refused.returncode, 1, refused.stdout + refused.stderr)
            self.assertIn('daemon_configuration_mismatch', refused.stderr)
            self.assertIn(named, refused.stderr)
        # The daemon has no model of its own: --model belongs to run alone.
        stats = self.agent('stats', '--store', str(self.store), '--model', 'openai/other', check=False)
        self.assertEqual(stats.returncode, 2)
        self.assertIn('does not accept --model', stats.stderr)
        self.assertEqual(attempt('--model', 'openai/synthetic-model').returncode, 0)
        self.assertEqual(json.loads(self.agent('turns', '--store', str(self.store), '--bot', 'Bob').stdout)[0]['status'],
                         'completed')

    def test_normalized_daemon_limits_match_on_startup_and_attach(self):
        flags = ['--idle-exit', '0', '--context-bytes', '512', '--context-items', '1', '--stall-timeout', '30',
                 '--keep-warm', '0', '--cache-ttl', '1h']
        self.agent('run', *self.common, *flags, '--new', '--bot', 'Bob', 'hi')
        self.agent('run', *self.again, *flags, '--bot', 'Bob', 'again')
        self.agent('run', *self.again, '--context-bytes', '1024', '--context-items', '2',
                   '--bot', 'Bob', 'effective')
        refused = self.agent('stats', '--store', str(self.store), '--idle-exit', '1', check=False)
        self.assertIn('daemon_configuration_mismatch', refused.stderr)
        refused = self.agent('stats', '--store', str(self.store), '--stall-timeout', '120', check=False)
        self.assertIn('--stall-timeout: requested 120 but daemon has 30', refused.stderr)
        refused = self.agent('stats', '--store', str(self.store), '--keep-warm', '240', check=False)
        self.assertIn('--keep-warm: requested 240 but daemon has 0', refused.stderr)
        refused = self.agent('stats', '--store', str(self.store), '--cache-ttl', '5m', check=False)
        self.assertIn('--cache-ttl: requested 5m but daemon has 1h', refused.stderr)

    def test_help_and_invalid_flags_do_not_start_a_daemon(self):
        for args in [('--help',), ('-h',), ('help', 'run')]+[(c, '--help') for c in
                ('run', 'follow', 'fork', 'interrupt', 'ls', 'turns', 'result', 'wait', 'rm', 'prune', 'stats', 'shutdown', 'serve')]:
            with self.subTest(args=args):
                result = self.agent(*args)
                self.assertIn('Usage:', result.stdout)
                self.assertEqual(result.stderr, '')
        invalid = [
            ('follow', '--all', '--bot', 'Bob'),
            ('stats', '--any'), ('ls', 'ignored'), ('ls', '-x'),
            ('wait', '--all', 'proc:1'),
            ('run', '--bot', 'Bob', '--reasoning', 'low', 'hi'),
            ('run', '--new', '--instructions', 'one', '--instructions-file', 'missing', 'hi'),
            ('wait', '--any=true', 'proc:1'),
            ('run', '--bot', 'Bob', '--bot', 'Alice', 'hi'),
            ('run', '--max-output-tokens', '0', 'hi'),
            ('run', '--stall-timeout', '0', 'hi'),
            ('run', '--stall-timeout', '86401', 'hi'),
            ('run', '--keep-warm', '300', 'hi'),
            ('run', '--cache-ttl', '2h', 'hi'),
            ('serve', '--context-items', '0'),
            ('follow', '--after=-1', '--all'),
        ]
        for args in invalid:
            with self.subTest(args=args):
                result = self.agent(args[0], '--store', str(self.store), *args[1:], check=False)
                self.assertEqual(result.returncode, 2, result.stderr)
                self.assertEqual(result.stdout, '')
        self.assertFalse(self.store.exists())
        self.assertFalse(self.socket.exists())

    def test_equals_delimiter_and_json_output_conventions(self):
        run = self.agent('run', *self.common, '--new', '--bot=Bob', '--', '--help')
        self.assertIn('turn_finished', run.stdout)
        # Help after -- is literal prompt text, and = keeps flag-like values literal.
        self.assertEqual(self.model.requests.get(timeout=2)['input'][-1]['content'][0]['text'], '--help')
        first = self.agent('fork', '--store='+str(self.store), '--source=Bob', '--bot=Branch')
        self.assertEqual(len(first.stdout.splitlines()), 1)
        pretty = self.agent('fork', '--store='+str(self.store), '--source=Bob', '--bot=Pretty', '--pretty')
        self.assertGreater(len(pretty.stdout.splitlines()), 1)
        self.assertEqual(json.loads(first.stdout)['head'], json.loads(pretty.stdout)['head'])
        detached = self.agent('run', '--store='+str(self.store), '--bot=Bob', '--detach', '--pretty', 'hi')
        self.assertGreater(len(detached.stdout.splitlines()), 1)
        self.agent('wait', '--store='+str(self.store), json.loads(detached.stdout)['handle'])
        # The file option uses the same value syntax, and the daemon never
        # sees it: the client resolves a new bot's instructions before asking.
        instructions = self.path / 'instructions.txt'
        instructions.write_text('synthetic instructions')
        self.agent('run', *self.common, '--new', '--bot=Told', '--instructions-file='+str(instructions), 'hi')
        seen = []
        while not seen or seen[-1]['instructions'] != 'synthetic instructions':
            seen.append(self.model.requests.get(timeout=2))
        self.assertEqual(seen[-1]['input'][-1]['content'][0]['text'], 'hi')

    def test_rm_and_prune_bound_a_bot_and_remove_it(self):
        for n in range(3):
            self.agent('run', *(self.common + ['--new'] if n == 0 else self.again), '--bot', 'Bob', f'p{n}')
        pruned = json.loads(self.agent('prune', '--store', str(self.store), '--bot', 'Bob', '--keep-turns', '1').stdout)
        self.assertGreater(pruned['events'], 0)
        self.assertIn('usage', self.agent('prune', '--store', str(self.store), '--bot', 'Bob', check=False).stderr)
        freed = json.loads(self.agent('rm', '--store', str(self.store), '--bot', 'Bob').stdout)
        self.assertEqual(freed['turns'], 3)
        self.assertNotIn('Bob', self.agent('ls', '--store', str(self.store), '--pretty').stdout)
        self.assertIn('bot_not_found', self.agent('rm', '--store', str(self.store), '--bot', 'Bob', check=False).stderr)

    def test_consecutive_submissions_preserve_live_completion_order(self):
        client = SocketClient(self.binary, self.store, self.url, 'echo')
        self.addCleanup(client.close)
        client.request('create', bot='Bob', workspace=str(self.path))
        turns = []
        for n in range(20):
            deadline = time.monotonic() + 5
            while True:
                response = client.request('submit', bot='Bob', request_id=str(n), prompt='fast')
                if 'result' in response:
                    turns.append(response['result']['turn'])
                    break
                self.assertEqual(response['error'], 'bot_busy')
                self.assertLess(time.monotonic(), deadline)
        client.finished(turns[-1])
        page = client.request('events', bot='Bob', after=0, limit=256)['result']
        client.verify_followers({'Bob': page})
        self.assertEqual([e['turn'] for e in page['events'] if e['event'] == 'turn_finished'], turns)

    def test_socket_shutdown_delivers_completion_and_wait_result(self):
        client = SocketClient(self.binary, self.store, self.url, 'echo')
        self.addCleanup(client.close)
        client.request('create', bot='Bob', workspace=str(self.path))
        turn = client.request('submit', bot='Bob', request_id='shutdown', prompt='wait')['result']['turn']
        self.model.requests.get(timeout=3)
        follower = client.followers['Bob']
        handle = f'turn:Bob/{turn}'
        follower.socket.sendall((json.dumps({'id': 'waiter', 'op': 'wait', 'handles': [handle]}) + '\n').encode())
        # This acknowledgment ensures the wait is registered before shutdown
        # arrives on the independent control connection.
        follower.request('resume', bot='Bob')
        client.request('shutdown')
        self.assertEqual(client.finished(turn)['data']['error'], 'cancelled')
        result = follower.receive(lambda e: e.get('id') == 'waiter')['result']
        self.assertEqual(result['results'][handle]['error'], 'cancelled')
        self.assertEqual(result['pending'], [])
        self.assertEqual(client.process.wait(timeout=2), 0)

    def test_retry_of_pruned_turn_exits_and_retained_retry_still_replays(self):
        self.agent('run', *self.common, '--new', '--bot', 'Bob', '--request-id', 'old', 'first')
        retry = ['run', '--store', str(self.store), '--bot', 'Bob', '--request-id', 'old', 'first']
        self.assertIn('turn_finished', self.agent(*retry, timeout=3).stdout)
        self.agent('run', *self.again, '--bot', 'Bob', '--request-id', 'new', 'second')
        self.agent('prune', '--store', str(self.store), '--bot', 'Bob', '--keep-turns', '1')
        expired = self.agent(*retry, check=False, timeout=3)
        self.assertEqual(expired.returncode, 1)
        self.assertIn('turn_result_pruned', expired.stderr)
        # A gap in earlier history must not reject a retained terminal event.
        kept = self.agent('run', *self.again, '--bot', 'Bob', '--request-id', 'new', 'second', timeout=3)
        self.assertIn('turn_finished', kept.stdout)

    def test_detached_peer_handle_is_collected_by_wait(self):
        common = [*self.common[:-2], '--tools', 'echo,shell,wait']
        nested = '"$AGENT_BIN" run --detach --no-spawn --new --bot Alice -- hello'
        bob = self.agent('run', *common, '--new', '--bot', 'Bob', f'shell:{nested}')
        events = [json.loads(line) for line in bob.stdout.splitlines()]
        node = next(e['data']['node'] for e in events if e['event'] == 'tool_completed')
        with_socket = ['--store', str(self.store)]
        # The shell tool's stdout is the detach JSON, including the turn handle.
        import socket as sockets
        with sockets.socket(sockets.AF_UNIX) as sock:
            sock.connect(str(self.socket))
            reader = sock.makefile('r')
            reader.readline()
            sock.sendall((json.dumps({'id': 1, 'op': 'item', 'bot': 'Bob', 'node': node}) + '\n').encode())
            shell_result = json.loads(json.loads(reader.readline())['result']['output'])
        handle = json.loads(shell_result['stdout'])['handle']
        self.assertTrue(handle.startswith('turn:Alice/'), handle)
        waited = self.agent('run', *with_socket, '--bot', 'Bob', 'wait:' + handle)
        kinds = [json.loads(line)['event'] for line in waited.stdout.splitlines()]
        self.assertIn('turn_waiting', kinds)
        self.assertIn('turn_resumed', kinds)
        self.assertEqual(kinds[-1], 'turn_finished')
        node = next(json.loads(l)['data']['node'] for l in waited.stdout.splitlines()
                    if json.loads(l)['event'] == 'tool_completed')
        with sockets.socket(sockets.AF_UNIX) as sock:
            sock.connect(str(self.socket))
            reader = sock.makefile('r')
            reader.readline()
            sock.sendall((json.dumps({'id': 1, 'op': 'item', 'bot': 'Bob', 'node': node}) + '\n').encode())
            outcome = json.loads(json.loads(reader.readline())['result']['output'])
        self.assertEqual(outcome['results'][handle]['text'], 'reply:hello')
        self.assertEqual(outcome['results'][handle]['status'], 'completed')

    def test_cli_wait_blocks_outside_tools_and_is_refused_inside_them(self):
        common = [*self.common[:-2], '--tools', 'echo,shell,wait']
        detached = self.agent('run', *common, '--detach', '--new', '--bot', 'Alice', 'slow')
        handle = json.loads(detached.stdout)['handle']
        waited = self.agent('wait', '--store', str(self.store), handle)
        result = json.loads(waited.stdout)
        self.assertEqual(result['pending'], [])
        self.assertEqual(result['results'][handle]['text'], 'reply:slow')
        pending = self.agent('run', '--store', str(self.store), '--detach', '--bot', 'Alice', 'wait')
        again = json.loads(pending.stdout)['handle']
        timed = self.agent('wait', '--store', str(self.store), '--timeout-ms', '200', again, check=False)
        self.assertEqual(timed.returncode, 1)
        self.assertEqual(json.loads(timed.stdout)['pending'], [again])
        inside = subprocess.run([*self.base, 'wait', '--store', str(self.store), again],
                                env={**clean_env(), 'AGENT_SHELL_CONTEXT': '1'}, capture_output=True, text=True, timeout=5)
        self.assertNotEqual(inside.returncode, 0)
        self.assertIn('blocking_tool_client', inside.stderr)
        self.agent('interrupt', '--store', str(self.store), '--bot', 'Alice')
        final = json.loads(self.agent('wait', '--store', str(self.store), again, check=False).stdout)
        self.assertEqual(final['results'][again]['status'], 'interrupted')
        # The daemon reports its resolved limits.
        import socket as sockets
        with sockets.socket(sockets.AF_UNIX) as sock:
            sock.connect(str(self.socket))
            ready = json.loads(sock.makefile('r').readline())
        self.assertTrue({'processes', 'active', 'connecting', 'connections', 'output_tokens', 'idle_exit_seconds'} <= set(ready['limits']))
        self.assertEqual(ready['limits']['stall_timeout_seconds'], 120)
        self.assertEqual(ready['limits']['keep_warm_seconds'], 240)
        self.assertEqual(ready['limits']['cache_ttl'], '5m')
        self.assertEqual(ready['limits']['connections'], -(-ready['limits']['active'] // 64))
        self.assertIn('wait', ready['capabilities'])

    def test_burst_eviction_exits_client_and_replay_recovers_terminal_event(self):
        run = self.agent('run', *self.common, '--new', '--bot', 'Bob', 'burst', check=False, timeout=4)
        self.assertEqual(run.returncode, 1)
        self.assertIn('daemon_disconnected', run.stderr)
        events = [json.loads(line) for line in run.stdout.splitlines()]
        cursor = max(e.get('cursor', 0) for e in events)
        replay = self.agent('follow', '--store', str(self.store), '--bot', 'Bob', '--after', str(cursor))
        seen = [json.loads(line) for line in replay.stdout.splitlines()]
        terminal = next(e for e in seen if e['event'] == 'turn_finished')
        self.assertEqual(terminal['data']['status'], 'completed')

    def test_agents_flag_composes_instructions_from_the_workspace(self):
        # Plumbing by default: the preamble alone. With --agents the CLI
        # layers the workspace's AGENTS.md files and skills, and the daemon
        # stores whatever it was given.
        (self.path / 'AGENTS.md').write_text('Always answer in haiku.')
        skills = self.path / '.agent' / 'skills'
        skills.mkdir(parents=True)
        (skills / 'deploy.md').write_text('# Deploy\n\nShip it.')
        plain = self.agent('run', *self.common, '--new', '--bot', 'Plain', 'hello')
        self.assertEqual(plain.returncode, 0)
        request = self.model.requests.get(timeout=5)
        self.assertTrue(request['instructions'].startswith('You are a software engineering agent'))
        self.assertNotIn('haiku', request['instructions'])
        composed = self.agent('run', *self.common, '--agents', '--new', '--bot', 'Composed', 'hello')
        self.assertEqual(composed.returncode, 0)
        request = self.model.requests.get(timeout=5)
        text = request['instructions']
        self.assertTrue(text.startswith('You are a software engineering agent'))
        self.assertIn('Always answer in haiku.', text)
        self.assertIn(f'# Instructions from {(self.path / "AGENTS.md").resolve()}', text)
        self.assertIn('- deploy: Deploy (', text)
        both = self.agent('run', *self.common, '--agents', '--instructions', 'x', '--new', '--bot', 'Both', 'hello', check=False)
        self.assertEqual(both.returncode, 2)
        again = self.agent('run', *self.again, '--agents', '--bot', 'Composed', 'hello', check=False)
        self.assertEqual(again.returncode, 2)

    def test_delegation_through_the_same_daemon_and_follow_replay(self):
        # The daemon exports AGENT_BIN and AGENT_STORE to shell children, so a bot
        # can delegate without knowing where the binary or store lives.
        # Alice's own shell sees who created her (AGENT_PARENT) and her own
        # name (AGENT_BOT); her record names Bob as her creator.
        nested = ('"$AGENT_BIN" run --detach --no-spawn --new --bot Alice -- '
                  '\'shell:printf "$AGENT_PARENT/$AGENT_PARENT_ID/$AGENT_BOT" > lineage\'')

        bob = self.agent('run', *self.common, '--new', '--bot', 'Bob', '--pretty', f'shell:{nested}')
        # Bob's shell tool ran the client, which created Alice on the same daemon.
        self.assertIn('echo:', bob.stdout)
        self.assertIn('turn', bob.stdout)
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            state = json.loads(self.agent('ls', '--store', str(self.store)).stdout)
            if all(b['status'] == 'completed' for b in state):
                break
            time.sleep(.02)
        listing = json.loads(self.agent('ls', '--store', str(self.store)).stdout)
        self.assertEqual({b['name'] for b in listing}, {'Alice', 'Bob'})
        self.assertTrue(all(b['status'] == 'completed' for b in listing))
        self.assertEqual({b['name']: b['created_by'] for b in listing}, {'Alice': 'Bob', 'Bob': None})
        by_name = {b['name']: b for b in listing}
        self.assertEqual(by_name['Alice']['created_by_id'], by_name['Bob']['id'])
        self.assertEqual((self.path / 'lineage').read_text(), f"Bob/{by_name['Bob']['id']}/Alice")
        replay = self.agent('follow', '--store', str(self.store), '--bot', 'Alice')
        events = [json.loads(line) for line in replay.stdout.splitlines()]
        self.assertEqual([e['event'] for e in events][:2], ['created', 'accepted'])
        self.assertEqual(events[-1]['event'], 'follow_live')
        self.assertTrue(all(e['cursor'] < f['cursor'] for e, f in zip(events[:-2], events[1:-1])))
        # A follower attached while a turn runs replays, then sees live deltas and the end.
        live = subprocess.Popen([*self.base, 'run', '--store', str(self.store), '--bot', 'Alice', 'wait'],
                                env=clean_env(), stdout=subprocess.PIPE, text=True)
        # Earlier delegation requests are still queued. Wait for this specific
        # turn before attaching, so the assertion actually exercises live follow.
        while True:
            request = self.model.requests.get(timeout=5)
            if request['input'][-1].get('content') == [{'type': 'input_text', 'text': 'wait'}]:
                break
        follower = subprocess.Popen([*self.base, 'follow', '--store', str(self.store), '--bot', 'Alice',
                                     '--after', str(events[-3]['cursor'])], env=clean_env(),
                                    stdout=subprocess.PIPE, text=True)
        out, _ = follower.communicate(timeout=15)
        self.assertEqual(live.wait(timeout=15), 0)
        seen = [json.loads(line) for line in out.splitlines()]
        kinds = [e['event'] for e in seen]
        self.assertEqual(kinds[0], 'turn_finished')  # replayed tail from the requested cursor
        self.assertIn('follow_live', kinds)
        self.assertIn('text_delta', kinds[kinds.index('follow_live'):])
        self.assertEqual(kinds[-1], 'turn_finished')
        self.assertEqual(seen[-1]['turn'], json.loads(live.stdout.read().splitlines()[-1])['turn'])
        live.stdout.close()
        # The stored parent identity remains pinned even after the name is reused.
        control = Connection(self.socket)
        self.addCleanup(control.close)
        self.assertIn('result', control.request('delete', bot='Bob'))
        self.agent('run', *self.common, '--new', '--bot', 'Bob', 'replacement')
        replacement = control.request('resume', bot='Bob')['result']
        self.assertNotEqual(replacement['id'], by_name['Bob']['id'])
        route = ('"$AGENT_BIN" run --detach --bot "$AGENT_PARENT" '
                 '--bot-id "$AGENT_PARENT_ID" -- should-not-deliver > route.out 2> route.err; '
                 'printf "%s" "$?" > route.status')
        self.agent('run', *self.again, '--bot', 'Alice', f'shell:{route}')
        self.assertEqual((self.path / 'route.status').read_text(), '1')
        self.assertIn('bot_not_found', (self.path / 'route.err').read_text())
        self.assertEqual(control.request('resume', bot='Bob')['result']['head'], replacement['head'])

    def test_creator_identity_is_required_and_survives_daemon_restart(self):
        self.agent('run', *self.common, '--new', '--bot', 'Creator',
                   'shell:printf "%s" "$AGENT_BOT_ID" > own-id')
        creator = json.loads(self.agent('ls', '--store', str(self.store)).stdout)[0]
        self.assertEqual((self.path / 'own-id').read_text(), str(creator['id']))
        # A surviving shell retains this environment even across daemon replacement.
        shell_env = dict(clean_env(), AGENT_BOT='Creator', AGENT_BOT_ID=str(creator['id']))
        self.shutdown()
        self.agent('run', *self.again, '--bot', 'Creator', 'after restart')
        control = Connection(self.socket)
        self.addCleanup(control.close)
        self.assertIn('result', control.request('delete', bot='Creator'))
        self.agent('run', *self.common, '--new', '--bot', 'Creator', 'replacement')
        replacement = control.request('resume', bot='Creator')['result']
        for operation in ('create', 'fork'):
            args = (['run', *self.common, '--new', '--bot', 'Child', 'hello']
                    if operation == 'create' else
                    ['fork', '--store', str(self.store), '--source', 'Creator', '--bot', 'Child'])
            for identity in (str(creator['id']), None, str(replacement['id'])):
                env = dict(shell_env)
                if identity is None:
                    env.pop('AGENT_BOT_ID')
                else:
                    env['AGENT_BOT_ID'] = identity
                result = subprocess.run([*self.base, *args], env=env, cwd=self.path,
                                        capture_output=True, text=True, timeout=15)
                if identity == str(replacement['id']):
                    self.assertEqual(result.returncode, 0, result.stderr)
                    child = control.request('resume', bot='Child')['result']
                    self.assertEqual(child['created_by_id'], replacement['id'])
                    self.assertIn('result', control.request('delete', bot='Child'))
                else:
                    self.assertNotEqual(result.returncode, 0)
                    self.assertNotIn('result', control.request('resume', bot='Child'))
        self.assertEqual(control.request('resume', bot='Creator')['result']['head'], replacement['head'])

    def test_large_shell_output_is_previewed_and_retained_as_an_artifact(self):
        run = self.agent('run', *self.common, '--new', '--bot', 'Bob',
                         'shell:head -c 1048576 /dev/zero | tr "\\0" y')
        events = [json.loads(line) for line in run.stdout.splitlines()]
        completed = next(e for e in events if e['event'] == 'tool_completed')
        self.assertEqual(completed['data']['artifacts'], ['stdout'])
        import socket as sockets
        with sockets.socket(sockets.AF_UNIX) as sock:
            sock.connect(str(self.socket))
            reader = sock.makefile('r')
            self.assertEqual(json.loads(reader.readline())['event'], 'ready')
            sock.sendall((json.dumps({'id': 1, 'op': 'artifact', 'bot': 'Bob', 'turn': completed['turn'],
                                      'call_id': completed['data']['call_id']}) + '\n').encode())
            self.assertEqual(json.loads(reader.readline())['error'], 'response_size_limit')
            offset, parts = 0, []
            while True:
                sock.sendall((json.dumps({'id': 3, 'op': 'artifact', 'bot': 'Bob', 'turn': completed['turn'],
                                         'call_id': completed['data']['call_id'], 'stream': 'stdout',
                                         'offset': offset, 'limit': 65536}) + '\n').encode())
                page = json.loads(reader.readline())['result']
                parts.append(page['text'])
                offset = page['next_offset']
                if page['done']:
                    break
            self.assertEqual(''.join(parts), 'y' * 1048576)
            sock.sendall((json.dumps({'id': 2, 'op': 'item', 'bot': 'Bob', 'node': completed['data']['node']}) + '\n').encode())
            preview = json.loads(json.loads(reader.readline())['result']['output'])
            self.assertIn('bytes omitted', preview['stdout'])


@unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'requires built Rust runtime')
class CliTests(ModelFixture):
    @staticmethod
    def stop_process(process):
        if process.poll() is None:
            process.kill()
        process.wait(timeout=3)
        for pipe in (process.stdin, process.stdout, process.stderr):
            if pipe is not None:
                pipe.close()

    def test_cli_follow_keeps_the_terminal_event_when_a_turn_finishes_during_attach(self):
        for status, expected_code in [('completed', 0), ('failed', 1)]:
            with self.subTest(status=status), tempfile.TemporaryDirectory(dir='/tmp') as directory:
                path = Path(directory)/'daemon.sock'
                errors = []
                terminal = dict(cursor=3, bot='Bob', turn=8, event='turn_finished', data=dict(status=status))
                replay = [dict(cursor=1, bot='Bob', turn=7, event='turn_finished', data=dict(status='completed')),
                          dict(cursor=2, bot='Bob', turn=8, event='accepted', data=dict(request_id='current'))]
                with socket.socket(socket.AF_UNIX) as listener:
                    listener.bind(str(path))
                    listener.listen(1)
                    listener.settimeout(3)
                    def serve():
                        try:
                            with listener.accept()[0] as peer:
                                peer.settimeout(3)
                                with peer.makefile('rb') as reader:
                                    def send(*events):
                                        peer.sendall(b''.join((json.dumps(event)+'\n').encode() for event in events))
                                    send(dict(event='ready', protocol=3))
                                    request = json.loads(reader.readline())
                                    if request['op'] == 'resume':
                                        # Snapshot while active, then finish before subscription.
                                        send(dict(id=request['id'], result=dict(name='Bob', running_turn=8, status='running')))
                                        request = json.loads(reader.readline())
                                        self.assertEqual(request['op'], 'follow')
                                        send(dict(id=request['id'], result=dict(following='Bob', after=0)),
                                             *replay, terminal, dict(event='follow_live', bot='Bob', cursor=3))
                                    else:
                                        # Previous ordering: completion arrives during the idle RPC.
                                        self.assertEqual(request['op'], 'follow')
                                        send(dict(id=request['id'], result=dict(following='Bob', after=0)),
                                             *replay, dict(event='follow_live', bot='Bob', cursor=2))
                                        request = json.loads(reader.readline())
                                        self.assertEqual(request['op'], 'resume')
                                        send(terminal, dict(id=request['id'], result=dict(name='Bob', running_turn=None, status=status)))
                        except Exception as error:
                            errors.append(error)
                    worker = threading.Thread(target=serve)
                    worker.start()
                    try:
                        result = subprocess.run([str(self.binary), 'follow', '--bot', 'Bob', '--store', str(Path(directory)/'state.db'),
                                                 '--socket', str(path)], env=clean_env(), capture_output=True, text=True, timeout=5)
                    finally:
                        worker.join(timeout=4)
                    self.assertFalse(worker.is_alive())
                    self.assertEqual(errors, [])
                    events = [json.loads(line) for line in result.stdout.splitlines()]
                    self.assertEqual([event for event in events if 'cursor' in event and event['event'] != 'follow_live'],
                                     [*replay, terminal])
                    self.assertEqual(result.returncode, expected_code, result.stderr)

    def test_concurrent_cli_startup_waits_for_store_recovery(self):
        # Recreate a crash with the maximum admitted number of unfinished turns.
        with tempfile.TemporaryDirectory(dir='/tmp') as directory:
            path = Path(directory)/'state.db'
            bootstrap = Client(self.binary, path, self.url)
            bootstrap.close()
            with sqlite3.connect(path) as db:
                for n in range(1, 1025):
                    db.execute("INSERT INTO bots(name,id,head,workspace,status,running_turn,provider,family,model,instructions,reasoning)"
                               " VALUES (?,?,NULL,?,'running',?,'openai','responses','synthetic-model','',NULL)",
                               (f'old-{n}', n, directory, n))
                    db.execute("INSERT INTO turns(id,bot,request_id,prompt,status,workspace,model) VALUES (?,?,'old','check','running',?,'openai/synthetic-model')",
                               (n, f'old-{n}', directory))
                db.execute("UPDATE turn_sequence SET last_id=1024 WHERE singleton=1")
                db.execute("UPDATE bot_sequence SET last_id=1024 WHERE singleton=1")
            barrier = threading.Barrier(16)
            common = ['--store', str(path), '--provider', 'openai=responses,'+self.url,
                      '--model', 'openai/synthetic-model', '--tools', 'echo', '--workspace', directory]
            def run(n):
                barrier.wait(timeout=5)
                return subprocess.run([str(self.binary), 'run', '--detach', '--new', '--bot', f'new-{n}',
                                       *common, 'hello'], env=clean_env(), capture_output=True, text=True, timeout=12)
            try:
                with concurrent.futures.ThreadPoolExecutor(max_workers=16) as pool:
                    results = list(pool.map(run, range(16)))
                self.assertEqual([r.stderr for r in results if r.returncode], [])
                with sqlite3.connect(path) as db:
                    self.assertEqual(db.execute("SELECT count(*) FROM turns WHERE status='interrupted'").fetchone()[0], 1024)
                    self.assertEqual(db.execute("SELECT count(*) FROM turns WHERE bot LIKE 'new-%'").fetchone()[0], 16)
            finally:
                subprocess.run([str(self.binary), 'shutdown', '--store', str(path)], env=clean_env(),
                               capture_output=True, timeout=3)

    def test_invalid_daemon_configuration_fails_without_waiting_for_a_winner(self):
        start = time.monotonic()
        result = subprocess.run([str(self.binary), 'run', '--detach', '--new', '--bot', 'bad',
                                 '--store', str(self.path/'bad.db'), '--provider', 'fixture=responses,not-a-url',
                                 '--model', 'fixture/model', 'hello'], env=clean_env(),
                                capture_output=True, text=True, timeout=3)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('daemon_start_failed', result.stderr)
        self.assertIn('invalid_provider_url', result.stderr)
        self.assertLess(time.monotonic()-start, 2)

    def test_startup_ownership_conflict_has_a_bounded_wait(self):
        # A stdio owner never publishes the socket the CLI is waiting for.
        with tempfile.TemporaryDirectory(dir='/tmp') as directory:
            path = Path(directory)/'state.db'
            owner = Client(self.binary, path, self.url)
            try:
                start = time.monotonic()
                result = subprocess.run([str(self.binary), 'run', '--detach', '--new', '--bot', 'waiting',
                                         '--store', str(path), '--provider', 'openai=responses,'+self.url,
                                         '--model', 'openai/synthetic-model', '--tools', 'echo', 'hello'],
                                        env=clean_env(), capture_output=True, text=True, timeout=12)
                elapsed = time.monotonic()-start
                self.assertNotEqual(result.returncode, 0)
                self.assertIn('daemon_start_timeout', result.stderr)
                self.assertGreaterEqual(elapsed, 9)
                self.assertLess(elapsed, 12)
                self.assertEqual(owner.request('resume', bot='waiting')['error'], 'bot_not_found')
            finally:
                owner.close()

    def test_cli_handshake_deadline_covers_silent_and_trickling_listeners(self):
        def check(command, trickle):
            with tempfile.TemporaryDirectory(dir='/tmp') as directory:
                path = Path(directory)/'daemon.sock'
                store = Path(directory)/'state.db'
                stop = threading.Event()
                with socket.socket(socket.AF_UNIX) as listener:
                    listener.bind(str(path))
                    listener.listen(1)
                    listener.settimeout(3)
                    args = [str(self.binary), command, '--store', str(store), '--socket', str(path)]
                    if command == 'run':
                        args += ['--detach', '--provider', 'openai=responses,'+self.url,
                                 '--model', 'openai/synthetic-model', 'hello']
                    start = time.monotonic()
                    process = subprocess.Popen(args, env=clean_env(), stdout=subprocess.PIPE, stderr=subprocess.PIPE)
                    try:
                        with listener.accept()[0] as peer:
                            def send_partial_line():
                                while not stop.wait(.1):
                                    try:
                                        peer.sendall(b' ')
                                    except OSError:
                                        return
                            writer = threading.Thread(target=send_partial_line) if trickle else None
                            if writer:
                                writer.start()
                            try:
                                _, stderr = process.communicate(timeout=12)
                                self.assertNotEqual(process.returncode, 0)
                                self.assertIn(b'daemon_start_timeout', stderr)
                                self.assertGreaterEqual(time.monotonic()-start, 9)
                                self.assertLess(time.monotonic()-start, 12)
                                self.assertFalse(store.exists())
                                self.assertFalse(Path(str(store)+'.log').exists())
                            finally:
                                stop.set()
                                if writer:
                                    writer.join(timeout=1)
                    finally:
                        self.stop_process(process)
        # Cover automatic startup and a command that only connects. Trickled
        # bytes must not reset the overall deadline like an idle timeout would.
        with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
            list(pool.map(lambda args: check(*args), [('run', False), ('ls', True)]))

    def test_explicit_store_overrides_inherited_socket(self):
        client = SocketClient(self.binary, self.path/'first.db', self.url, 'echo')
        self.addCleanup(client.close)
        client.control.request('create', bot='first')
        env = {**clean_env(), 'AGENT_SOCKET':str(client.socket_path)}
        base = [str(self.binary), 'ls', '--store', str(self.path/'other.db')]
        result = subprocess.run(base, env=env, capture_output=True, text=True, timeout=3)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('daemon_unavailable', result.stderr)
        result = subprocess.run([*base, '--socket', str(client.socket_path)], env=env, capture_output=True, text=True, timeout=3)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout)[0]['name'], 'first')

    def test_cli_starts_and_reconnects_with_a_deep_store_path(self):
        store = self.path/('snapshot-'*18)/'state.db'
        env = clean_env()
        common = ['--store', str(store)]
        def cli(*args):
            return subprocess.run([str(self.binary), *args, *common], env=env,
                                  capture_output=True, text=True, timeout=12)
        self.addCleanup(lambda: cli('shutdown'))
        result = cli('run', '--detach', '--new', '--bot', 'deep', '--provider',
                     'openai=responses,'+self.url, '--tools', 'echo',
                     '--model', 'openai/synthetic-model', 'hello')
        self.assertEqual(result.returncode, 0, result.stderr)
        result = cli('ls')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout)[0]['name'], 'deep')
        self.assertTrue(store.exists())
        self.assertEqual(cli('shutdown').returncode, 0)

    def test_listing_pages_metadata_and_cli_lists_every_bot(self):
        client = SocketClient(self.binary, self.path/'state.db', self.url, 'echo,shell')
        self.addCleanup(client.close)
        expected = [f'bot-{n:03}' for n in range(70)]
        for bot in expected:
            self.assertIn('result', client.control.request('create', bot=bot,
                workspace=str(self.path), instructions='x'*65536))
        names, after = [], None
        while True:
            page = client.control.request('bots', after=after, limit=7)['result']
            self.assertLessEqual(len(page['bots']), 7)
            self.assertTrue(all('instructions' not in b for b in page['bots']))
            names.extend(b['name'] for b in page['bots'])
            after = page['next_after']
            if after is None:
                break
        self.assertEqual(names, expected)
        listing = subprocess.run([str(self.binary), 'ls', '--store', str(self.path/'state.db'), '--socket', str(client.socket_path)],
                                 env=clean_env(), capture_output=True, text=True, timeout=5)
        self.assertEqual(listing.returncode, 0, listing.stderr)
        self.assertEqual([b['name'] for b in json.loads(listing.stdout)], expected)
        self.assertEqual(client.control.request('bots', limit=0)['error'], 'invalid_bot_page')

    def test_peer_submissions_detach_and_blocking_tool_clients_fail_fast(self):
        client = SocketClient(self.binary, self.path/'peers.db', self.url, 'echo,shell')
        self.addCleanup(client.close)
        turns = []
        for n in range(16):
            client.request('create', bot=f'caller-{n}', workspace=str(self.path))
            command = f'"$AGENT_BIN" run --detach --new --bot peer-{n} -- "shell:sleep .3; touch leaf-{n}"'
            turns.append(client.request('submit', bot=f'caller-{n}', request_id='r', prompt='shell:'+command)['result']['turn'])
        for turn in turns:
            self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        deadline = time.monotonic()+5
        while len(list(self.path.glob('leaf-*')))<16 and time.monotonic()<deadline:
            time.sleep(.02)
        self.assertEqual(len(list(self.path.glob('leaf-*'))), 16)
        env = {**clean_env(), 'AGENT_SHELL_CONTEXT':'1'}
        for args in [('run','--new','--bot','blocked','hello'), ('follow','--bot','peer-0')]:
            result = subprocess.run([str(self.binary), *args, '--store', str(self.path/'peers.db'), '--socket', str(client.socket_path)],
                                    env=env, capture_output=True, text=True, timeout=2)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn('blocking_tool_client', result.stderr)
        self.assertEqual(client.control.request('resume', bot='blocked')['error'], 'bot_not_found')
