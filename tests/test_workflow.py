"""The workflow skill's runner: plans, schemas, resuming, stopping, and a lead
that starts a run from its shell and hears back once."""
import http.server
import importlib.util
import json
import os
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from pathlib import Path

from bench.targets import clean_env

ROOT = Path(__file__).resolve().parent.parent
RUNNER = ROOT / 'app/skills/workflow/workflow.py'
BINARY = ROOT / '.local/target/release/agent'
spec = importlib.util.spec_from_file_location('workflow', RUNNER)
workflow = importlib.util.module_from_spec(spec)
spec.loader.exec_module(workflow)


class Model(http.server.BaseHTTPRequestHandler):
    """A Responses stub. A worker's prompt says what it replies:
    `say:TEXT`, `json:VALUE`, `delay:MS:TEXT`, `hold:` (until released),
    `fail:` (a provider refusal), `badjson:` (prose, then JSON once told it
    does not fit). A lead's `start:COMMAND` runs COMMAND detached, or in the
    foreground with `fg:COMMAND`."""
    protocol_version = 'HTTP/1.1'

    def log_message(self, *_):
        pass

    def handle(self):
        try:
            super().handle()
        except (BrokenPipeError, ConnectionResetError):
            pass  # a held reply whose turn was interrupted

    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        texts = [i['content'][0]['text'] for i in request['input'] if i.get('role') == 'user']
        user, last = texts[-1], request['input'][-1]
        self.server.prompts.append(user)
        called = last.get('type') == 'function_call_output'
        text, output = '', []
        if user.startswith('fail:'):
            body = json.dumps({'error': {'message': 'refused'}}).encode()
            self.send_response(400)
            self.send_header('Content-Type', 'application/json')
            self.send_header('Content-Length', str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        if user.startswith(('start:', 'fg:')) and not called:
            command = user.split(':', 1)[1]
            output = [{'type': 'function_call', 'name': 'shell', 'call_id': 'sh-1',
                       'arguments': json.dumps({'command': command, 'detach': user.startswith('start:')})}]
        elif user.startswith(('start:', 'fg:')):
            self.server.shell_outputs.append(last['output'])
            text = 'started'
        elif user.startswith('hold:'):
            self.server.release.wait(30)
            text = 'held'
        elif user.startswith('delay:'):
            _, ms, text = user.split(':', 2)
            time.sleep(int(ms) / 1000)
        elif user.startswith('say:'):
            text = user[4:].split('\n\n')[0]
        elif user.startswith('json:'):
            text = user[5:].split('\n\n')[0]
        elif user.startswith('That reply does not fit'):
            text = '```json\n{"verdict": "real"}\n```'
        elif user.startswith('badjson:'):
            text = 'I think it is real.'
        else:
            text = 'noted'
        if text:
            output = [{'type': 'message', 'role': 'assistant', 'content': [{'type': 'output_text', 'text': text}]}]
        events = [{'type': 'response.created', 'response': {'id': 'r'}},
                  *([{'type': 'response.output_text.delta', 'delta': text}] if text else []),
                  {'type': 'response.completed', 'response': {'status': 'completed', 'output': output,
                   'usage': {'input_tokens': 3, 'output_tokens': 2, 'input_tokens_details': {'cached_tokens': 0}}}}]
        body = b''.join(f'data: {json.dumps(e)}\n\n'.encode() for e in events)
        self.send_response(200)
        self.send_header('Content-Type', 'text/event-stream')
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)


