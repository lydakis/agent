#!/usr/bin/env python3
"""Runs a workflow: a plan script that starts fresh agents, passes what they
return from one to the next in code, and reports once to the agent that
started it.

    workflow.py start PLAN.py [--name NAME] [--parallel N] [--max-agents N]
                              [--timeout DURATION] [--agent-budget-tokens N]
    workflow.py status [NAME] [--agents] [--pretty]
    workflow.py stop NAME

A run lives in ~/.agent/workflows/NAME: run.json (its state now),
events.jsonl (what happened, in order), result.json and log. Each agent is a
new bot, NAME.LABEL, created by the bot that started the run; its turn's
end answers one `wait` request on a single daemon connection, so a run holds
one process however many agents it waits for.

Standard library only, and Python 3.9 (the macOS Command Line Tools').
"""
import fcntl
import hashlib
import json
import os
import re
import signal
import socket
import subprocess
import sys
import threading
import time
import traceback
from pathlib import Path

AGENT = os.environ.get('AGENT_BIN') or 'agent'
NAME = re.compile(r'[A-Za-z0-9_-]{1,48}')
LABEL = re.compile(r'[A-Za-z0-9_.-]{1,64}')
# The result a lead reads in its message; the rest stays in result.json.
REPORT_BYTES = 8 * 1024
# A run past this many agents is a runaway loop, whatever --max-agents says.
AGENT_CEILING = 1000
SUMMARY_INTERVAL = 0.25
STOP_GRACE = 5  # seconds a stopped plan has to reach its next agent()
USAGE = __doc__.split('\n\n')[1]


def now_ms():
    return int(time.time() * 1000)


def home():
    return Path(os.environ.get('HOME') or Path.home()) / '.agent' / 'workflows'


class Refused(Exception):
    """A refusal the CLI prints as one JSON object on stderr."""

    def __init__(self, error, detail, status=1, **facts):
        super().__init__(f'{error}: {detail}')
        self.body = {'error': error, 'detail': detail, **facts}
        self.status = status


class RunEnded(BaseException):
    """The run was stopped, timed out or hit a limit. A BaseException, so a
    plan's `except Exception` does not swallow it."""

    def __init__(self, status, error=None, detail=None):
        super().__init__(detail or status)
        self.status, self.error, self.detail = status, error, detail


def duration(text):
    match = re.fullmatch(r'(\d+)(ms|s|m|h|d)', text)
    if not match:
        raise Refused('usage', f'{text!r} is not a duration such as 30s, 45m or 2h', 2)
    unit = {'ms': .001, 's': 1, 'm': 60, 'h': 3600, 'd': 86400}[match.group(2)]
    if int(match.group(1)) == 0:
        raise Refused('usage', f'{text!r}: a duration must be more than zero', 2)
    return int(match.group(1)) * unit


def cli(*args, stdin=None):
    """One `agent` command; its JSON reply, or its refusal as Refused."""
    done = subprocess.run([AGENT, *args], input=stdin, capture_output=True, text=True)
    if done.returncode != 0:
        said = done.stderr.strip()
        try:
            body = json.loads(said)
            raise Refused(body.pop('error', 'agent_failed'), body.pop('detail', said), **body)
        except ValueError:
            match = re.match(r'agent: ([a-z_]+): ?(.*)', said, re.S)
            if match:
                raise Refused(match.group(1), match.group(2) or match.group(1)) from None
            raise Refused('agent_failed', said or f'agent {args[0]} exited {done.returncode}') from None
    return json.loads(done.stdout) if done.stdout.strip() else None


