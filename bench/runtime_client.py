"""Small JSONL controller shared by lifecycle tests and measurements."""
import json
import queue
import subprocess
import threading
import time
from .targets import clean_env

def serve_args(path, url, tools="echo", model="synthetic-model", key_env=None,
               provider="openai", family="responses", extra=()):
    """Arguments for a stdio service bound to one synthetic provider endpoint."""
    spec = f'{provider}={family},{url}' + (f',{key_env}' if key_env else '')
    return ['serve', '--store', str(path), '--provider', spec, *extra]


class Client:
    def __init__(self, binary, path, url, tools="echo", model="synthetic-model", key_env=None, env=None,
                 provider="openai", family="responses", extra=()):
        self.process = subprocess.Popen([str(binary), *serve_args(path, url, tools, model, key_env, provider, family, extra)],
                                        stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                        stderr=subprocess.DEVNULL, text=True, env=env or clean_env(),
                                        start_new_session=True)
        self.queue = queue.Queue()
        self.saved = []
        self.next_id = 0
        # What this client tells a new bot when the caller says nothing; the
        # daemon supplies no agent behavior.
        self.model = f'{provider}/{model}'
        self.instructions = 'Test agent.'
        self.tools = tools.split(',')
        def read():
            for line in self.process.stdout:
                message = json.loads(line)
                if message.get('event') == 'turn_finished':
                    message['_received_at'] = time.monotonic()
                self.queue.put(message)
            self.queue.put(None)
        self.reader = threading.Thread(target=read, daemon=True)
        self.reader.start()
        try:
            self.ready = self.receive(lambda m: m.get('event') == 'ready')
        except Exception:
            self.close(kill=True)
            raise

    def receive(self, predicate, timeout=5):
        for index, message in enumerate(self.saved):
            if predicate(message):
                return self.saved.pop(index)
        deadline = time.monotonic() + timeout
        while True:
            message = self.queue.get(timeout=max(.01, deadline - time.monotonic()))
            if message is None:
                raise AssertionError('runtime exited before expected response')
            if predicate(message):
                return message
            self.saved.append(message)

    def request(self, op, **params):
        if op == 'create':
            params.setdefault('model', self.model)
            params.setdefault('instructions', self.instructions)
            params.setdefault('tools', self.tools)
        self.next_id += 1
        self.process.stdin.write(json.dumps({'id': self.next_id, 'op': op, **params}) + '\n')
        self.process.stdin.flush()
        return self.receive(lambda m: m.get('id') == self.next_id)

    def finished(self, turn):
        return self.receive(lambda m: m.get('event') == 'turn_finished' and m.get('turn') == turn)

    def close(self, kill=False):
        if self.process.poll() is None:
            if kill:
                self.process.kill()
            else:
                self.process.stdin.close()
            self.process.wait(timeout=5)
        self.reader.join(timeout=1)
        if not self.process.stdin.closed:
            self.process.stdin.close()
        self.process.stdout.close()
