"""Build the lookout eval's store: toy projects played out by scripted agents.

Usage: python3 -m bench.lookout_cases.build OUT [--lead-model chatgpt/gpt-6-sol]

Each scenario in `scenarios.py` is one project: a lead, its tasks, and the
person, each saying what the script says, through a real daemon. The lead
starts and messages its tasks with `"$AGENT_BIN" run`, as a Sonny lead
does, so every brief carries who sent it. A scripted model server stands in
for every model; the store keeps the lead's model as `--lead-model`, so
`sonny lookout --replay --judge lead` on a daemon with that provider forks
each lead with its real model. OUT gets `state.sqlite` and `labels.jsonl`,
one {"task", "turn", "kind", "tell", "cat", "why"} per check.
"""
import argparse
import http.server
import json
import os
import shlex
import socket
import subprocess
import sys
import tempfile
import threading
import time
from collections import deque
from pathlib import Path

from . import scenarios

ROOT = Path(__file__).resolve().parents[2]


class Script:
    """Each bot's model rounds, in order, and the words that name the bot:
    the first message in its history."""

    def __init__(self):
        self.rounds, self.first, self.lock = {}, [], threading.Lock()
        self.failures = []

    def name(self, bot, first):
        with self.lock:
            self.first.append((first, bot))
            self.rounds.setdefault(bot, deque())

    def add(self, bot, *rounds):
        with self.lock:
            self.rounds.setdefault(bot, deque()).extend(rounds)

    def next(self, request):
        texts = [part.get('text', '') for item in request.get('input', []) if item.get('role') == 'user'
                 for part in item.get('content', []) if isinstance(part, dict)]
        first = texts[0] if texts else ''
        with self.lock:
            # A brief may quote the person's first words to the lead: the
            # longest words found name the bot.
            bot = max(((len(words), bot) for words, bot in self.first if words in first), default=(0, None))[1]
            if bot is None or not self.rounds.get(bot):
                self.failures.append((bot, first[:200]))
                return [message('(the script has nothing more to say)')]
            return self.rounds[bot].popleft()


def message(text):
    return {'type': 'message', 'role': 'assistant', 'content': [{'type': 'output_text', 'text': text}]}


def shell(command, call_id):
    return {'type': 'function_call', 'name': 'shell', 'call_id': call_id,
            'arguments': json.dumps({'command': command, 'timeout_ms': 30000})}


class Model(http.server.BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'

    def log_message(self, *_):
        pass

    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        output = self.server.script.next(request)
        text = ''.join(part['text'] for item in output if item['type'] == 'message' for part in item['content'])
        events = [{'type': 'response.created', 'response': {'id': 'response_case'}}]
        if text:
            events.append({'type': 'response.output_text.delta', 'delta': text})
        events.append({'type': 'response.completed', 'response': {
            'model': request['model'], 'status': 'completed', 'output': output,
            'usage': {'input_tokens': 100, 'output_tokens': 10}}})
        body = b''.join(('data: ' + json.dumps(e) + '\n\n').encode() for e in events)
        self.send_response(200)
        self.send_header('Content-Type', 'text/event-stream')
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)


