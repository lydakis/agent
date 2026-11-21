"""A call its client yields (`yield_call`) becomes `proc:N`, which wait
collects, a steer ends a wait on at once, and wait with terminate or
write_stdin with kill stops. A steer never moves a running call itself."""
import json
import os
import time
import unittest

from tests.test_images import image
from tests.test_runtime import ModelFixture
from tests.test_steps import EXEC

START_TASK = {'name': 'start_task', 'description': 'Start a task.',
           'parameters': {'type': 'object', 'properties': {'name': {'type': 'string'}},
                          'required': ['name'], 'additionalProperties': False},
           'answered_by': 'sonny', 'yields': True}
# A script tool whose client yields a script still running.
YIELDING = dict(EXEC, yields=True)
TOOLS = ['echo', 'shell', 'read', 'wait', 'write_stdin']


class Calls(ModelFixture):
    def call(self, client, request_id, name, arguments, **delivery):
        prompt = 'calls:' + json.dumps([[name, arguments]])
        return client.request('submit', bot='Lead', request_id=request_id, prompt=prompt, **delivery)['result']

    def output_of(self, call_id):
        while True:
            request = self.model.requests.get(timeout=10)
            for item in request['input']:
                if item.get('type') == 'function_call_output' and item['call_id'] == call_id:
                    return item['output']

    def drain(self):
        while not self.model.requests.empty():
            self.model.requests.get_nowait()

    def events(self, client, kind):
        events = client.request('events', bot='Lead', after=0, limit=256)['result']['events']
        return [e['data'] for e in events if e['event'] == kind]


@unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'set AGENT_TEST_RUNTIME=1 after a Rust release build')
class LiftedCalls(Calls):
    """What a call its client yielded does on past its turn."""
    def parked(self):
        """A lead whose answered call waits on its client, parked."""
        client = self.client('echo,wait,write_stdin', settings={'approval_hold_ms': 0})
        self.addCleanup(lambda: client.close(kill=True))
        client.request('create', bot='Lead', workspace=str(self.path), declare=[START_TASK],
                       tools=['echo', 'wait', 'write_stdin'])
        self.model.call_script = [('start_task', {'name': 'fix'})]
        turn = client.request('submit', bot='Lead', request_id='t', prompt='script')['result']['message']
        client.receive(lambda m: m.get('event') == 'turn_waiting' and m.get('turn') == turn)
        return client, turn

    def lifted(self):
        """A lead whose answered call its client yielded: the client, and its handle."""
        client, turn = self.parked()
        reply = client.request('yield_call', bot='Lead', turn=turn, call_id='script-0', output='starting')
        self.assertIn('result', reply, reply)
        lifted = client.receive(lambda m: m.get('event') == 'call_lifted' and m.get('turn') == turn)['data']
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        return client, turn, lifted['handle']

    def test_a_steer_ends_a_wait_on_a_lifted_call_at_once(self):
        client, _, handle = self.lifted()
        waiting = self.call(client, 'w', 'wait', {'handles': [handle]})['message']
        client.receive(lambda m: m.get('event') == 'turn_waiting' and m.get('turn') == waiting)
        self.drain()
        began = time.monotonic()
        client.request('submit', bot='Lead', request_id='s2', prompt='steer:now', delivery='steer')
        result = json.loads(self.output_of('wait-1'))
        self.assertEqual((result['pending'], result['steered']), ([handle], True))
        self.assertLess(time.monotonic() - began, 1.0)

    def test_a_lifted_call_s_end_is_told_and_its_pictures_reach_the_wait(self):
        client, turn, handle = self.lifted()
        answered = client.request('tool_result', bot='Lead', turn=turn, call_id='script-0',
                                  output=[{'type': 'text', 'text': 'started fix'}, dict(image(), type='image')])
        self.assertIn('result', answered, answered)
        client.receive(lambda m: m.get('event') == 'lifted_ended' and m['data']['handle'] == handle)
        told = client.receive(lambda m: m.get('event') == 'queued'
                              and m.get('data', {}).get('origin') == 'background', 10)
        self.assertEqual(client.finished(told['message'])['data']['status'], 'completed')
        prompts = client.request('prompts', bot='Lead', turn=told['message'], bytes=4096)['result']
        self.assertIn(handle, json.loads(prompts['prompts'][0]['text'])['finished'])
        waited = self.call(client, 'w', 'wait', {'handles': [handle]}, delivery='queue')['message']
        self.assertEqual(client.finished(waited)['data']['status'], 'completed')
        output = self.output_of('wait-1')
        self.assertTrue(any(part.get('type') == 'input_image' for part in output), output)

    def test_a_kill_on_a_lifted_call_s_handle_reaches_its_client(self):
        client, turn, handle = self.lifted()
        killed = self.call(client, 'k', 'write_stdin', {'handle': handle, 'kill': True})['message']
        self.assertEqual(client.finished(killed)['data']['status'], 'completed')
        ended = client.receive(lambda m: m.get('event') == 'lifted_ended' and m['data']['handle'] == handle)
        self.assertEqual(ended['data']['outcome'], 'stopped')
        late = client.request('tool_result', bot='Lead', turn=turn, call_id='script-0', output='late')
        self.assertIn('error', late)
        self.assertEqual(client.request('tool_calls', tag='sonny')['result']['calls'], [])


@unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'set AGENT_TEST_RUNTIME=1 after a Rust release build')
class YieldedCalls(Calls):
    def script(self, tool=YIELDING, parked=True):
        """A lead whose script call waits on its client: parked, or still
        live in its hold."""
        client = self.client(','.join(TOOLS), settings={'approval_hold_ms': 0})
        self.addCleanup(lambda: client.close(kill=True))
        created = client.request('create', bot='Lead', workspace=str(self.path), declare=[tool], tools=TOOLS)
        self.assertIn('result', created, created)
        self.model.call_script = [('exec', {'code': 'run()'})]
        turn = client.request('submit', bot='Lead', request_id='t', prompt='script')['result']['message']
        kind = 'turn_waiting' if parked else 'tool_started'
        client.receive(lambda m: m.get('event') == kind and m.get('turn') == turn)
        return client, turn

    def yielded(self, client, turn, output='so far'):
        reply = client.request('yield_call', bot='Lead', turn=turn, call_id='script-0', output=output)
        self.assertIn('result', reply, reply)
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        return reply['result']['handle']

    def ask(self, client, turn, step, arguments):
        """A shell step sent without waiting for it; its id, to receive the reply by."""
        client.next_id += 1
        request = dict(id=client.next_id, op='run_step', bot='Lead', turn=turn, call_id='script-0',
                       step=step, name='shell', arguments=arguments)
        client.process.stdin.write(json.dumps(request) + '\n')
        client.process.stdin.flush()
        return client.next_id

    def running(self, client):
        deadline = time.monotonic() + 5
        while client.request('stats')['result']['steps']['commands'] == 0:
            self.assertLess(time.monotonic(), deadline, 'no step command started')
            time.sleep(.01)

    def test_a_yielded_call_gives_the_model_its_handle_and_the_output_so_far(self):
        client, turn = self.script(parked=False)
        shot = image()
        handle = self.yielded(client, turn, output=[{'type': 'text', 'text': 'built 3 of 9'},
                                                    dict(shot, type='image')])
        output = self.output_of('script-0')
        said = json.loads(output[0]['text'])
        self.assertEqual(said, {'handle': handle, 'background': True, 'yielded': True,
                                'output': 'built 3 of 9'})
        self.assertTrue(any(part.get('type') == 'input_image' for part in output), output)
        [lifted] = self.events(client, 'call_lifted')
        self.assertEqual((lifted['handle'], lifted['waits_for'], lifted['yielded']), (handle, 'answer', True))

    def test_a_parked_call_yields_the_same_way(self):
        client, turn = self.script()
        handle = self.yielded(client, turn)
        said = json.loads(self.output_of('script-0'))
        self.assertEqual(said, {'handle': handle, 'background': True, 'yielded': True, 'output': 'so far'})

    def test_a_yielded_call_s_steps_run_on_and_page_what_earlier_steps_kept(self):
        client, turn = self.script()
        long = client.request('run_step', bot='Lead', turn=turn, call_id='script-0', step='1', name='shell',
                              arguments={'command': 'seq 1 60000'})['result']
        [kept] = json.loads(long['output'])['artifacts']
        slow = self.ask(client, turn, '2', {'command': 'sleep 1; echo on'})
        self.running(client)
        self.yielded(client, turn)
        ran = client.receive(lambda m: m.get('id') == slow, 15)['result']
        self.assertEqual(json.loads(ran['output'])['stdout'], 'on\n')
        read = client.request('run_step', bot='Lead', turn=turn, call_id='script-0', step='3', name='read',
                              arguments={'artifact': kept, 'offset': 30000, 'limit': 1})['result']
        self.assertFalse(read.get('failed'), read)
        self.assertIn('30000', read['output'])

    def test_a_yielded_call_steps_and_is_answered_past_its_tool_s_time(self):
        client, turn = self.script(tool=dict(YIELDING, expire_ms=1000))
        handle = self.yielded(client, turn)
        time.sleep(1.5)
        # Past the tool's own second its steps still run.
        stepped = client.request('run_step', bot='Lead', turn=turn, call_id='script-0', step='1', name='shell',
                                 arguments={'command': 'echo late'})
        self.assertIn('result', stepped, stepped)
        self.assertEqual(json.loads(stepped['result']['output'])['stdout'], 'late\n')
        answered = client.request('tool_result', bot='Lead', turn=turn, call_id='script-0',
                                  output=[{'type': 'text', 'text': 'all 9 built'}, dict(image(), type='image')])
        self.assertIn('result', answered, answered)
        waited = self.call(client, 'w', 'wait', {'handles': [handle]}, delivery='queue')['message']
        self.assertEqual(client.finished(waited)['data']['status'], 'completed')
        output = self.output_of('wait-1')
        self.assertIn('all 9 built', json.dumps(output))
        self.assertTrue(any(part.get('type') == 'input_image' for part in output), output)

    def test_wait_with_terminate_stops_a_yielded_call_and_its_running_step(self):
        client, turn = self.script()
        step = self.ask(client, turn, '1', {'command': 'sleep 30'})
        self.running(client)
        handle = self.yielded(client, turn)
        self.drain()
        waited = self.call(client, 'w', 'wait', {'handles': [handle], 'terminate': True})['message']
        self.assertEqual(client.finished(waited)['data']['status'], 'completed')
        result = json.loads(self.output_of('wait-1'))
        self.assertEqual(result['results'][handle]['error'], 'stopped', result)
        ended = client.receive(lambda m: m.get('event') == 'lifted_ended' and m['data']['handle'] == handle)
        self.assertEqual(ended['data']['outcome'], 'stopped')
        stopped = client.receive(lambda m: m.get('id') == step, 10)
        self.assertTrue(stopped.get('error') or stopped['result'].get('failed'), stopped)
        late = client.request('tool_result', bot='Lead', turn=turn, call_id='script-0', output='late')
        self.assertIn('error', late)

    def test_a_steer_ends_a_wait_on_a_yielded_call_at_once(self):
        client, turn = self.script()
        handle = self.yielded(client, turn)
        waiting = self.call(client, 'w', 'wait', {'handles': [handle]}, delivery='queue')['message']
        client.receive(lambda m: m.get('event') == 'turn_waiting' and m.get('turn') == waiting)
        self.drain()
        began = time.monotonic()
        client.request('submit', bot='Lead', request_id='s', prompt='steer:now', delivery='steer')
        result = json.loads(self.output_of('wait-1'))
        self.assertEqual((result['pending'], result['steered']), ([handle], True))
        self.assertLess(time.monotonic() - began, 1.0)

    def test_wait_with_terminate_kills_a_background_command(self):
        client = self.client(','.join(TOOLS))
        self.addCleanup(lambda: client.close(kill=True))
        client.request('create', bot='Lead', workspace=str(self.path), tools=TOOLS)
        started = self.call(client, 'b', 'shell', {'command': 'sleep 30', 'background': True})['message']
        self.assertEqual(client.finished(started)['data']['status'], 'completed')
        handle = json.loads(self.output_of('shell-1'))['handle']
        began = time.monotonic()
        waited = self.call(client, 'w', 'wait', {'handles': [handle], 'terminate': True}, delivery='queue')['message']
        self.assertEqual(client.finished(waited, timeout=10)['data']['status'], 'completed')
        self.assertLess(time.monotonic() - began, 5)
        result = json.loads(self.output_of('wait-1'))
        self.assertIn(handle, result['results'], result)
        self.assertNotEqual(result['results'][handle].get('exit_code'), 0, result)

    def test_terminate_stops_only_commands(self):
        client, turn = self.script()
        self.yielded(client, turn)
        waited = self.call(client, 'w', 'wait', {'handles': ['Lead/1'], 'terminate': True}, delivery='queue')
        self.assertEqual(client.finished(waited['message'])['data']['status'], 'completed')
        self.assertEqual(json.loads(self.output_of('wait-1'))['error'], 'invalid_handle')

    def test_only_a_waiting_call_of_a_yields_tool_yields(self):
        client, turn = self.script(tool=EXEC)
        refused = client.request('yield_call', bot='Lead', turn=turn, call_id='script-0', output='x')
        self.assertEqual(refused['error'], 'call_not_yieldable', refused)

    def test_a_call_yields_once(self):
        client, turn = self.script()
        self.yielded(client, turn)
        again = client.request('yield_call', bot='Lead', turn=turn, call_id='script-0', output='x')
        self.assertIn('error', again)


if __name__ == '__main__':
    unittest.main()
