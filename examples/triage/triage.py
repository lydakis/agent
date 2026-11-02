#!/usr/bin/env python3
"""Triage issues with Agent bots, through two tools this program declares and answers.

It talks to Agent the two ways any program can, with no SDK: the `agent` CLI,
whose commands print JSON, and the daemon's Unix socket, which speaks JSON lines.

    python3 triage.py ISSUES.jsonl >> triaged.jsonl

Each line of ISSUES.jsonl is {"number", "title", "body"}. Each issue is triaged
by its own fork of one template bot, which sees only two tools:
`search_issues`, answered from ISSUES.jsonl, and `set_triage`, whose decision
this program prints as a JSON line. AGENT_MODEL names the model, as for `agent run`.
"""
import json
import os
import socket
import subprocess
import sys
import threading
import time

AGENT = os.environ.get('AGENT_BIN', 'agent')
TEMPLATE = 'triage'   # set up once, never run: every issue forks it
TAG = 'triage-app'    # who answers the declared tools: this program
LABELS = ['bug', 'feature', 'docs', 'security', 'duplicate', 'question']

INSTRUCTIONS = """You triage issues for a small open-source project, one issue per conversation.
First look for earlier reports with search_issues. Then call set_triage once.
Finish with one sentence that says what you decided and why."""

# A waiting call is parked, a row in the store, so let it outlast a restart of this program.
TOOLS = [
    {'name': 'search_issues', 'answered_by': TAG, 'expire_ms': 3_600_000,
     'description': 'Search the tracker for issues whose title or body contains every word of the query.',
     'parameters': {'type': 'object', 'properties': {'query': {'type': 'string'}},
                    'required': ['query'], 'additionalProperties': False}},
    {'name': 'set_triage', 'answered_by': TAG, 'expire_ms': 3_600_000,
     'description': 'Record the decision for the issue in this conversation.',
     'parameters': {'type': 'object', 'additionalProperties': False,
                    'required': ['labels', 'priority', 'summary'],
                    'properties': {'labels': {'type': 'array', 'items': {'type': 'string', 'enum': LABELS}},
                                   'priority': {'type': 'string', 'enum': ['p0', 'p1', 'p2', 'p3']},
                                   'duplicate_of': {'type': 'integer'},
                                   'summary': {'type': 'string'}}}},
]


def cli(*args):
    """Run one agent command and parse the JSON it prints; a refusal is JSON on stderr.

    `wait` exits 1 when a turn failed but still prints every result.
    """
    done = subprocess.run([AGENT, *args], capture_output=True, text=True)
    if not done.stdout:
        sys.exit(f'agent {args[0]} failed: {done.stderr.strip()}')
    return json.loads(done.stdout)


class Session:
    """One socket session: a `ready` line, then requests and replies matched by id."""

    def __init__(self, path):
        self.sock = socket.socket(socket.AF_UNIX)
        self.sock.connect(path)
        self.lines = self.sock.makefile('r', encoding='utf-8')
        self.ready = json.loads(self.lines.readline())
        self.next_id = 0

    def send(self, op, **fields):
        self.next_id += 1
        self.sock.sendall((json.dumps({'id': self.next_id, 'op': op, **fields}) + '\n').encode())
        return self.next_id

    def request(self, op, **fields):
        sent = self.send(op, **fields)
        for line in self.lines:  # events for this session may come first
            message = json.loads(line)
            if message.get('id') == sent:
                return message


def set_up_template(session):
    """Make the template bot, or bring its tools up to date. CLI-made bots can't declare tools."""
    tools = {'instructions': INSTRUCTIONS, 'tools': [], 'declare': TOOLS}
    made = session.request('create', bot=TEMPLATE, model=os.environ['AGENT_MODEL'],
                           workspace=os.getcwd(), **tools)  # a turn needs a folder, even with no file tools
    if made.get('error') == 'bot_exists':
        made = session.request('configure', bot=TEMPLATE, **tools)
    if 'error' in made:
        sys.exit(f'template: {made}')


def answer(call, issues):
    """What the model reads back from one call to a declared tool."""
    number = int(call['bot'].rsplit('.', 1)[1])
    arguments = call['arguments']
    if call['name'] == 'search_issues':
        words = arguments['query'].lower().split()
        found = [{'number': n, 'title': i['title']} for n, i in issues.items()
                 if n != number and all(w in (i['title'] + ' ' + i['body']).lower() for w in words)]
        return json.dumps(found) if found else 'No matching issues.'
    print(json.dumps({'issue': number, **arguments}), flush=True)
    return 'Recorded.'


def serve(session, issues):
    """Answer every call to our tools, from any bot, for as long as the program runs.

    A started call is the cue to list the calls waiting for us, which carry
    their arguments whole; so is going live, for calls that began before.
    """
    session.send('follow', bot='*', since_ms=int(time.time() * 1000), kinds=['tool_started'])
    listing, answered = None, set()
    for line in session.lines:
        message = json.loads(line)
        if message.get('event') == 'follow_live' or message.get('data', {}).get('answered_by') == TAG:
            listing = session.send('tool_calls', tag=TAG)
        elif message.get('id') == listing and 'result' in message:
            page = message['result']
            for call in page['calls']:
                key = (call['bot'], call['turn'], call['call_id'])
                if key not in answered:
                    answered.add(key)
                    session.send('tool_result', bot=call['bot'], turn=call['turn'],
                                 call_id=call['call_id'], output=answer(call, issues))
            if page['next_after'] is not None:
                listing = session.send('tool_calls', tag=TAG, after=page['next_after'])
        elif 'error' in message:
            print(f'triage: {message}', file=sys.stderr)


def main():
    issues = {i['number']: i for i in (json.loads(line) for line in open(sys.argv[1]) if line.strip())}
    socket_path = cli('start')['socket']  # starts the daemon if none runs
    set_up_template(Session(socket_path))
    threading.Thread(target=serve, args=(Session(socket_path), issues), daemon=True).start()
    handles = []
    for number, issue in issues.items():
        submitted = cli('run', '--fork', TEMPLATE, '--bot', f'{TEMPLATE}.{number}', '--detach',
                        '--request-id', f'issue-{number}', '--',
                        f"Issue #{number}: {issue['title']}\n\n{issue['body']}")
        handles.append(submitted['handle'])
    failed = 0
    for start in range(0, len(handles), 64):  # a wait takes at most 64 handles
        results = cli('wait', '--timeout', '10m', *handles[start:start + 64])['results']
        for handle in handles[start:start + 64]:
            outcome = results.get(handle, {'status': 'pending', 'text': '', 'error': 'not finished'})
            failed += outcome['status'] != 'completed'
            print(f"{handle}: {outcome['status']}: {outcome['text'] or outcome['error']}", file=sys.stderr)
    sys.exit(1 if failed else 0)


if __name__ == '__main__':
    main()