class Waiter:
    """One daemon connection for every wait of a run. Each agent's turn is
    one `wait` request, answered when the turn ends. Handles are durable, so
    after a dropped connection (a daemon restart) the pending ones are asked
    again on a new one."""

    def __init__(self, path, ended):
        self.path, self.ended = path, ended
        self.lock = threading.Lock()
        self.pending, self.next, self.file, self.gone = {}, 0, None, False
        self.attach(self.connect())

    def connect(self):
        conn = socket.socket(socket.AF_UNIX)
        conn.connect(self.path)
        file = conn.makefile('rw', encoding='utf-8')
        ready = json.loads(file.readline() or '{}')
        if ready.get('event') != 'ready':
            raise OSError(f'no ready line from {self.path}')
        return file

    def attach(self, file):
        with self.lock:
            self.file = file
            again = [(id, entry[0]) for id, entry in self.pending.items()]
        threading.Thread(target=self.read, args=(file,), daemon=True).start()
        for id, body in again:
            self.send(id, body)

    def send(self, id, body):
        with self.lock:
            if self.file is None:
                return  # asked again once the connection is back
            try:
                self.file.write(json.dumps({'id': id, **body}) + '\n')
                self.file.flush()
            except OSError:
                pass  # the reader sees the same failure and reconnects

    def read(self, file):
        try:
            for line in file:
                reply = json.loads(line)
                with self.lock:
                    entry = self.pending.pop(reply.get('id'), None)
                if entry:
                    entry[2] = reply
                    entry[1].set()
        except (OSError, ValueError):
            pass
        with self.lock:
            if self.file is not file:
                return
            self.file = None
        delay = .05
        while True:
            try:
                return self.attach(self.connect())
            except OSError:
                if self.ended():
                    return self.abandon()
                time.sleep(delay)
                delay = min(delay * 2, 5)

    def abandon(self):
        with self.lock:
            entries, self.pending, self.gone = list(self.pending.values()), {}, True
        for entry in entries:
            entry[2] = {'error': 'daemon_unreachable', 'detail': f'lost {self.path}'}
            entry[1].set()

    def request(self, body):
        """One protocol request; its result, or Refused."""
        entry = [body, threading.Event(), None]
        with self.lock:
            if self.gone:
                raise Refused('daemon_unreachable', f'lost {self.path}')
            self.next += 1
            id = self.next
            self.pending[id] = entry
        self.send(id, body)
        entry[1].wait()
        reply = entry[2]
        error = reply.get('error')
        if error is not None:
            if isinstance(error, dict):
                raise Refused(error.get('error', 'request_failed'), error.get('detail'))
            raise Refused(error, reply.get('detail'))
        return reply['result']

    def wait(self, handle):
        try:
            return self.request({'op': 'wait', 'handles': [handle]})['results'][handle]
        except Refused as refused:
            return {'error': refused.body['error'], 'detail': refused.body['detail']}


class Result:
    """What one agent returned. `ok` is true when its turn completed and,
    with a schema, its reply parsed and matched."""

    FIELDS = ('label', 'bot', 'bot_id', 'phase', 'status', 'ok', 'text', 'data', 'error', 'detail',
              'truncated', 'attempts', 'reused')

    def __init__(self, **fields):
        for name in self.FIELDS:
            setattr(self, name, fields.get(name))

    def as_dict(self):
        return {name: getattr(self, name) for name in self.FIELDS if getattr(self, name) is not None}

    def __repr__(self):
        return f'Result({self.label!r}, {self.status}, ok={self.ok})'


def failed(error, detail, label=None):
    return Result(label=label, status='failed', ok=False, error=error, detail=detail)


def plain(value):
    if isinstance(value, Result):
        return value.as_dict()
    raise TypeError(f'{type(value).__name__} is not JSON')


def parse_reply(text):
    text = (text or '').strip()
    fenced = re.fullmatch(r'```(?:json)?\s*\n(.*)\n```', text, re.S)
    def finite_only(name):
        raise ValueError(f'{name} is not JSON')
    return json.loads(fenced.group(1) if fenced else text, parse_constant=finite_only)


def same(a, b):
    """JSON equality: true is not 1, though 1 is 1.0."""
    if isinstance(a, bool) or isinstance(b, bool):
        return type(a) is type(b) and a == b
    if isinstance(a, dict) or isinstance(b, dict):
        return (isinstance(a, dict) and isinstance(b, dict) and a.keys() == b.keys()
                and all(same(a[k], b[k]) for k in a))
    if isinstance(a, list) or isinstance(b, list):
        return isinstance(a, list) and isinstance(b, list) and len(a) == len(b) and all(map(same, a, b))
    return a == b


TYPES = {'object': dict, 'array': list, 'string': str, 'boolean': bool, 'null': type(None)}


def mismatch(value, schema, where='reply'):
    """Why `value` breaks `schema` (type, enum, required, properties,
    items), or None."""
    kinds = schema.get('type')
    if kinds is not None:
        kinds = kinds if isinstance(kinds, list) else [kinds]

        def fits(kind):
            if kind == 'integer':
                return isinstance(value, int) and not isinstance(value, bool)
            if kind == 'number':
                return isinstance(value, (int, float)) and not isinstance(value, bool)
            return isinstance(value, TYPES.get(kind, ()))
        if not any(fits(k) for k in kinds):
            return f'{where} is {type(value).__name__}, not {" or ".join(kinds)}'
    if 'enum' in schema and not any(same(value, member) for member in schema['enum']):
        return f'{where} is {value!r}, not one of {schema["enum"]!r}'
    if isinstance(value, dict):
        for key in schema.get('required', []):
            if key not in value:
                return f'{where} lacks "{key}"'
        for key, inner in schema.get('properties', {}).items():
            if key in value:
                why = mismatch(value[key], inner, f'{where}.{key}')
                if why:
                    return why
    if isinstance(value, list) and isinstance(schema.get('items'), dict):
        for i, item in enumerate(value):
            why = mismatch(item, schema['items'], f'{where}[{i}]')
            if why:
                return why
    return None