class Pieces(unittest.TestCase):
    def test_schema_mismatches_name_the_place(self):
        schema = {'type': 'object', 'required': ['items'],
                  'properties': {'items': {'type': 'array', 'items': {'type': 'integer'}},
                                 'kind': {'enum': ['a', 'b']}}}
        self.assertIsNone(workflow.mismatch({'items': [1, 2], 'kind': 'a'}, schema))
        self.assertEqual(workflow.mismatch({}, schema), 'reply lacks "items"')
        self.assertEqual(workflow.mismatch({'items': [1, True]}, schema), 'reply.items[1] is bool, not integer')
        self.assertEqual(workflow.mismatch({'items': [], 'kind': 'c'}, schema),
                         "reply.kind is 'c', not one of ['a', 'b']")
        self.assertEqual(workflow.mismatch([], schema), 'reply is list, not object')

    def test_replies_parse_bare_or_fenced(self):
        self.assertEqual(workflow.parse_reply(' {"a": 1} '), {'a': 1})
        self.assertEqual(workflow.parse_reply('```json\n[1]\n```'), [1])
        with self.assertRaises(ValueError):
            workflow.parse_reply('Here: {"a": 1}')
        with self.assertRaises(ValueError):
            workflow.parse_reply('{"score": NaN}')

    def test_durations_and_flags(self):
        self.assertEqual(workflow.duration('90s'), 90)
        self.assertEqual(workflow.duration('2h'), 7200)
        with self.assertRaises(workflow.Refused):
            workflow.duration('2 hours')
        with self.assertRaises(workflow.Refused):
            workflow.duration('0s')
        found, rest = workflow.flags(['plan.py', '--name=x', '--parallel', '4', '--pretty'],
                                     ('--name', '--parallel'), ('--pretty',))
        self.assertEqual((found, rest), ({'--name': 'x', '--parallel': '4', '--pretty': True}, ['plan.py']))
        with self.assertRaises(workflow.Refused):
            workflow.flags(['--nope'], ())

    def test_history_waits_on_the_latest_reply_asked_for(self):
        with tempfile.TemporaryDirectory() as temp:
            folder = Path(temp)
            started = {'event': 'agent_started', 'label': 'x', 'bot': 'r.x', 'bot_id': 7, 'key': 'k'}
            lines = [{**started, 'handle': 'turn:r.x/1', 'attempts': 0},
                     {**started, 'handle': 'turn:r.x/2', 'attempts': 1}]
            (folder / 'events.jsonl').write_text(''.join(json.dumps(line) + '\n' for line in lines) + '{"cut')
            records, bots = workflow.history(folder)
            self.assertEqual((records['x']['handle'], records['x']['attempts']), ('turn:r.x/2', 1))
            self.assertEqual(bots, {'r.x': 7})

    def test_one_runner_holds_a_run(self):
        with tempfile.TemporaryDirectory() as temp:
            folder = Path(temp)
            self.assertIsNone(workflow.runner(folder))
            held = workflow.claim(folder)
            self.assertEqual(workflow.runner(folder), os.getpid())
            # flock is per open file, so a second claim in this process is refused like another's.
            self.assertIsNone(workflow.claim(folder))
            held.close()
            self.assertIsNone(workflow.runner(folder))
            workflow.claim(folder).close()


@unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'set AGENT_TEST_RUNTIME=1 after a Rust release build')
class Runs(unittest.TestCase):
    def setUp(self):
        (ROOT / '.local').mkdir(exist_ok=True)
        temp = tempfile.TemporaryDirectory(dir=ROOT / '.local')
        self.addCleanup(temp.cleanup)
        self.path = Path(temp.name)
        self.home = self.path / 'home'
        self.home.mkdir()

        class Server(http.server.ThreadingHTTPServer):
            daemon_threads = True
        self.model = Server(('127.0.0.1', 0), Model)
        self.model.prompts, self.model.shell_outputs = [], []
        self.model.release = threading.Event()
        self.addCleanup(self.model.release.set)
        threading.Thread(target=self.model.serve_forever, daemon=True).start()
        self.addCleanup(self.model.server_close)
        self.addCleanup(self.model.shutdown)
        self.env = {**clean_env(), 'HOME': str(self.home), 'AGENT_STORE': str(self.path / 'state.sqlite'),
                    'AGENT_SOCKET': str(self.path / 'agent.sock'), 'AGENT_BIN': str(BINARY),
                    'AGENT_MODEL': 'stub/model'}
        url = f'http://127.0.0.1:{self.model.server_port}/v1'
        self.daemon = subprocess.Popen([str(BINARY), 'serve', '--store', self.env['AGENT_STORE'], '--socket',
                                        self.env['AGENT_SOCKET'], '--provider', f'stub=responses,{url}'],
                                       env=self.env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        self.addCleanup(lambda: self.daemon.wait(30))
        self.addCleanup(self.agent, 'shutdown')
        for _ in range(200):
            if Path(self.env['AGENT_SOCKET']).exists():
                break
            time.sleep(.05)

    def agent(self, *args, check=True):
        done = subprocess.run([str(BINARY), *args], env=self.env, capture_output=True, text=True, timeout=30)
        if check:
            self.assertEqual(done.returncode, 0, done.stderr)
        lines = done.stdout.splitlines()
        # A blocking run streams events; its last line is the turn's end.
        return json.loads(lines[-1]) if len(lines) > 1 and args[0] == 'run' else (
            json.loads(done.stdout) if lines else None)

    def plan(self, text, name='plan.py'):
        path = self.path / name
        path.write_text(text)
        return path

    def start(self, plan, *args, background=False, cwd=None):
        command = [sys.executable, str(RUNNER), 'start', str(plan), *args]
        if background:
            return subprocess.Popen(command, env=self.env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        done = subprocess.run(command, env=self.env, capture_output=True, text=True, timeout=60, cwd=cwd)
        return done.returncode, json.loads(done.stdout) if done.stdout.strip() else json.loads(done.stderr)

    def folder(self, name):
        return self.home / '.agent/workflows' / name

    def events(self, name):
        return [json.loads(line) for line in (self.folder(name) / 'events.jsonl').read_text().splitlines()]

    def result(self, name):
        return json.loads((self.folder(name) / 'result.json').read_text())

    def test_a_plan_fans_out_retries_a_schema_and_keeps_a_failure_in_its_place(self):
        plan = self.plan('''
phase("Find", "what to check")
found = agent('json:{"claims": ["a", "b", "c"]}', label="find",
              schema={"type": "object", "required": ["claims"]})
phase("Check")
VERDICT = {"type": "object", "required": ["verdict"], "properties": {"verdict": {"enum": ["real", "not-real"]}}}
checks = parallel(found.data["claims"], lambda c: agent(
    "badjson:" + c if c == "b" else "fail:" + c if c == "c" else 'json:{"verdict": "not-real"}',
    label="check-" + c, schema=VERDICT))
lengths = pipeline(["x", "yy"], lambda s: agent("say:" + s * 2, label="double-" + s),
                   lambda r: len(r.text))
result = {"verdicts": [c.data["verdict"] if c.ok else c.error for c in checks], "lengths": lengths,
          "first": checks[0]}
''')
        status, summary = self.start(plan, '--name', 'fan')
        self.assertEqual((status, summary['status']), (0, 'completed'), summary)
        result = self.result('fan')
        self.assertEqual(result['verdicts'][:2], ['not-real', 'real'])
        self.assertNotEqual(result['verdicts'][2], 'real')
        self.assertEqual(result['lengths'], [2, 4])
        self.assertEqual((result['first']['bot'], result['first']['ok']), ('fan.check-a', True))
        self.assertEqual(summary['agents'], {'started': 6, 'running': 0, 'completed': 5, 'failed': 1, 'reused': 0})
        self.assertEqual([(p['name'], p['agents'], p['failed']) for p in summary['phases']],
                         [('Find', 1, 0), ('Check', 5, 1)])
        # Five tokens a reply, and the refused agent made none.
        self.assertEqual(summary['tokens_used'], 30)
        retried = next(e for e in self.events('fan') if e['event'] == 'agent_finished' and e['label'] == 'check-b')
        self.assertEqual((retried['attempts'], retried['data']), (2, {'verdict': 'real'}))
        self.assertIn('That reply does not fit: reply is not JSON', '\n'.join(self.model.prompts))
        # Each agent is a bot of its own, created with the schema in its prompt.
        bots = {b['name'] for b in self.agent('ls')}
        self.assertTrue({'fan.find', 'fan.check-a', 'fan.double-yy'} <= bots)
        self.assertIn('matching this JSON Schema', next(p for p in self.model.prompts if p.startswith('badjson:b')))

    def test_starting_again_reuses_finished_agents_and_reruns_the_rest(self):
        text = '''
first = agent("say:one", label="one")
second = agent("fail:two", label="two")
result = [first.text, second.ok]
'''
        plan = self.plan(text)
        self.assertEqual(self.start(plan, '--name', 'again')[1]['agents']['failed'], 1)
        asked = len(self.model.prompts)
        self.plan(text.replace('fail:two', 'say:two'))
        status, summary = self.start(plan, '--name', 'again')
        self.assertEqual(status, 0)
        self.assertEqual(summary['agents'], {'started': 1, 'running': 0, 'completed': 2, 'failed': 0, 'reused': 1})
        self.assertEqual(self.result('again'), ['one', True])
        self.assertEqual(self.model.prompts[asked:], ['say:two'])
        self.assertIn('again.two.2', {b['name'] for b in self.agent('ls')})
        # From another folder an agent would work elsewhere, so none is reused.
        status, summary = self.start(plan, '--name', 'again', cwd=self.home)
        self.assertEqual((status, summary['agents']['reused'], summary['agents']['started']), (0, 0, 2))

    def test_stop_interrupts_running_agents_and_ends_the_run_stopped(self):
        plan = self.plan('''
phase("Wait")
held = parallel(["a", "b"], lambda x: agent("hold:" + x, label="hold-" + x))
agent("say:never", label="after")
result = "unreachable"
''')
        runner = self.start(plan, '--name', 'halt', background=True)
        self.addCleanup(runner.kill)
        for _ in range(200):
            summary = workflow.read_summary(self.folder('halt'))
            if summary and summary['agents']['running'] == 2:
                break
            time.sleep(.05)
        stopped = subprocess.run([sys.executable, str(RUNNER), 'stop', 'halt'], env=self.env,
                                 capture_output=True, text=True, timeout=30)
        self.assertEqual(json.loads(stopped.stdout)['stopping'], True)
        out, _ = runner.communicate(timeout=30)
        summary = json.loads(out)
        self.assertEqual((runner.returncode, summary['status'], summary['detail']), (1, 'stopped', 'stopped by request'))
        statuses = {e['label']: e['status'] for e in self.events('halt') if e['event'] == 'agent_finished'}
        self.assertEqual(statuses, {'hold-a': 'interrupted', 'hold-b': 'interrupted'})
        self.assertNotIn('say:never', self.model.prompts)

    def test_a_plan_busy_outside_agent_is_stopped_and_one_runner_holds_a_run(self):
        plan = self.plan('import time\ntime.sleep(600)\nresult = 1\n')
        runner = self.start(plan, '--name', 'busy', background=True)
        self.addCleanup(runner.kill)
        for _ in range(200):
            if workflow.runner(self.folder('busy')):
                break
            time.sleep(.05)
        status, refused = self.start(plan, '--name', 'busy')
        self.assertEqual((status, refused['error']), (1, 'run_running'))
        subprocess.run([sys.executable, str(RUNNER), 'stop', 'busy'], env=self.env, timeout=30, check=True,
                       capture_output=True)
        out, _ = runner.communicate(timeout=30)
        self.assertEqual(json.loads(out)['status'], 'stopped')
        self.assertIsNone(workflow.runner(self.folder('busy')))

    def test_a_daemon_restart_mid_run_is_waited_through(self):
        plan = self.plan('''
held = agent("hold:x", label="held")
after = agent("say:after", label="after")
result = [held.status, after.text]
''')
        runner = self.start(plan, '--name', 'restart', background=True)
        self.addCleanup(runner.kill)
        for _ in range(200):
            summary = workflow.read_summary(self.folder('restart'))
            if summary and summary['agents']['running'] == 1:
                break
            time.sleep(.05)
        self.agent('shutdown')
        self.daemon.wait(30)
        url = f'http://127.0.0.1:{self.model.server_port}/v1'
        self.daemon = subprocess.Popen([str(BINARY), 'serve', '--store', self.env['AGENT_STORE'], '--socket',
                                        self.env['AGENT_SOCKET'], '--provider', f'stub=responses,{url}'],
                                       env=self.env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        out, _ = runner.communicate(timeout=60)
        self.assertEqual(json.loads(out)['status'], 'completed', out)
        self.assertEqual(self.result('restart'), ['interrupted', 'after'], self.events('restart'))

    def test_limits_end_a_runaway_plan(self):
        plan = self.plan('''
while True:
    agent("say:again")
''')
        status, summary = self.start(plan, '--name', 'loop', '--max-agents', '3')
        self.assertEqual((status, summary['status'], summary['error']), (1, 'failed', 'max_agents'))
        self.assertEqual(summary['agents']['started'], 3)
        # The bound is the run's: resumed, it reuses the three and starts no more.
        asked = len(self.model.prompts)
        status, summary = self.start(plan, '--name', 'loop', '--max-agents', '3')
        self.assertEqual((summary['error'], summary['agents']['reused'], summary['agents']['started']),
                         ('max_agents', 3, 0))
        self.assertEqual(len(self.model.prompts), asked)
        status, summary = self.start(self.plan('x = 1\n', 'empty.py'), '--name', 'empty')
        self.assertEqual((summary['status'], summary['error']), ('failed', 'no_result'))
        status, refused = self.start(self.plan('def (', 'broken.py'))
        self.assertEqual((status, refused['error']), (1, 'plan_invalid'))
        # Agents refused before a bot exists: an unknown model, and a zero budget kept as zero.
        refused = self.plan('result = [agent("say:x", label="bad", model="nope/none").ok, '
                            'agent("say:x", label="zero", budget_tokens=0).ok]\n', 'refused.py')
        self.assertEqual(self.start(refused, '--name', 'refused')[1]['agents']['failed'], 2)
        self.assertEqual(self.result('refused'), [False, False])
        shown = subprocess.run([sys.executable, str(RUNNER), 'status', 'refused', '--agents', '--pretty'],
                               env=self.env, capture_output=True, text=True, timeout=30)
        self.assertEqual(shown.returncode, 0, shown.stderr)
        self.assertIn('  bad  -  ', shown.stdout)

    def test_a_lead_starts_a_run_detached_and_hears_back_once(self):
        plan = self.plan('''
reviews = parallel(["a", "b"], lambda x: agent("say:review " + x, label="review-" + x))
result = [r.text for r in reviews]
''')
        command = f'{sys.executable} {RUNNER} start {plan} --name led'
        self.agent('run', '--new', '--bot', 'lead', '--tools', 'shell', '--', f'fg:{command}')
        self.assertEqual(json.loads(self.model.shell_outputs[-1])['exit_code'], 1)
        self.assertIn('detach_required', self.model.shell_outputs[-1])
        self.agent('run', '--bot', 'lead', '--', f'start:{command}')
        for _ in range(300):
            turns = self.agent('turns', '--bot', 'lead')
            if len(turns) == 3 and turns[-1]['status'] == 'completed':
                break
            time.sleep(.05)
        report = turns[-1]['prompt_preview']
        self.assertTrue(report.startswith('Workflow run led ended: completed.'), report)
        self.assertIn('2 started, 2 completed, 0 failed', report)
        self.assertIn('"review a"', report)
        lead_id = next(b['id'] for b in self.agent('ls') if b['name'] == 'lead')
        workers = [b for b in self.agent('ls') if b['name'].startswith('lead-led.')]
        self.assertEqual({b['created_by_id'] for b in workers}, {lead_id})
        self.assertEqual(workflow.read_summary(self.folder('led'))['lead'], {'bot': 'lead', 'bot_id': lead_id})


if __name__ == '__main__':
    unittest.main()
