"""Daemon sessions: slow readers, replay lifecycle, shutdown, socket and file ownership."""
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


@unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'requires built Rust runtime')
class DaemonTests(ModelFixture):
    @staticmethod
    def stop_process(process):
        if process.poll() is None:
            process.kill()
        process.wait(timeout=3)
        for pipe in (process.stdin, process.stdout, process.stderr):
            if pipe is not None:
                pipe.close()

    def test_file_tools_reject_fifos_and_leave_shutdown_responsive(self):
        calls = []
        for path in ('pipe', 'alias', '/dev/null'):
            for tool, args in [('read', {}), ('edit', dict(old='old', new='new')), ('write', dict(content='data'))]:
                calls.append(dict(type='function_call', name=tool, call_id=f'{path}-{tool}',
                                  arguments=json.dumps(dict(path=path, **args))))
        class FilesModel(http.server.BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass
            def do_POST(self):
                request = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
                output = calls if request['input'][-1].get('type') != 'function_call_output' else [
                    dict(type='message', role='assistant', content=[dict(type='output_text', text='')])]
                event = dict(type='response.completed', response=dict(status='completed', output=output))
                body = ('data: '+json.dumps(event)+'\n\n').encode()
                self.send_response(200)
                self.send_header('Content-Type', 'text/event-stream')
                self.send_header('Content-Length', str(len(body)))
                self.end_headers()
                self.wfile.write(body)
        model = http.server.ThreadingHTTPServer(('127.0.0.1', 0), FilesModel)
        worker = threading.Thread(target=model.serve_forever, daemon=True)
        worker.start()
        with tempfile.TemporaryDirectory(dir='/tmp') as directory:
            root = Path(directory)
            os.mkfifo(root/'pipe')
            (root/'alias').symlink_to('pipe')
            client = None
            try:
                client = Client(self.binary, root/'state.db', f'http://127.0.0.1:{model.server_port}/v1', 'read,write,edit')
                client.request('create', bot='Bob', workspace=str(root))
                turn = client.request('submit', bot='Bob', request_id='fifo', prompt='inspect the files')['result']['turn']
                terminal = client.finished(turn)
                self.assertEqual(terminal['data']['status'], 'completed')
                events = client.request('events', bot='Bob', after=0, limit=256)['result']['events']
                completed = [event for event in events if event['event'] == 'tool_completed']
                self.assertEqual(len(completed), len(calls))
                for event in completed:
                    item = client.request('item', bot='Bob', node=event['data']['node'])['result']
                    self.assertEqual(json.loads(item['output'])['error'], 'file_not_regular')
                self.assertTrue((root/'alias').is_symlink())
                self.assertIn('result', client.request('shutdown'))
                self.assertEqual(client.process.wait(timeout=2), 0)
            finally:
                if client is not None:
                    client.close(kill=True)
                model.shutdown()
                model.server_close()
                worker.join(timeout=2)

    def test_one_socket_follows_every_bot_and_stats_report_the_fleet(self):
        client = SocketClient(self.binary, self.path/'state.db', self.url, 'echo')
        self.addCleanup(client.close)
        for bot in ('Alice', 'Bob'):
            client.request('create', bot=bot, workspace=str(self.path))
        first = client.request('submit', bot='Alice', request_id='a', prompt='hi')['result']['turn']
        client.finished(first)
        # A controller follows `*` from a store-wide cursor: replay covers
        # both bots in cursor order, then live events for any bot arrive.
        everyone = Connection(client.socket_path)
        self.addCleanup(everyone.close)
        self.assertEqual(everyone.request('follow', bot='*', after=0)['result']['following'], '*')
        live = everyone.receive(lambda e: e.get('event') == 'follow_live')
        self.assertEqual([e['bot'] for e in everyone.durable if e['event'] == 'created'], ['Alice', 'Bob'])
        self.assertTrue(any(e['event'] == 'turn_finished' and e['bot'] == 'Alice' for e in everyone.durable))
        self.assertEqual(live['bot'], '*')
        second = client.request('submit', bot='Bob', request_id='b', prompt='hi')['result']['turn']
        finished = everyone.receive(lambda e: e.get('event') == 'turn_finished' and e.get('turn') == second)
        self.assertEqual(finished['bot'], 'Bob')
        # Cursors are store-wide, so a reconnect resumes from the last one seen.
        again = Connection(client.socket_path)
        self.addCleanup(again.close)
        again.request('follow', bot='*', after=finished['cursor'])
        again.receive(lambda e: e.get('event') == 'follow_live')
        self.assertEqual(again.durable, [])
        # Identities created after the replay/live boundary must also arrive.
        client.control.request('create', bot='New')
        created = everyone.receive(lambda e: e.get('event') == 'created' and e.get('bot') == 'New')
        client.control.request('fork', source='Alice', bot='Branch')
        forked = everyone.receive(lambda e: e.get('event') == 'forked' and e.get('bot') == 'Branch')
        for event in (created, forked):
            replay = client.control.request('events', bot=event['bot'], after=0, limit=1)['result']['events']
            self.assertEqual(replay, [event])
        stats = client.request('stats')['result']
        # control, one follower per bot, and the two `*` followers
        self.assertEqual(stats['sessions'], 5)
        self.assertEqual((stats['active_turns'], stats['waiting_turns'], stats['running_processes']), (0, 0, 0))
        provider = stats['providers']['openai']
        loads = stats['transport']['in_flight_by_shard']
        self.assertEqual(loads, [0] * len(loads))
        self.assertEqual(provider['pools'], {'synthetic-model': provider['pools']['synthetic-model']})
        self.assertIsNone(provider['pools']['synthetic-model']['tokens_per_minute'])
        self.assertGreater(stats['store']['jobs'], 0)
        self.assertGreater(stats['store']['bytes'], 0)
        self.assertEqual(stats['handles'], {'waiters': 0, 'retained': 0})

    def test_stats_count_shared_transport_once_across_providers(self):
        self.model.release_headers = threading.Event()
        self.model.all_streaming = threading.Barrier(2)
        self.addCleanup(self.model.release_headers.set)
        client = self.client(extra=('--provider', f'other=responses,{self.url}'))
        client.request('create', bot='Bob', workspace=str(self.path))
        turn = client.request('submit', bot='Bob', request_id='one', prompt='gate')['result']['turn']
        self.model.requests.get(timeout=3)
        stats = client.request('stats')['result']
        self.assertEqual(sum(stats['transport']['in_flight_by_shard']), 1)
        self.assertEqual(stats['sessions'], 1)
        self.assertEqual(stats['providers']['other'], {'pools': {}})
        self.assertEqual(set(stats['providers']['openai']), {'pools'})
        self.assertIn('synthetic-model', stats['providers']['openai']['pools'])
        self.assertEqual(stats['providers']['openai']['pools']['synthetic-model']['reserved_requests'], 1)
        self.model.release_headers.set()
        deadline = time.monotonic() + 3
        while time.monotonic() < deadline:
            stats = client.request('stats')['result']
            if stats['providers']['openai']['pools']['synthetic-model']['reserved_requests'] == 0:
                break
            time.sleep(.01)
        pool = stats['providers']['openai']['pools']['synthetic-model']
        self.assertEqual(pool['reserved_requests'], 0)
        self.assertNotIn('in_flight', pool)
        self.assertEqual(sum(stats['transport']['in_flight_by_shard']), 1)
        self.model.all_streaming.wait(timeout=3)
        client.finished(turn)
        self.assertEqual(sum(client.request('stats')['result']['transport']['in_flight_by_shard']), 0)

    def test_stats_use_the_opened_database_path_after_alias_changes(self):
        path = self.path/'real.sqlite'
        seed = Client(self.binary, path, self.url, 'echo')
        seed.close()
        alias = self.path/'alias.sqlite'
        alias.symlink_to(path)
        client = Client(self.binary, alias, self.url, 'echo')
        self.addCleanup(client.close)
        client.request('create', bot='Bob')
        wal = Path(str(path)+'-wal')
        self.assertGreater(wal.stat().st_size, 0)
        for remove_alias in (False, True):
            if remove_alias:
                alias.unlink()
            stats = client.request('stats')['result']['store']
            self.assertEqual(stats['bytes'], path.stat().st_size)
            self.assertEqual(stats['wal_bytes'], wal.stat().st_size)

    def test_slow_rpc_reader_does_not_delay_other_clients(self):
        client = SocketClient(self.binary, self.path/'state.db', self.url, 'echo')
        self.addCleanup(client.close)
        client.control.request('create', bot='Bob', instructions='x'*65536)
        with socket.socket(socket.AF_UNIX) as slow:
            slow.settimeout(2)
            slow.connect(str(client.socket_path))
            with slow.makefile('rb') as reader:
                self.assertEqual(json.loads(reader.readline())['event'], 'ready')
                slow.sendall(b''.join((json.dumps(dict(id=n, op='resume', bot='Bob'))+'\n').encode()
                                     for n in range(100)))
                time.sleep(.2)
                start = time.monotonic()
                self.assertIn('result', client.control.request('resume', bot='Bob'))
                self.assertLess(time.monotonic()-start, 1)
                # Saturation closes the writer even while the peer keeps its read side open.
                while reader.readline():
                    pass

    def test_unfollow_and_replacement_stop_old_replay(self):
        path = self.path/'state.db'
        bootstrap = Client(self.binary, path, self.url)
        bootstrap.request('create', bot='Bob')
        bootstrap.close()
        with sqlite3.connect(path) as db:
            db.executemany("INSERT INTO events(bot,turn,kind,data) VALUES ('Bob',NULL,'fixture',?)",
                           [(json.dumps({'text':'x'*1024}),)]*4000)
            last = db.execute('SELECT MAX(id) FROM events').fetchone()[0]
        client = SocketClient(self.binary, path, self.url, 'echo')
        self.addCleanup(client.close)
        for replacement in (dict(op='unfollow', bot='Bob'), dict(op='follow', bot='Bob', after=last)):
            with socket.socket(socket.AF_UNIX) as peer:
                peer.settimeout(3); peer.connect(str(client.socket_path))
                with peer.makefile('rb') as reader:
                    json.loads(reader.readline())
                    requests = [dict(id=1, op='follow', bot='Bob', after=0), dict(id=2, **replacement), dict(id=3, op='resume', bot='Bob')]
                    peer.sendall(b''.join((json.dumps(r)+'\n').encode() for r in requests))
                    acknowledged = False
                    while True:
                        event = json.loads(reader.readline())
                        if event.get('id') == 2: acknowledged = True
                        elif acknowledged:
                            self.assertNotEqual(event.get('event'), 'fixture')
                        if event.get('id') == 3: break
                    # Drain any scheduled replay, then establish a second response barrier.
                    time.sleep(.1)
                    peer.sendall(b'{"id":4,"op":"resume","bot":"Bob"}\n')
                    while True:
                        event = json.loads(reader.readline())
                        self.assertNotEqual(event.get('event'), 'fixture')
                        if replacement['op'] == 'unfollow':
                            self.assertNotEqual(event.get('event'), 'follow_live')
                        if event.get('id') == 4: break

    def test_stdio_shutdown_releases_an_active_replay(self):
        path = self.path/'state.db'
        client = Client(self.binary, path, self.url)
        client.request('create', bot='Bob', workspace=str(self.path))
        client.close()
        with sqlite3.connect(path) as db:
            db.executemany("INSERT INTO events(bot,turn,kind,data) VALUES ('Bob',NULL,'fixture',?)",
                           [(json.dumps({'text':'x'*1024}),)]*4000)
        client = Client(self.binary, path, self.url)
        self.addCleanup(lambda: client.close(kill=True))
        client.process.stdin.write('{"id":1,"op":"follow","bot":"Bob"}\n{"id":2,"op":"shutdown"}\n')
        client.process.stdin.flush()
        self.assertIn('result', client.receive(lambda m: m.get('id') == 2))
        self.assertEqual(client.process.wait(timeout=3), 0)
        # The process and its database ownership are both released.
        reopened = Client(self.binary, path, self.url)
        self.addCleanup(reopened.close)
        self.assertIn('result', reopened.request('resume', bot='Bob'))

    def test_socket_collision_preserves_live_owner_and_regular_files(self):
        client = SocketClient(self.binary, self.path/'first.db', self.url, 'echo')
        self.addCleanup(client.close)
        client.control.request('create', bot='first', workspace=str(self.path))
        args = [str(self.binary), *serve_args(self.path/'second.db', self.url), '--socket', str(client.socket_path)]
        second = subprocess.Popen(args, env=clean_env(), stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        self.addCleanup(self.stop_process, second)
        _, error = second.communicate(timeout=3)
        self.assertNotEqual(second.returncode, 0)
        self.assertIn('socket_already_owned', error)
        connection = Connection(client.socket_path)
        self.addCleanup(connection.close)
        self.assertIn('result', connection.request('resume', bot='first'))
        # Unrelated files at a configured socket path must never be deleted.
        with tempfile.TemporaryDirectory(dir='/tmp') as td:
            target = Path(td)/'socket'
            target.write_text('keep')
            result = subprocess.run([*args[:-1], str(target)], env=clean_env(), capture_output=True, timeout=3)
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(target.read_text(), 'keep')
            target.unlink()
            stale = socket.socket(socket.AF_UNIX)
            stale.bind(str(target)); stale.close()
            process = subprocess.Popen([*args[:-1], str(target)], env=clean_env(), stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            self.addCleanup(self.stop_process, process)
            deadline = time.monotonic()+3
            while True:
                try:
                    with socket.socket(socket.AF_UNIX) as probe:
                        probe.connect(str(target))
                    connection = Connection(target)
                    break
                except (ConnectionRefusedError, FileNotFoundError):
                    if time.monotonic()>deadline: raise
                    time.sleep(.01)
            connection.request('shutdown'); connection.close()
            self.assertEqual(process.wait(timeout=3), 0)
            self.assertFalse(target.exists())