class Run:
    def __init__(self, name, folder, plan, options, socket_path, lead, earlier, earlier_bots, earlier_turns):
        self.name, self.dir, self.plan, self.options, self.lead = name, folder, plan, options, lead
        self.prefix = worker_prefix(name, lead)
        self.lock = threading.Lock()
        self.slots = threading.BoundedSemaphore(options['parallel'])
        self.labels, self.running = set(), {}
        # What an earlier runner of this run recorded: the last record of
        # each label, and every bot it asked for, whose names stay taken.
        self.earlier, self.taken = earlier, dict(earlier_bots)
        # The turns the run submitted, by bot: what its token count adds up.
        self.turns = {bot: set(turns) for bot, turns in earlier_turns.items()}
        # --max-agents bounds the run, so the agents earlier runners made count.
        self.made = sum(1 for bot_id in earlier_bots.values() if bot_id is not None)
        self.counts = {'started': 0, 'running': 0, 'completed': 0, 'failed': 0, 'reused': 0}
        self.phases, self.phase_now = [], None
        self.started_ms, self.ended_ms = now_ms(), None
        self.status, self.error, self.detail, self.tokens = 'running', None, None, None
        self.end = None  # RunEnded once stopping
        self.events = open(folder / 'events.jsonl', 'a', encoding='utf-8')
        self.logfile = open(folder / 'log', 'a', encoding='utf-8')
        self.dirty, self.writing = threading.Event(), threading.Lock()
        self.waiter = Waiter(socket_path, lambda: self.end is not None)

    # The record of the run: an event per change, and the summary now.

    def record(self, event, **fields):
        line = json.dumps({'t': now_ms(), 'event': event, **fields}, default=plain)
        with self.lock:
            self.events.write(line + '\n')
            self.events.flush()
        self.dirty.set()

    def log(self, text):
        with self.lock:
            self.logfile.write(f'{time.strftime("%Y-%m-%dT%H:%M:%S")} {text}\n')
            self.logfile.flush()

    def summary(self):
        with self.lock:
            return {'run': self.name, 'status': self.status, 'error': self.error, 'detail': self.detail,
                    'pid': os.getpid(), 'lead': self.lead, 'plan': str(self.plan), 'dir': str(self.dir),
                    'started_ms': self.started_ms, 'ended_ms': self.ended_ms, 'phase': self.phase_now,
                    'phases': [dict(p) for p in self.phases], 'agents': dict(self.counts),
                    'tokens_used': self.tokens, 'options': self.options}

    def write_summary(self, final=False):
        with self.writing:
            # The end's summary is the last; a periodic one after it would be stale.
            if not final and self.status != 'running':
                return
            temporary = self.dir / f'.run.json.{os.getpid()}'
            temporary.write_text(json.dumps(self.summary(), indent=1) + '\n')
            os.replace(temporary, self.dir / 'run.json')

    def summaries(self):
        # At most one summary write per interval, however many agents finish.
        while self.status == 'running':
            self.dirty.wait()
            self.dirty.clear()
            if self.status == 'running':
                self.write_summary()
                time.sleep(SUMMARY_INTERVAL)

    def send(self, *args, stdin=None):
        """An `agent` command to the run's daemon. While the daemon is down
        (a restart) it waits for it rather than starting one of its own."""
        delay = .05
        while True:
            try:
                return cli(*args, stdin=stdin)
            except Refused as refused:
                if refused.body['error'] != 'daemon_unavailable' or self.end:
                    raise
            time.sleep(delay)
            delay = min(delay * 2, 5)

    def phase_of(self, name):
        """The latest phase of that name; agents before any phase count in
        none. Called holding the lock."""
        for p in reversed(self.phases):
            if p['name'] == name:
                return p
        return {'agents': 0, 'failed': 0}

    # The plan's primitives.

    def phase(self, name, description=''):
        with self.lock:
            if self.phases and self.phases[-1].get('ended_ms') is None:
                self.phases[-1]['ended_ms'] = now_ms()
            self.phases.append({'name': name, 'description': description, 'started_ms': now_ms(),
                                'ended_ms': None, 'agents': 0, 'failed': 0})
            self.phase_now = name
        self.record('phase', name=name, description=description)

    def agent(self, prompt, label=None, phase=None, schema=None, model=None, effort=None,
              budget_tokens=None, profile=None, instructions=None, workspace=None, tools=None,
              retries=1):
        if not isinstance(prompt, str) or not prompt:
            raise ValueError('agent() needs a prompt')
        if self.end:
            raise self.end
        with self.lock:
            if label is None:
                label = f'a{len(self.labels) + 1}'
            if not LABEL.fullmatch(label):
                raise ValueError(f'label {label!r}: use 1-64 of A-Z a-z 0-9 _ . -')
            if label in self.labels:
                raise ValueError(f'label {label!r} is used twice in this run')
            self.labels.add(label)
            phase = phase or self.phase_now
        if budget_tokens is None:  # 0 stays 0, which the CLI refuses, rather than no budget
            budget_tokens = self.options['agent_budget_tokens']
        # What a new agent would get, defaults included: a run resumed from
        # another folder or model starts its agents afresh.
        context = [os.getcwd(), os.environ.get('AGENT_MODEL'), os.environ.get('AGENT_REASONING')]
        key = hashlib.sha256(json.dumps([prompt, schema, model, effort, budget_tokens, profile, instructions,
                                         workspace, tools, context], sort_keys=True).encode()).hexdigest()[:16]
        before = self.earlier.get(label)
        if before and before.get('key') == key and before.get('ok'):
            result = Result(**{**before, 'reused': True, 'phase': phase})
            self.finished(result, key)
            return result
        found = None
        if before and before.get('key') == key and before.get('request_id'):
            # Asked for when an earlier runner ended, its turn unrecorded.
            before = found = self.reconcile(before)
        waiting = before and before.get('key') == key and before.get('handle') and before.get('ok') is None
        self.admit(new=not waiting or found is not None)
        try:
            if waiting:
                # Still running when an earlier runner ended: wait for that turn again.
                result = self.follow(label, phase, before, schema, retries, key)
            else:
                result = self.start(label, phase, prompt, schema, retries, key, model=model, effort=effort,
                                    budget_tokens=budget_tokens, profile=profile, instructions=instructions,
                                    workspace=workspace, tools=tools)
        finally:
            self.slots.release()
        self.finished(result, key)
        return result

    def admit(self, new):
        with self.lock:
            over = new and self.made >= self.options['max_agents']
            if not over:
                self.made += new
                self.counts['started'] += 1
        if over:
            # Ends the whole run now, as stop and the timeout do, rather than
            # once this agent's siblings finish.
            self.stop('failed', 'max_agents', f'the run started more than {self.options["max_agents"]} '
                      'agents (--max-agents), counting those of earlier runners of it')
            raise self.end
        while not self.slots.acquire(timeout=1):
            if self.end:
                raise self.end
        if self.end:
            self.slots.release()
            raise self.end

    def reconcile(self, asked):
        """The turn an earlier runner asked for but ended before recording,
        as an agent_started record, or None. It is the run's when it is the
        bot's first turn and carries the request id the run recorded; a bot
        without it (made, then the runner ended before submitting; or
        someone else's of that name) is left alone, its name taken."""
        bot = asked['bot']
        try:
            found = self.waiter.request({'op': 'resume', 'bot': bot})
            first = self.waiter.request({'op': 'turns', 'bot': bot, 'after': 0, 'limit': 1})['turns']
        except Refused as refused:
            if refused.body['error'] == 'bot_not_found':
                with self.lock:  # never made: its name is free again
                    self.taken.pop(bot, None)
            else:
                self.log(f'reconciling {bot}: {refused}')
            return None
        if not first or first[0]['request_id'] != asked['request_id']:
            return None
        return {**asked, 'bot_id': found['id'], 'handle': f'turn:{bot}/{first[0]["turn"]}'}

    def start(self, label, phase, prompt, schema, retries, key, **settings):
        if schema is not None:
            prompt += ('\n\nReply with only a JSON value, no other text, matching this JSON Schema:\n'
                       + json.dumps(schema))
        args = ['run', '--new', '--detach', '--no-spawn']
        for flag, name in (('--model', 'model'), ('--reasoning', 'effort'), ('--budget-tokens', 'budget_tokens'),
                           ('--workspace', 'workspace'), ('--profile', 'profile')):
            if settings[name] is not None:
                args += [flag, str(settings[name])]
        if settings['tools'] is not None:
            args += ['--tools', ','.join(settings['tools'])]
        if settings['instructions'] is not None:
            args.append(f'--instructions={settings["instructions"]}')
        elif settings['profile'] is None:
            args.append('--agents')
        # Recorded before it is asked for, so a runner that ends between the
        # two leaves a resumed run the means to find the turn (reconcile).
        request_id = f'workflow.{self.name}.{key}'
        tries = 0
        while True:
            tries += 1
            bot = f'{self.prefix}{label}' + (f'.{tries}' if tries > 1 else '')
            with self.lock:
                if bot in self.taken:
                    continue
                self.taken[bot] = None
            self.record('agent_submitting', label=label, bot=bot, key=key, request_id=request_id)
            try:
                submitted = self.send(*args, '--bot', bot, '--request-id', request_id, '--', '-', stdin=prompt)
                break
            except Refused as refused:
                if refused.body['error'] != 'bot_exists' or tries >= 20:
                    result = failed(refused.body['error'], refused.body['detail'], label)
                    result.phase = phase
                    return result
        return self.follow(label, phase, {'bot': bot, 'bot_id': submitted['bot_id'],
                                          'handle': submitted['handle']}, schema, retries, key)

    def follow(self, label, phase, worker, schema, retries, key):
        bot, bot_id, handle = worker['bot'], worker['bot_id'], worker['handle']
        with self.lock:
            self.running[bot] = bot_id
            self.taken[bot] = bot_id
            self.counts['running'] += 1
            self.phase_of(phase)['agents'] += 1
            # Made while the run was stopping, after stop() listed the running.
            late = self.end is not None
        attempts = worker.get('attempts') or 0
        self.submitted(bot, handle)
        self.record('agent_started', label=label, bot=bot, bot_id=bot_id, handle=handle, phase=phase, key=key,
                    attempts=attempts)
        if late:
            self.interrupt(bot)
        try:
            while True:
                attempts += 1
                done = self.waiter.wait(handle)
                result = Result(label=label, bot=bot, bot_id=bot_id, phase=phase, attempts=attempts,
                                status=done.get('status', 'failed'), text=done.get('text'),
                                error=done.get('error'), detail=done.get('detail'),
                                truncated=done.get('text_truncated') or None)
                result.ok = result.status == 'completed' and not result.error
                if not result.ok or schema is None:
                    return result
                try:
                    result.data = parse_reply(result.text)
                    why = mismatch(result.data, schema)
                except ValueError as error:
                    result.data, why = None, f'reply is not JSON ({error})'
                if why is None:
                    return result
                result.ok, result.error, result.detail = False, 'schema_mismatch', why
                if attempts > retries or self.end:
                    return result
                retry = f'That reply does not fit: {why}. Reply again with only the JSON value.'
                try:
                    again = self.send('run', '--detach', '--no-spawn', '--bot', bot, '--bot-id', str(bot_id),
                                      '--delivery', 'queue', '--', '-', stdin=retry)
                except Refused as refused:
                    result.detail += f'; asking again failed: {refused.body["detail"]}'
                    return result
                handle = again['handle']
                self.submitted(bot, handle)
                # So a resumed run waits for this reply rather than asking again.
                self.record('agent_started', label=label, bot=bot, bot_id=bot_id, handle=handle, phase=phase,
                            key=key, attempts=attempts)
                if self.end:  # stopped while asking: stop() interrupted the bot before this turn
                    self.interrupt(bot)
        finally:
            with self.lock:
                self.running.pop(bot, None)
                self.counts['running'] -= 1

    def submitted(self, bot, handle):
        with self.lock:
            self.turns.setdefault(bot, set()).add(turn_number(handle))

    def finished(self, result, key):
        with self.lock:
            self.counts['completed' if result.ok else 'failed'] += 1
            if result.reused:
                self.counts['reused'] += 1
            if not result.ok:
                self.phase_of(result.phase)['failed'] += 1
        self.record('agent_finished', key=key, **result.as_dict())

    def parallel(self, items, fn=None):
        """`fn` over each item at once, or each zero-argument callable at
        once; the results in order once all are done. An exception becomes
        a failed Result in its place."""
        items = list(items)
        calls = [(lambda item=item: fn(item)) for item in items] if fn else items
        if not all(callable(c) for c in calls):
            raise TypeError('parallel() takes callables, or items and a function')
        return self.together([[c] for c in calls], lambda call, _: call())

    def pipeline(self, items, *stages):
        """Each item through every stage in turn, items independently, so
        one can be in a later stage while another is in an earlier one. A
        stage takes the previous stage's output (the first takes the item)."""
        if not stages:
            raise TypeError('pipeline() needs at least one stage')
        return self.together([[item] for item in items], lambda value, stage: stage(value), stages)

    def together(self, items, step, stages=(None,)):
        out, ended = [None] * len(items), []

        def each(i):
            value = items[i][0]
            try:
                for stage in stages:
                    value = step(value, stage)
                out[i] = value
            except RunEnded as end:
                ended.append(end)
            except Exception as error:
                out[i] = failed('exception', f'{type(error).__name__}: {error}')
                self.log(''.join(traceback.format_exception(type(error), error, error.__traceback__)))
        threads = [threading.Thread(target=each, args=(i,), daemon=True) for i in range(len(items))]
        for thread in threads:
            thread.start()
        for thread in threads:
            while thread.is_alive():
                thread.join(1)
        if ended:
            raise ended[0]
        return out

    # Ending.

    def stop(self, status, error, detail):
        with self.lock:
            if self.end:
                return
            self.end = RunEnded(status, error, detail)
            running = dict(self.running)
        self.log(f'stopping: {detail}')
        for bot in running:
            self.interrupt(bot)

    def interrupt(self, bot):
        try:
            cli('interrupt', '--bot', bot)
        except Refused as refused:
            self.log(f'interrupt {bot}: {refused.body["detail"]}')

    def execute(self, code):
        namespace = {'__name__': '__workflow__', '__file__': str(self.plan), 'agent': self.agent,
                     'parallel': self.parallel, 'pipeline': self.pipeline, 'phase': self.phase,
                     'log': lambda text: self.record('log', text=str(text)), 'Result': Result,
                     'run': {'name': self.name, 'dir': str(self.dir), 'lead': self.lead}}
        self.record('run_started', run=self.name, plan=str(self.plan), lead=self.lead, options=self.options,
                    resumed=bool(self.earlier))
        threading.Thread(target=self.summaries, daemon=True).start()
        if self.options['timeout']:
            def deadline():
                time.sleep(self.options['timeout'])
                self.stop('failed', 'timeout', f'the run passed --timeout {self.options["timeout_text"]}')
            threading.Thread(target=deadline, daemon=True).start()
        raised = []

        def body():
            try:
                exec(code, namespace)
            except BaseException as caught:  # RunEnded, and a plan's sys.exit() too
                raised.append(caught)
                if not isinstance(caught, RunEnded):
                    self.log(traceback.format_exc())
        # The plan runs on its own thread, so a stop ends the run even while
        # the plan is busy outside agent(): it gets STOP_GRACE to get there.
        plan = threading.Thread(target=body, daemon=True)
        plan.start()
        given_up = None
        while plan.is_alive():
            plan.join(.25)
            if self.end and given_up is None:
                given_up = time.monotonic() + STOP_GRACE
            if given_up and time.monotonic() > given_up:
                self.log(f'the plan did not stop within {STOP_GRACE}s; ending without it')
                break
        result, status, error, detail = None, 'completed', None, None
        if plan.is_alive():
            pass  # ended by self.end below
        elif raised and isinstance(raised[0], RunEnded):
            status, error, detail = raised[0].status, raised[0].error, raised[0].detail
        elif raised:
            status, error = 'failed', 'plan_error'
            detail = ''.join(traceback.format_exception_only(type(raised[0]), raised[0])).strip()
        elif 'result' not in namespace:
            status, error, detail = 'failed', 'no_result', 'the plan ended without setting `result`'
        else:
            result = namespace['result']
        if self.end and status == 'completed':
            status, error, detail = self.end.status, self.end.error, self.end.detail
        return self.finish(result, status, error, detail)

    def finish(self, result, status, error, detail):
        try:
            text = json.dumps(result, indent=1, default=plain, allow_nan=False)
        except (TypeError, ValueError) as unfit:
            text, status, error, detail = 'null', 'failed', 'result_not_json', str(unfit)
        (self.dir / 'result.json').write_text(text + '\n')
        self.tokens = self.usage()
        with self.lock:
            self.status, self.error, self.detail, self.ended_ms = status, error, detail, now_ms()
            if self.phases and self.phases[-1].get('ended_ms') is None:
                self.phases[-1]['ended_ms'] = self.ended_ms
        self.end = self.end or RunEnded(status)
        self.record('run_ended', status=status, error=error, detail=detail, tokens_used=self.tokens)
        self.write_summary(final=True)
        self.dirty.set()
        self.report(text)
        return self.summary()

    def usage(self):
        """Tokens of the turns the run submitted, read from each agent's
        turns from the first of them: the cost follows the run's size rather
        than the store's, and a turn given to an agent after the run is not
        the run's. Turn ids are the store's, so a bot deleted and its name
        reused has none of them."""
        total = 0
        for bot, turns in list(self.turns.items()):
            left, after = set(turns), min(turns) - 1
            while left and after is not None:
                try:
                    page = self.waiter.request({'op': 'turns', 'bot': bot, 'after': after, 'limit': 256})
                except Refused as unread:
                    self.log(f'tokens of {bot}: {unread}')
                    break
                for turn in page['turns']:
                    if turn['turn'] in left:
                        left.discard(turn['turn'])
                        total += turn['input_tokens'] + turn['output_tokens']
                after = page.get('next_after')
        return total

    def report(self, text):
        if not self.lead:
            return
        counts = self.counts
        what = f'Workflow run {self.name} ended: {self.status}'
        if self.error:
            what += f' ({self.error}: {self.detail})'
        cut = text if len(text.encode()) <= REPORT_BYTES else text.encode()[:REPORT_BYTES].decode('utf-8', 'ignore')
        message = (f'{what}.\nAgents: {counts["started"]} started, {counts["completed"]} completed, '
                   f'{counts["failed"]} failed' + (f', {counts["reused"]} reused' if counts['reused'] else '')
                   + (f'; {self.tokens} tokens used' if self.tokens is not None else '') + '.\n'
                   + f'Result{" (cut; all of it in result.json)" if cut != text else ""}:\n{cut}\n'
                   + f'Run folder: {self.dir}')
        try:
            self.send('run', '--detach', '--no-spawn', '--bot', self.lead['bot'], '--bot-id', str(self.lead['bot_id']),
                      '--delivery', 'queue', '--', '-', stdin=message)
        except Refused as refused:
            self.log(f'reporting to {self.lead["bot"]}: {refused.body["detail"]}')


