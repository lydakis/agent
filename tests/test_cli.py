"""The agent command: daemon startup, run/follow/ls, peers, and socket rendezvous."""
import json
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
        self.addCleanup(self.shutdown)

    def shutdown(self):
        if self.socket.exists():
            subprocess.run([*self.base, 'shutdown', '--store', str(self.store)], env=clean_env(),
                           capture_output=True, timeout=5)
            deadline = time.monotonic() + 5
            while self.socket.exists() and time.monotonic() < deadline:
                time.sleep(.05)

    def agent(self, *args, check=True, timeout=30, stdin=None):
        result = subprocess.run([*self.base, *args], env=clean_env(), capture_output=True, text=True,
                                timeout=timeout, cwd=self.path, input=stdin)
        if check:
            self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
        return result

    def test_run_starts_a_daemon_streams_the_turn_and_resumes_the_bot(self):
        missing = self.agent('run', *self.common, '--bot', 'Bob', 'hello', check=False)
        self.assertEqual(missing.returncode, 1)
        self.assertIn('bot_not_found', missing.stderr)
        first = self.agent('run', *self.common, '--new', '--bot', 'Bob', '--pretty', 'hello')
        self.assertIn('reply:hello', first.stdout)
        self.assertIn('turn 1 (new bot)', first.stderr)
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
        deadline = time.monotonic() + 2
        while self.socket.exists() and time.monotonic() < deadline:
            time.sleep(.01)
        self.assertFalse(self.socket.exists())

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
        self.assertEqual(set(ready['limits']), {'processes', 'active', 'connecting'})
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

    def test_delegation_through_the_same_daemon_and_follow_replay(self):
        # The daemon exports AGENT_BIN and AGENT_STORE to shell children, so a bot
        # can delegate without knowing where the binary or store lives.
        nested = '"$AGENT_BIN" run --detach --no-spawn --new --bot Alice -- hello'

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
                    db.execute("INSERT INTO bots VALUES (?,NULL,?,'running',?,'openai','responses','synthetic-model','',NULL)",
                               (f'old-{n}', directory, n))
                    db.execute("INSERT INTO turns(id,bot,request_id,prompt,status,workspace,model) VALUES (?,?,'old','check','running',?,'openai/synthetic-model')",
                               (n, f'old-{n}', directory))
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
        base = [str(self.binary), 'ls', '--store', str(self.path/'other.db'), '--no-spawn']
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
        self.addCleanup(lambda: cli('shutdown', '--no-spawn'))
        result = cli('run', '--detach', '--new', '--bot', 'deep', '--provider',
                     'openai=responses,'+self.url, '--tools', 'echo',
                     '--model', 'openai/synthetic-model', 'hello')
        self.assertEqual(result.returncode, 0, result.stderr)
        result = cli('ls', '--no-spawn')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout)[0]['name'], 'deep')
        self.assertTrue(store.exists())
        self.assertEqual(cli('shutdown', '--no-spawn').returncode, 0)

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