class Daemon:
    """One daemon on the store, and a JSONL connection to it."""

    def __init__(self, binary, store, url, home, lead_provider):
        self.dir = tempfile.TemporaryDirectory(prefix='lookout-cases-', dir='/tmp')
        self.socket_path = Path(self.dir.name) / 'daemon.sock'
        env = {'PATH': f'{binary.parent}:{os.environ["PATH"]}', 'HOME': str(home),
               'AGENT_BIN': str(binary), 'AGENT_SOCKET': str(self.socket_path), 'AGENT_STORE': str(store)}
        providers = ['--provider', f'openai=responses,{url}']
        if lead_provider != 'openai':
            providers += ['--provider', f'{lead_provider}=responses,{url}']
        self.process = subprocess.Popen(
            [str(binary), 'serve', '--store', str(store), *providers, '--socket', str(self.socket_path)],
            stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
            start_new_session=True, env=env)
        deadline = time.monotonic() + 10
        while not self.socket_path.exists():
            if self.process.poll() is not None or time.monotonic() > deadline:
                raise RuntimeError('the daemon did not start')
            time.sleep(.02)
        self.sock = socket.socket(socket.AF_UNIX)
        self.sock.connect(str(self.socket_path))
        self.reader = self.sock.makefile('r')
        self.next_id = 0

    def request(self, op, **params):
        self.next_id += 1
        self.sock.sendall((json.dumps(dict(id=self.next_id, op=op, **params)) + '\n').encode())
        for line in self.reader:
            reply = json.loads(line)
            if reply.get('id') == self.next_id:
                if 'error' in reply:
                    raise RuntimeError(f'{op}: {reply["error"]}')
                return reply['result']
        raise RuntimeError('the daemon closed the connection')

    def finish(self, bot, turn):
        handle = f'turn:{bot}/{turn}'
        result = self.request('wait', handles=[handle], timeout_ms=30000)['results'][handle]
        if result.get('pending') or result.get('status') != 'completed':
            raise RuntimeError(f'{handle} did not complete: {result}')
        return result

    def turns(self, bot):
        try:
            return [t['turn'] for t in self.request('turns', bot=bot, after=0, limit=200)['turns']]
        except RuntimeError:
            return []

    def close(self):
        try:
            self.request('shutdown')
            self.process.wait(timeout=10)
        except Exception:
            self.process.kill()
        self.sock.close()
        self.dir.cleanup()


class Play:
    """Plays one scenario: the steps in order, each waited out, and the
    label of each check it makes."""

    def __init__(self, daemon, script, project, folder, model):
        self.daemon, self.script, self.project, self.folder = daemon, script, project, folder
        self.lead = f'{project}.lead'
        self.model = model
        self.tasks, self.labels, self.calls, self.started = set(), [], 0, False
        # Each task's newest turn and the first line it ended with.
        self.last = {}
        role = (ROOT / 'sonny/agents/coordinator.md').read_text().split('---', 2)[2].strip()
        daemon.request('create', bot=self.lead, workspace=str(folder), model=model,
                       instructions=role, tools=['shell', 'read', 'write', 'edit', 'wait', 'history'])

    def label(self, task, turn, kind, label):
        if label is not None:
            self.labels.append({'task': task, 'turn': turn, 'kind': kind, **label})

    def lead_turn(self, prompt, steps):
        """The lead's turn on `prompt`: a shell call for each message it sends
        a task, then its reply; then each task's turn that started."""
        if not self.started:
            self.script.name(self.lead, prompt)
            self.started = True
        rounds, sent = [], []
        for step in steps:
            if isinstance(step, str):
                continue
            task = f'{self.project}.{step.task}'
            self.calls += 1
            if task not in self.tasks:
                command = (f'"$AGENT_BIN" run --detach --new --bot {task} --workspace "$HOME/projects/{self.project}" '
                           f'-- {shlex.quote(step.text)}')
                self.script.name(task, step.text)
                self.tasks.add(task)
            else:
                command = (f'"$AGENT_BIN" run --detach --delivery {step.delivery} --bot {task} '
                           f'-- {shlex.quote(step.text)}')
            rounds.append([shell(command, f'{self.project}-{self.calls}')])
            self.script.add(task, *[[message(text)] for text in step.replies])
            sent.append((task, step))
        reply = next((s for s in steps if isinstance(s, str)), 'Started.')
        rounds.append([message(reply)])
        self.script.add(self.lead, *rounds)
        before = {task: len(self.daemon.turns(task)) for task, _ in sent}
        turn = self.daemon.request('submit', bot=self.lead, request_id=f'{self.lead}-{time.monotonic_ns()}',
                                   prompt=prompt, delivery='queue')['turn']
        self.daemon.finish(self.lead, turn)
        self.settle(sent, before)

    def settle(self, sent, before):
        """Waits out each task's turn the lead's messages started, and labels
        how each ended."""
        for task, step in sent:
            deadline = time.monotonic() + 30
            while len(self.daemon.turns(task)) <= before[task]:
                if time.monotonic() > deadline:
                    raise RuntimeError(f'{task} never started a turn')
                time.sleep(.05)
            turn = self.daemon.turns(task)[before[task]]
            self.daemon.finish(task, turn)
            self.label(task, turn, 'Ended', step.ended)
            self.last[task] = (turn, step.replies[-1])

    def notify(self, tasks, steps):
        """The lead hears how these tasks' newest turns ended, as the
        lookout tells it, and takes its turn on that."""
        lines = []
        for task in tasks:
            name = f'{self.project}.{task}'
            turn, reply = self.last[name]
            lines.append(f'- {name}: turn:{name}/{turn} completed: {reply.splitlines()[0][:160]}')
        self.lead_turn('Task updates: task turns that ended since you last heard:\n' + '\n'.join(lines), steps)

    def person(self, task, text, replies, said, ended):
        """The person's own words in a task, and the turn they start."""
        task = f'{self.project}.{task}'
        self.script.add(task, *[[message(reply)] for reply in replies])
        turn = self.daemon.request('submit', bot=task, request_id=f'{task}-{time.monotonic_ns()}',
                                   prompt=text, delivery='queue')['turn']
        self.daemon.finish(task, turn)
        self.label(task, turn, 'Said', said)
        self.label(task, turn, 'Ended', ended)
        self.last[task] = (turn, replies[-1])


