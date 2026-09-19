"""Synthetic lifecycle controller: one daemon, one socket follower per bot.

Controllers live in the benchmark observer, outside the measured target tree.
The target includes the daemon and its tool descendants, not CLI processes.
"""
import json
from pathlib import Path
import queue
import socket
import subprocess
import tempfile
import threading
import time

from .runtime_client import Client, serve_args
from .targets import clean_env


class Connection:
    receive = Client.receive
    finished = Client.finished

    def __init__(self, path):
        self.socket = socket.socket(socket.AF_UNIX)
        self.socket.connect(str(path))
        self.reader = self.socket.makefile('r')
        self.queue, self.saved, self.durable = queue.Queue(), [], []
        self.next_id = 0

        def read():
            try:
                for line in self.reader:
                    event = json.loads(line)
                    if 'cursor' in event and event.get('event') != 'follow_live':
                        self.durable.append(event.copy())
                    if event.get('event') == 'turn_finished':
                        event['_received_at'] = time.monotonic()
                    self.queue.put(event)
            except (OSError, ValueError):
                pass
            finally:
                self.queue.put(None)

        self.worker = threading.Thread(target=read, daemon=True)
        self.worker.start()
        try:
            self.receive(lambda e: e.get('event') == 'ready')
        except Exception:
            self.close()
            raise

    def request(self, op, **params):
        if op == 'create':
            # This client's choice for a new bot; the daemon supplies none.
            params.setdefault('model', 'openai/synthetic-model')
            params.setdefault('instructions', 'Test agent.')
        self.next_id += 1
        self.socket.sendall((json.dumps(dict(id=self.next_id, op=op, **params)) + '\n').encode())
        return self.receive(lambda e: e.get('id') == self.next_id)

    def close(self):
        try:
            self.socket.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass
        self.worker.join(timeout=2)
        self.reader.close()
        self.socket.close()


class SocketClient:
    def __init__(self, binary, path, url, tools):
        # Keep AF_UNIX paths short even in deep remote snapshot directories.
        self.directory = tempfile.TemporaryDirectory(prefix='agent-bench-', dir='/tmp')
        self.socket_path = Path(self.directory.name) / 'daemon.sock'
        self.followers, self.turns = {}, {}
        self.control = None
        self.process = subprocess.Popen(
            [str(binary), *serve_args(path, url, tools), '--socket', str(self.socket_path)],
            stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
            start_new_session=True, env=clean_env())
        try:
            deadline = time.monotonic() + 5
            while not self.socket_path.exists():
                if self.process.poll() is not None or time.monotonic() > deadline:
                    raise RuntimeError('daemon startup failed')
                time.sleep(.01)
            self.control = Connection(self.socket_path)
        except Exception:
            self.close(kill=True)
            raise

    def request(self, op, **params):
        response = self.control.request(op, **params)
        if 'result' not in response:
            return response
        if op in ('create', 'resume') and params['bot'] not in self.followers:
            bot = params['bot']
            follower = Connection(self.socket_path)
            self.followers[bot] = follower
            assert 'result' in follower.request('follow', bot=bot, after=0)
            follower.receive(lambda e: e.get('event') == 'follow_live')
        if op == 'submit':
            self.turns[response['result']['turn']] = params['bot']
        return response

    def finished(self, turn):
        return self.followers[self.turns[turn]].finished(turn)

    def verify_followers(self, pages):
        for bot, page in pages.items():
            assert self.followers[bot].durable == page['events'], 'follower/replay mismatch'

    def close(self, kill=False):
        try:
            if self.process.poll() is None:
                if kill or self.control is None:
                    self.process.kill()
                else:
                    self.control.request('shutdown')
                self.process.wait(timeout=5)
        finally:
            if self.process.poll() is None:
                self.process.kill()
                self.process.wait(timeout=5)
            for follower in self.followers.values():
                follower.close()
            if self.control:
                self.control.close()
            self.directory.cleanup()
