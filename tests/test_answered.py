"""Answered declared tools: a call waits for its client's answer, live or parked."""
import json
import os
import time
import unittest

from bench.runtime_client import node_item
from tests.test_runtime import ModelFixture

START = {'name': 'start_task', 'description': 'Start a task.',
         'parameters': {'type': 'object', 'properties': {'name': {'type': 'string'}},
                        'required': ['name'], 'additionalProperties': False},
         'answered_by': 'sonny'}


@unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1', 'set AGENT_TEST_RUNTIME=1 after a Rust release build')
class AnsweredToolTests(ModelFixture):
    def lead(self, settings=None, tool=None, client=None, **create):
        client = client or self.client('echo', settings=settings)
        created = client.request('create', bot='Lead', workspace=str(self.path),
                                 declare=[tool or START], **create)['result']
        self.assertEqual(created['declared'][0]['answered_by'], 'sonny')
        self.model.call_script = [('start_task', {'name': 'fix'})]
        return client

    def submit(self, client):
        return client.request('submit', bot='Lead', request_id='t', prompt='script')['result']['turn']

    def started(self, client, turn):
        return client.receive(lambda m: m.get('event') == 'tool_started' and m.get('turn') == turn)['data']

    def answer(self, client, turn, **answer):
        return client.request('tool_result', bot=answer.pop('bot', 'Lead'), turn=turn,
                              call_id=answer.pop('call_id', 'script-0'), **answer)

    def output(self, client, call_id='script-0'):
        events = client.request('events', bot='Lead', after=0, limit=256)['result']['events']
        done = [e for e in events if e['event'] == 'tool_completed' and e['data']['call_id'] == call_id][-1]
        return node_item(client, 'Lead', done['data']['node'])['result']['output']

    def events(self, client, turn, kind):
        events = client.request('events', bot='Lead', after=0, limit=256)['result']['events']
        return [e['data'] for e in events if e.get('turn') == turn and e['event'] == kind]

    def test_a_declared_tool_names_its_answerer_only_as_a_tag_with_a_sane_expiry(self):
        client = self.client('echo')
        for tool, detail in [(dict(START, result='Started.'), 'result'),
                             (dict(START, answered_by='a b'), 'tag'),
                             (dict(START, answered_by=''), 'empty'),
                             (dict(START, expire_ms=0), 'zero'),
                             (dict(START, expire_ms=3_600_001), 'long'),
                             ({k: v for k, v in START.items() if k != 'answered_by'} | {'expire_ms': 5}, 'alone')]:
            refused = client.request('create', bot='Bad', workspace=str(self.path), declare=[tool])
            self.assertEqual(refused['error'], 'invalid_declare', detail)

    def test_an_answer_within_the_hold_is_the_result_and_the_turn_never_parks(self):
        client = self.lead()
        turn = self.submit(client)
        started = self.started(client, turn)
        self.assertEqual(started['answered_by'], 'sonny')
        self.assertGreater(started['expires_ms'], time.time() * 1000)
        [call] = client.request('tool_calls', tag='sonny')['result']['calls']
        self.assertEqual({k: call[k] for k in ('bot', 'turn', 'call_id', 'name', 'arguments')},
                         {'bot': 'Lead', 'turn': turn, 'call_id': 'script-0', 'name': 'start_task',
                          'arguments': {'name': 'fix'}})
        self.assertEqual(client.request('tool_calls', tag='other')['result'], {'calls': [], 'next_after': None})
        self.assertEqual(self.answer(client, turn, output='a', error={'code': 'x'})['error'], 'invalid_tool_result')
        self.assertEqual(self.answer(client, turn, error={'code': 'Not Ok'})['error'], 'invalid_tool_result')
        self.assertEqual(self.answer(client, turn, output='x' * 65537)['error'], 'tool_output_limit')
        self.assertEqual(self.answer(client, turn, call_id='nope', output='a')['error'], 'no_pending_call')
        self.assertEqual(self.answer(client, turn, bot='Other', output='a')['error'], 'turn_not_found')
        self.assertEqual(self.answer(client, turn, output='started fix')['result'],
                         {'bot': 'Lead', 'turn': turn, 'call_id': 'script-0'})
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        self.assertEqual(self.output(client), 'started fix')
        self.assertEqual(self.events(client, turn, 'turn_waiting'), [])
        self.assertEqual(client.request('tool_calls', tag='sonny')['result']['calls'], [])
        self.assertEqual(self.answer(client, turn, output='again')['error'], 'stale_turn')

    def test_a_slow_answer_parks_the_turn_and_its_error_is_the_result(self):
        client = self.lead(settings={'approval_hold_ms': 0})
        turn = self.submit(client)
        waiting = client.receive(lambda m: m.get('event') == 'turn_waiting' and m.get('turn') == turn)['data']
        self.assertEqual((waiting['call_id'], waiting['answer']), ('script-0', 'sonny'))
        stats = client.request('stats')['result']
        self.assertEqual((stats['active_turns'], stats['waiting_turns']), (0, 1))
        [view] = client.request('turns', bot='Lead', newest=True, limit=1)['result']['turns']
        self.assertEqual(view['waiting_on'], {'call_id': 'script-0', 'answer': 'sonny',
                                              'deadline_ms': waiting['deadline_ms']})
        self.assertEqual(len(client.request('tool_calls', tag='sonny')['result']['calls']), 1)
        answered = self.answer(client, turn, error={'code': 'task_exists', 'detail': 'fix exists'})
        self.assertIn('result', answered)
        self.assertEqual(self.answer(client, turn, output='twice')['error'], 'call_already_answered')
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        self.assertEqual(json.loads(self.output(client)), {'error': 'task_exists', 'detail': 'fix exists'})
        self.assertEqual(len(self.events(client, turn, 'turn_resumed')), 1)

    def test_a_call_nobody_answers_in_time_is_unserved_and_the_turn_goes_on(self):
        # Out of time within the hold, and while parked.
        for expire_ms, parks in ((100, False), (600, True)):
            with self.subTest(expire_ms=expire_ms):
                self.setUp()
                client = self.lead(tool=dict(START, expire_ms=expire_ms))
                turn = self.submit(client)
                self.assertEqual(client.finished(turn)['data']['status'], 'completed')
                output = json.loads(self.output(client))
                self.assertEqual(output['error'], 'tool_unserved')
                self.assertEqual(bool(self.events(client, turn, 'turn_waiting')), parks)
                self.assertEqual(self.answer(client, turn, output='late')['error'], 'stale_turn')

    def test_a_parked_call_survives_restart_and_interrupt_ends_it(self):
        client = self.lead(settings={'approval_hold_ms': 0})
        turn = self.submit(client)
        client.receive(lambda m: m.get('event') == 'turn_waiting' and m.get('turn') == turn)
        client.close(kill=True)
        client = self.client('echo')
        self.assertEqual(len(client.request('tool_calls', tag='sonny')['result']['calls']), 1)
        self.answer(client, turn, output='after restart')
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        self.assertEqual(self.output(client), 'after restart')
        # A second turn parks, and an interrupt ends it with the call.
        self.model.call_script = [None, ('start_task', {'name': 'next'})]
        turn = client.request('submit', bot='Lead', request_id='u', prompt='script')['result']['turn']
        client.receive(lambda m: m.get('event') == 'turn_waiting' and m.get('turn') == turn)
        self.assertEqual(client.request('interrupt', bot='Lead', turn=turn)['result']['status'], 'interrupted')
        self.assertEqual(client.request('tool_calls', tag='sonny')['result']['calls'], [])
        self.assertEqual(self.answer(client, turn, call_id='script-1', output='late')['error'], 'stale_turn')

    def test_a_gate_holds_back_a_declared_tool_before_its_answer(self):
        client = self.client('echo')
        refused = client.request('create', bot='Bad', workspace=str(self.path), declare=[START],
                                 approve=['react'], approver='manual')
        self.assertEqual(refused['error'], 'approve_not_in_tools')
        client = self.lead(client=client, approve=['start_task'], approver='manual')
        turn = self.submit(client)
        [call] = client.receive(lambda m: m.get('event') == 'approval_requested'
                                and m.get('turn') == turn)['data']['calls']
        self.assertEqual(call['name'], 'start_task')
        # Not waiting for an answer until it is allowed and starts.
        self.assertEqual(client.request('tool_calls', tag='sonny')['result']['calls'], [])
        client.request('answer', bot='Lead', turn=turn, call_id='script-0', request=1, decision='allow', by='test')
        self.assertEqual(self.started(client, turn)['answered_by'], 'sonny')
        self.answer(client, turn, output='allowed and started')
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        self.assertEqual(self.output(client), 'allowed and started')

    def test_a_later_calls_gate_that_lapses_ends_the_wait_for_an_answer(self):
        client = self.client('echo,shell')
        self.lead(client=client, tool=dict(START, expire_ms=30_000), approve=['shell'],
                  approver='manual', approve_expire_ms=100)
        self.model.call_script = [[('start_task', {'name': 'fix'}), ('shell', {'command': 'true'})]]
        started = time.monotonic()
        turn = self.submit(client)
        # Ended by the lapse, as a turn waiting on a verdict is, long before
        # the answered call's own time runs out.
        finished = client.finished(turn)['data']
        self.assertEqual((finished['status'], finished['error']), ('interrupted', 'approval_expired'))
        self.assertLess(time.monotonic() - started, 5)
        self.assertEqual(json.loads(self.output(client)),
                         {'error': 'tool_unserved', 'detail': "a later call's gate lapsed before the answer"})

    def test_configure_may_gate_a_tool_it_declares_at_once(self):
        client = self.client('echo')
        client.request('create', bot='Lead', workspace=str(self.path))
        configured = client.request('configure', bot='Lead', declare=[START],
                                    approve=['start_task'], approver='manual')['result']
        self.assertEqual(sorted(configured['changed']), ['declared', 'gates'])

    def test_a_call_that_does_not_fit_is_refused_and_waits_for_nothing(self):
        client = self.lead()
        self.model.call_script = [('start_task', {'name': 1})]
        turn = self.submit(client)
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        self.assertEqual(json.loads(self.output(client))['error'], 'invalid_arguments')
        self.assertNotIn('answered_by', self.events(client, turn, 'tool_started')[0])


if __name__ == '__main__':
    unittest.main()