def subscribe(store, lead, project):
    """The subscription Sonny writes for a lead when it opens its project,
    which is what the lookout replays the lead's checks for."""
    folder = store.parent / f'{store.name}.lookout'
    folder.mkdir(exist_ok=True)
    sub = {'name': 'tasks', 'on': ['said', 'ended', 'waiting', 'quiet'],
           'tasks_of': [lead], 'bots': [f'{project}.*'], 'updates': True}
    (folder / f'{lead}.json').write_text(json.dumps({'subscriptions': [sub]}, indent=2) + '\n')


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('out', type=Path)
    parser.add_argument('--lead-model', default='chatgpt/gpt-6-sol')
    parser.add_argument('--only', help='one scenario, by project name')
    parser.add_argument('--binary', type=Path, default=ROOT / '.local/target/release/agent')
    args = parser.parse_args()
    # The scripted model speaks the Responses API, so the lead's fork can
    # replay only on a provider that does.
    if args.lead_model.split('/')[0] not in ('openai', 'chatgpt'):
        sys.exit('--lead-model must be an openai/ or chatgpt/ model: the scripted cases are Responses items')
    # The daemon takes absolute folders only.
    args.out = args.out.resolve()
    args.out.mkdir(parents=True, exist_ok=True)
    store = args.out / 'state.sqlite'
    if store.exists():
        sys.exit(f'{store} exists; build into an empty folder')
    home = args.out / 'home'
    home.mkdir(exist_ok=True)
    script = Script()

    class Server(http.server.ThreadingHTTPServer):
        daemon_threads = True
    server = Server(('127.0.0.1', 0), Model)
    server.script = script
    threading.Thread(target=server.serve_forever, daemon=True).start()
    url = f'http://127.0.0.1:{server.server_port}/v1'
    daemon = Daemon(args.binary, store, url, home, args.lead_model.split('/')[0])
    labels = []
    try:
        for scenario in scenarios.ALL:
            if args.only and scenario.project != args.only:
                continue
            folder = home / 'projects' / scenario.project
            folder.mkdir(parents=True, exist_ok=True)
            play = Play(daemon, script, scenario.project, folder, args.lead_model)
            subscribe(store, play.lead, scenario.project)
            scenario.play(play)
            labels.extend(play.labels)
            print(f'{scenario.project}: {len(play.labels)} checks labelled', file=sys.stderr)
    finally:
        daemon.close()
        server.shutdown()
    if script.failures:
        sys.exit(f'the script ran out or missed a bot: {script.failures[:5]}')
    with open(args.out / 'labels.jsonl', 'w') as out:
        for label in labels:
            out.write(json.dumps(label) + '\n')
    tell = sum(label['tell'] for label in labels)
    print(f'{len(labels)} checks, {tell} the lead should hear about', file=sys.stderr)


if __name__ == '__main__':
    main()
