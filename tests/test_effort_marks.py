"""A turn at another effort than its bot's, on a model that takes a change
of effort inside the conversation, keeps the bot's effort on the request
and says the change where the turn starts, so the cache holds."""
import os
import unittest

from bench.runtime_client import Client
from bench.targets import clean_env
from tests import test_runtime
from tests.test_runtime import AnthropicModel, Model, ModelFixture, is_summary

RUNTIME = unittest.skipUnless(os.environ.get('AGENT_TEST_RUNTIME') == '1',
                              'set AGENT_TEST_RUNTIME=1 after a Rust release build')


class Turns:
    def turn(self, client, request_id, **effort):
        turn = client.request('submit', bot='Bob', request_id=request_id, prompt=request_id,
                              **effort)['result']['message']
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        return self.model.requests.get(timeout=3)


@RUNTIME
class ClaudeEffortMarkTests(Turns, ModelFixture):
    handler = AnthropicModel

    def anthropic(self, model):
        client = Client(self.binary, self.path / 'state.sqlite', self.url,
                        tools='echo,shell', provider='anthropic', family='anthropic',
                        model=model, key_env='ANTHROPIC_TEST_KEY',
                        env={**clean_env(), 'ANTHROPIC_TEST_KEY': 'synthetic-anthropic-key'})
        self.addCleanup(client.close)
        self.assertIn('result', client.request('create', bot='Bob', workspace=str(self.path), effort='high'))
        return client

    def test_a_boosted_turn_keeps_the_bots_effort_and_marks_where_it_starts(self):
        client = self.anthropic('claude-opus-5-5')
        plain = self.turn(client, 'one')
        self.assertEqual(plain['output_config'], {'effort': 'high'})
        self.assertNotIn('system', [m['role'] for m in plain['messages']])
        boosted = self.turn(client, 'two', effort='max')
        self.assertEqual(boosted['output_config'], {'effort': 'high'})
        messages = boosted['messages']
        self.assertEqual(messages[-2], {'role': 'system', 'content': [], 'output_config': {'effort': 'max'}})
        self.assertEqual(messages[-1]['role'], 'user')
        # What the plain turn sent is still the boosted one's start.
        self.assertEqual(messages[:len(plain['messages'])], plain['messages'])
        back = self.turn(client, 'three')
        self.assertEqual(back['output_config'], {'effort': 'high'})
        self.assertEqual([m['output_config']['effort'] for m in back['messages'] if m['role'] == 'system'],
                         ['max', 'high'])
        self.assertEqual(back['messages'][-2]['role'], 'system')
        # The boosted turn's request is the start of the next one's: the
        # change stays where it was said.
        self.assertEqual(back['messages'][:len(messages)], messages)

    def test_a_fork_keeps_the_marks_of_the_turns_it_shares_after_its_source_goes(self):
        client = self.anthropic('claude-opus-5-5')
        self.turn(client, 'one', effort='max')
        checkpoint = client.request('inspect', bot='Bob')['result']['head']
        self.assertIn('result', client.request('fork', source='Bob', checkpoint=checkpoint, bot='Fork',
                                               workspace=str(self.path)))
        self.assertIn('result', client.request('delete', bot='Bob'))
        turn = client.request('submit', bot='Fork', request_id='two', prompt='two')['result']['message']
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        request = self.model.requests.get(timeout=3)
        self.assertEqual(request['output_config'], {'effort': 'high'})
        self.assertEqual([m['output_config']['effort'] for m in request['messages'] if m['role'] == 'system'],
                         ['max', 'high'])

    def test_a_model_without_it_runs_the_turn_at_its_own_effort(self):
        client = self.anthropic('synthetic-claude')
        self.turn(client, 'one')
        boosted = self.turn(client, 'two', effort='max')
        self.assertEqual(boosted['output_config'], {'effort': 'max'})
        back = self.turn(client, 'three')
        self.assertEqual(back['output_config'], {'effort': 'high'})
        self.assertNotIn('system', [m['role'] for m in back['messages']])


@RUNTIME
class ResponsesEffortMarkTests(Turns, ModelFixture):
    handler = Model

    def responses(self, model, settings=None):
        self.model.models = (model,)
        client = Client(self.binary, self.path / 'state.sqlite', self.url, model=model, settings=settings)
        self.addCleanup(client.close)
        self.assertIn('result', client.request('create', bot='Bob', workspace=str(self.path), effort='high'))
        return client

    def test_a_boosted_turn_keeps_the_bots_effort_and_updates_the_configuration(self):
        client = self.responses('gpt-6-sol')
        plain = self.turn(client, 'one')
        self.assertEqual(plain['reasoning']['effort'], 'high')
        boosted = self.turn(client, 'two', effort='xhigh')
        self.assertEqual(boosted['reasoning']['effort'], 'high')
        self.assertEqual(boosted['input'][-2], {'type': 'configuration_update', 'reasoning': {'effort': 'xhigh'}})
        self.assertEqual(boosted['input'][-1]['role'], 'user')
        self.assertEqual(boosted['input'][:len(plain['input'])], plain['input'])
        back = self.turn(client, 'three')
        self.assertEqual(back['reasoning']['effort'], 'high')
        self.assertEqual([i['reasoning']['effort'] for i in back['input'] if i.get('type') == 'configuration_update'],
                         ['xhigh', 'high'])
        self.assertEqual(back['input'][:len(boosted['input'])], boosted['input'])

    def test_a_window_keeps_room_for_its_marks_within_the_item_limit(self):
        client = self.responses('gpt-6-sol', settings={'context_items': 6})
        for n in range(8):
            turn = client.request('submit', bot='Bob', request_id=f't{n}', prompt=f'turn {n}',
                                  **({'effort': 'xhigh'} if n % 2 else {}))['result']['message']
            self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        requests = []
        while not self.model.requests.empty():
            requests.append(self.model.requests.get_nowait())
        work = [r for r in requests if not is_summary(r)]
        self.assertTrue(any(i.get('type') == 'configuration_update' for r in work for i in r['input']))
        self.assertTrue(all(len(r['input']) <= 6 for r in work), [len(r['input']) for r in work])

    def test_an_earlier_model_runs_the_turn_at_its_own_effort(self):
        client = self.responses('synthetic-model')
        self.turn(client, 'one')
        boosted = self.turn(client, 'two', effort='xhigh')
        self.assertEqual(boosted['reasoning']['effort'], 'xhigh')
        self.assertFalse([i for i in boosted['input'] if i.get('type') == 'configuration_update'])


@RUNTIME
class SwitchedEffortMarkTests(unittest.TestCase):
    start = test_runtime.SwitchFamilyTests.start
    run_turn = test_runtime.SwitchFamilyTests.run_turn

    def test_a_level_the_new_family_does_not_take_runs_at_the_bots_own(self):
        client, openai, anthropic, path = self.start()
        openai.models = ('gpt-6-sol',)
        client.request('create', bot='Bob', workspace=str(path), model='anthropic/claude-opus-5-5', effort='high')
        turn = client.request('submit', bot='Bob', request_id='1', prompt='one', effort='max')['result']['message']
        self.assertEqual(client.finished(turn)['data']['status'], 'completed')
        self.run_turn(client, '2', 'two')
        self.assertIn('result', client.request('configure', bot='Bob', model='openai/gpt-6-sol', effort='high'))
        self.run_turn(client, '3', 'three')
        request = openai.requests.get(timeout=3)
        self.assertEqual(request['reasoning']['effort'], 'high')
        self.assertFalse([i for i in request['input'] if i.get('type') == 'configuration_update'])


if __name__ == '__main__':
    unittest.main()