def worker_prefix(name, lead):
    """Agents are named LEAD-RUN.LABEL: under their lead's name, like its
    forks and side chats, which the app does not count as its tasks (the run
    reports once instead). Without a lead, RUN.LABEL."""
    return f'{lead["bot"]}-{name}.' if lead else f'{name}.'


def turn_number(handle):
    return int(handle.rsplit('/', 1)[1])


def history(folder):
    """What earlier runners of a run recorded in its events.jsonl: the last
    record of each label; every bot they asked for, with its id once made;
    and the turns they submitted, by bot."""
    records, bots, turns = {}, {}, {}
    path = folder / 'events.jsonl'
    if not path.exists():
        return records, bots, turns
    for line in path.read_text(encoding='utf-8').splitlines():
        try:
            event = json.loads(line)
        except ValueError:
            continue  # a line a crash cut short
        if event['event'] == 'agent_submitting':
            bots.setdefault(event['bot'], None)
            records[event['label']] = {name: event.get(name) for name in ('label', 'bot', 'key', 'request_id')}
        elif event['event'] == 'agent_started':
            bots[event['bot']] = event['bot_id']
            turns.setdefault(event['bot'], set()).add(turn_number(event['handle']))
            records[event['label']] = {name: event.get(name) for name in
                                       ('label', 'bot', 'bot_id', 'handle', 'key', 'attempts')}
        elif event['event'] == 'agent_finished':
            records[event['label']] = {name: event.get(name) for name in (*Result.FIELDS, 'key')
                                       if name in event}
    return records, bots, turns


