"""The resume evaluation must halt every bot inside its long step, continue
it, and score from the step log and the answer, before any paid run."""
import http.server
import json
import os
import re
import unittest
from unittest.mock import patch

from bench import resume_eval
from bench.resume_eval import run_condition
from bench.targets import clean_env
from tests.test_runtime import ModelFixture


class Stepper(http.server.BaseHTTPRequestHandler):
    """A scripted agent: runs the next step whose code it lacks, and on a
    halt's answer reruns the cut step, or with `server.stops` ends its turn
    there, as the rehearsal's models did."""
    protocol_version = 'HTTP/1.1'

    def log_message(self, *_):
        pass

    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        outputs = [i['output'] for i in request['input'] if i.get('type') == 'function_call_output']
        calls = sum(i.get('type') == 'function_call' for i in request['input'])
        codes = re.findall(r'code: (\w{6})', ' '.join(outputs))
        halted = bool(outputs) and 'daemon_' in outputs[-1]
        if halted and self.server.stops:
            text, output = 'The pause stopped step 3.', None
        elif len(codes) < resume_eval.STEPS:
            text = ''
            output = [{'type': 'function_call', 'name': 'shell', 'call_id': f's{calls}',
                       'arguments': json.dumps({'command': f'./step {len(codes) + 1}', 'timeout_ms': 600_000})}]
        else:
            text, output = ' '.join(codes), None
        output = output or [{'type': 'message', 'role': 'assistant',
                             'content': [{'type': 'output_text', 'text': text}]}]
        events = [{'type': 'response.created', 'response': {'id': 'r'}},
                  *([{'type': 'response.output_text.delta', 'delta': text}] if text else []),
                  {'type': 'response.completed', 'response': {'status': 'completed', 'output': output, 'usage': {
                      'input_tokens': 1, 'output_tokens': 1, 'input_tokens_details': {'cached_tokens': 0}}}}]
        body = b''.join(('data: ' + json.dumps(e) + '\n\n').encode() for e in events)
        self.send_response(200)
        self.send_header('Content-Type', 'text/event-stream')
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)


@unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'requires release binary')
class ResumeRunnerTests(ModelFixture):
    handler = Stepper

    def run_one(self, condition, stops):
        self.model.stops = stops
        spec = ('openai', 'responses', self.url, None)
        with patch.object(resume_eval, 'QUICK', 0):
            block = run_condition(self.binary, spec, 'synthetic-model', condition, 1, self.path, clean_env(),
                                  11, hold=1, timeout=60)
        return block['bots'][f'{condition}-0']

    def test_a_bot_that_reruns_the_cut_step_passes_after_a_pause_and_a_restart(self):
        for condition in ('pause', 'shutdown'):
            with self.subTest(condition):
                result = self.run_one(condition, stops=False)
                self.assertEqual(result['status'], 'completed', result)
                self.assertTrue(result['passed'] and not result['stopped'], result)
                self.assertEqual(result['step_runs'], {1: 1, 2: 1, 3: 2, 4: 1, 5: 1, 6: 1})

    def test_a_bot_that_ends_its_turn_on_the_halt_is_counted_stopped(self):
        result = self.run_one('pause', stops=True)
        self.assertTrue(result['stopped'] and not result['passed'], result)
        self.assertEqual(result['steps_done'], [1, 2])