def claim(folder):
    """Lock the run for this process's life: one runner per run, and the
    proof `status` and `stop` read that it is still this runner. The file
    holds its pid. None when another runner holds it."""
    held = open(folder / 'lock', 'a+', encoding='utf-8')
    for _ in range(10):  # past a `runner()` look at the same moment
        try:
            fcntl.flock(held, fcntl.LOCK_EX | fcntl.LOCK_NB)
            break
        except BlockingIOError:
            time.sleep(.02)
    else:
        held.close()
        return None
    held.truncate(0)
    held.write(str(os.getpid()))
    held.flush()
    return held


def runner(folder):
    """The pid of the runner holding a run, or None when none does."""
    try:
        lock = open(folder / 'lock', encoding='utf-8')
    except FileNotFoundError:
        return None
    with lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_SH | fcntl.LOCK_NB)
            return None
        except BlockingIOError:
            pass
        for _ in range(50):  # a runner that has just taken the lock writes its pid next
            pid = lock.read().strip()
            if pid.isdigit():
                return int(pid)
            lock.seek(0)
            time.sleep(.01)
        return None


def read_summary(folder):
    try:
        return json.loads((folder / 'run.json').read_text())
    except (OSError, ValueError):
        return None


def flags(argv, valued, switches=()):
    """`--flag VALUE`, `--flag=VALUE` and switches; the rest positional."""
    found, positional, i = {}, [], 0
    while i < len(argv):
        arg = argv[i]
        name, eq, value = arg.partition('=')
        if name in valued:
            if not eq:
                i += 1
                if i == len(argv):
                    raise Refused('usage', f'{name} needs a value', 2)
                value = argv[i]
            found[name] = value
        elif arg in switches:
            found[arg] = True
        elif arg.startswith('--'):
            raise Refused('usage', f'unknown flag {arg}', 2)
        else:
            positional.append(arg)
        i += 1
    return found, positional


def number(found, name, default, low=1, high=None):
    if name not in found:
        return default
    value = found[name]
    if not value.isdigit() or int(value) < low or (high and int(value) > high):
        raise Refused('usage', f'{name} takes a whole number from {low}' + (f' to {high}' if high else ''), 2)
    return int(value)


def start(argv):
    found, positional = flags(argv, ('--name', '--parallel', '--max-agents', '--timeout', '--agent-budget-tokens'))
    if len(positional) != 1:
        raise Refused('usage', 'start takes one plan file', 2)
    plan = Path(positional[0]).resolve()
    try:
        code = compile(plan.read_text(encoding='utf-8'), str(plan), 'exec')
    except OSError as unread:
        raise Refused('plan_unreadable', str(unread)) from None
    except SyntaxError as invalid:
        raise Refused('plan_invalid', f'{invalid.msg} at line {invalid.lineno}') from None
    options = {'parallel': number(found, '--parallel', 16, 1, 64),
               'max_agents': number(found, '--max-agents', 100, 1, AGENT_CEILING),
               'agent_budget_tokens': number(found, '--agent-budget-tokens', None),
               'timeout': duration(found['--timeout']) if '--timeout' in found else 24 * 3600,
               'timeout_text': found.get('--timeout', '24h')}
    lead = None
    if os.environ.get('AGENT_SHELL_CONTEXT') == '1':
        # A bot's shell kills what a command leaves behind and holds a
        # process slot while it runs; a detached command does neither.
        if os.getsid(0) != os.getpgid(0):
            raise Refused('detach_required', 'a run outlives the shell call that starts it',
                          hint='start it with the shell tool\'s detach: true; its result arrives as a message')
        lead = {'bot': os.environ['AGENT_BOT'], 'bot_id': int(os.environ['AGENT_BOT_ID'])}
        nested(lead['bot_id'])
    name = found.get('--name') or time.strftime('wf-%Y%m%d-%H%M%S-') + str(os.getpid())
    if not NAME.fullmatch(name):
        raise Refused('usage', f'run name {name!r}: use 1-48 of A-Z a-z 0-9 _ -', 2)
    if len(worker_prefix(name, lead)) > 60:
        raise Refused('usage', f'agent names start {worker_prefix(name, lead)!r}, which leaves too little of '
                      'the 128 a name may have for labels', 2, hint='pick a shorter --name')
    folder = home() / name
    folder.mkdir(parents=True, exist_ok=True)
    held = claim(folder)
    if held is None:
        raise Refused('run_running', f'run {name} is running (pid {runner(folder)})',
                      hint=f'workflow.py stop {name}, or pick another --name')
    earlier, earlier_bots, earlier_turns = history(folder)
    socket_path = cli('start')['socket']
    run = Run(name, folder, plan, options, socket_path, lead, earlier, earlier_bots, earlier_turns)
    run.held = held
    # Off the handler: it interrupts the main thread, which may hold the lock.
    signal.signal(signal.SIGTERM, lambda *_: threading.Thread(
        target=run.stop, args=('stopped', None, 'stopped by request'), daemon=True).start())
    return run.execute(code)


def nested(bot_id):
    for folder in home().iterdir() if home().is_dir() else ():
        if runner(folder):
            _, bots, _ = history(folder)
            if bot_id in bots.values():
                raise Refused('nested_run', f'this bot is an agent of run {folder.name}; runs do not nest',
                              hint='do the work yourself and reply; the run combines what its agents return')


def status(argv):
    found, positional = flags(argv, (), ('--agents', '--pretty'))
    if len(positional) > 1:
        raise Refused('usage', 'status takes at most one run name', 2)
    def current(folder):
        summary = read_summary(folder)
        if summary and summary['status'] == 'running' and not runner(folder):
            summary['status'], summary['error'] = 'failed', 'runner_gone'
            summary['detail'] = 'its runner exited without recording an end; start the plan again to resume it'
        return summary
    if positional:
        folder = home() / positional[0]
        summary = current(folder)
        if summary is None:
            raise Refused('run_not_found', f'no run {positional[0]} in {home()}')
        if '--agents' in found:
            summary['agent_records'] = list(history(folder)[0].values())
        runs = [summary]
    else:
        runs = [s for s in map(current, sorted(home().iterdir())) if s] if home().is_dir() else []
    if '--pretty' not in found:
        print(json.dumps(runs[0] if positional else runs, indent=1))
        return 0
    for run in runs:
        took = ((run['ended_ms'] or now_ms()) - run['started_ms']) // 1000
        agents = run['agents']
        print(f'{run["run"]}  {run["status"]}' + (f' ({run["error"]})' if run.get('error') else '')
              + (f'  phase {run["phase"]}' if run.get('phase') else '')
              + f'  agents: {agents["running"]} running, {agents["completed"]} done, {agents["failed"]} failed'
              + f'  {took // 60}m{took % 60:02d}s')
        for record in run.get('agent_records', []):
            state = 'running' if record.get('ok') is None else 'ok' if record['ok'] else record.get('error')
            print(f'  {record["label"]}  {record.get("bot") or "-"}  {state}')
    return 0


def stop(argv):
    _, positional = flags(argv, ())
    if len(positional) != 1:
        raise Refused('usage', 'stop takes one run name', 2)
    folder = home() / positional[0]
    summary = read_summary(folder)
    if summary is None:
        raise Refused('run_not_found', f'no run {positional[0]} in {home()}')
    pid, stopping = runner(folder), False
    if pid:
        try:
            os.kill(pid, signal.SIGTERM)
            stopping = True
        except ProcessLookupError:  # it ended between the look and the signal
            summary = read_summary(folder) or summary
    print(json.dumps({'run': positional[0], 'stopping': stopping, 'status': summary['status']}))
    return 0


def main(argv):
    commands = {'start': start, 'status': status, 'stop': stop}
    if not argv or argv[0] in ('-h', '--help', 'help'):
        print(USAGE)
        return 0 if argv else 2
    if argv[0] not in commands:
        raise Refused('usage', f'unknown command {argv[0]}; use start, status or stop', 2)
    outcome = commands[argv[0]](argv[1:])
    if isinstance(outcome, dict):
        print(json.dumps(outcome, indent=1))
        return 0 if outcome['status'] == 'completed' else 1
    return outcome


if __name__ == '__main__':
    threading.stack_size(512 * 1024)
    try:
        sys.exit(main(sys.argv[1:]))
    except Refused as refused:
        print(json.dumps(refused.body), file=sys.stderr)
        sys.exit(refused.status)
